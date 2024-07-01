// todo! Use Arc<str> where helpful instead of String
// todo! Cache every model request based on a digest of input and model name
// todo! Estimate time remaining when progress reporting
// todo! Add unit/integration tests

mod database;
mod huggingface;
mod language_model;
mod ollama;

pub(crate) use database::*;
use huggingface::HuggingFaceClient;
pub(crate) use language_model::*;

use anyhow::{anyhow, Result};
use clap::Parser;
use fs::{Fs, RealFs};
use futures::{channel::mpsc, lock::Mutex, stream, Stream, StreamExt};
use git::GitHostingProviderRegistry;
use gpui::{App, BackgroundExecutor};
use ignore::gitignore::GitignoreBuilder;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    pin::Pin,
    process,
    sync::{atomic::AtomicUsize, Arc},
    time::SystemTime,
};
use tokenizers::{tokenizer::Tokenizer, FromPretrainedParameters};
use tree_sitter::{Node, Tree};

const CHUNK_SIZE: usize = 5000;
const HUGGINGFACE_ENDPOINT_URL: &str =
    "https://eezviumpj7crpq2t.us-east-1.aws.endpoints.huggingface.cloud";

#[derive(Parser)]
#[command(name = "Project Summarizer")]
#[command(author = "Your Name")]
#[command(version = "1.0")]
#[command(about = "Summarizes a project directory", long_about = None)]
struct Cli {
    /// The path to the project directory
    project_path: PathBuf,

    /// The path to the database
    #[arg(short = 'd', long = "db-path")]
    db_path: Option<PathBuf>,

    /// Number of worker threads
    #[arg(short = 'w', long = "workers", default_value = "8")]
    num_workers: usize,

    /// Path to read summaries from
    #[arg(long)]
    read: Option<PathBuf>,

    /// Export the database contents to stdout
    #[arg(long)]
    export: bool,
}

fn main() {
    let cli = Cli::parse();

    let project_path = cli.project_path;
    if !project_path.exists() || !project_path.is_dir() {
        eprintln!("Error: The provided project path does not exist or is not a directory.");
        std::process::exit(1);
    }

    let db_path = cli
        .db_path
        .unwrap_or_else(|| std::env::current_dir().unwrap().join("project_summaries"));

    println!("Summarizing project at: {}", project_path.display());
    println!("Using database at: {}", db_path.display());
    println!("Number of workers: {}", cli.num_workers);

    App::new().run(move |cx| {
        let mine = cx.spawn(|cx| async move {
            let fs = Arc::new(RealFs::new(
                Arc::new(GitHostingProviderRegistry::new()),
                None,
            )) as Arc<dyn Fs>;

            let language_model = Arc::new(HuggingFaceClient::new(
                HUGGINGFACE_ENDPOINT_URL.to_string(),
                std::env::var("HUGGINGFACE_API_KEY").expect("HUGGINGFACE_API_KEY not set"),
                cx.background_executor().clone(),
            )) as Arc<dyn LanguageModel>;

            let miner = Miner::new(
                db_path,
                project_path.to_path_buf(),
                cli.num_workers,
                fs,
                cx.background_executor().clone(),
                language_model,
            )
            .await?;

            if cli.export {
                miner.export_database().await?;
                return Ok(());
            }

            miner.summarize_project().await?;

            println!("Finished summarization");

            if let Some(read_path) = cli.read {
                let full_path = project_path.join(&read_path);
                if let Some(summary) = miner.summary_for_path(&full_path).await? {
                    println!("<path>{}</path>", full_path.to_string_lossy());
                    println!("<summary>{}</summary>", summary);
                    println!();
                }
            }

            anyhow::Ok(())
        });

        cx.spawn(|_cx| async move {
            match mine.await {
                Ok(()) => process::exit(0),
                Err(error) => {
                    eprintln!("error: {:?}", error);
                    process::exit(1);
                }
            }
        })
        .detach();
    });
}

pub struct Miner {
    root: PathBuf,
    num_workers: usize,
    database: Database,
    tokenizer: Tokenizer,
    language_model: Arc<dyn LanguageModel>,
    queue: Arc<Mutex<VecDeque<Entry>>>,
    summaries: Arc<Mutex<BTreeMap<PathBuf, String>>>,
    paths_loaded_from_cache: Arc<Mutex<BTreeMap<PathBuf, bool>>>,
    outstanding_chunks: Arc<AtomicUsize>,
    outstanding_symbols: Arc<AtomicUsize>,
    total_chunks: Arc<AtomicUsize>,
    total_symbols: Arc<AtomicUsize>,
    processed_chunks: Arc<Mutex<HashMap<(PathBuf, usize), bool>>>,
    processed_files: Arc<Mutex<HashSet<PathBuf>>>,
    fs: Arc<dyn Fs>,
    background_executor: BackgroundExecutor,
}

impl Miner {
    pub async fn new(
        db_path: PathBuf,
        root: PathBuf,
        num_workers: usize,
        fs: Arc<dyn Fs>,
        background_executor: BackgroundExecutor,
        language_model: Arc<dyn LanguageModel>,
    ) -> Result<Arc<Self>> {
        let database = Database::new(&db_path, &root, &background_executor).await?;

        let tokenizer = Tokenizer::from_pretrained(
            "Qwen/Qwen2-7B-Instruct",
            Some(FromPretrainedParameters {
                revision: "main".into(),
                user_agent: HashMap::default(),
                auth_token: Some(
                    std::env::var("HUGGINGFACE_API_KEY").expect("HUGGINGFACE_API_KEY not set"),
                ),
            }),
        )
        .unwrap();

        let queue = Arc::new(Mutex::new(VecDeque::new()));

        let summaries = Arc::new(Mutex::new(BTreeMap::new()));
        let paths_loaded_from_cache = Arc::new(Mutex::new(BTreeMap::new()));

        let outstanding_chunks = Arc::new(AtomicUsize::new(0));
        let outstanding_symbols = Arc::new(AtomicUsize::new(0));
        let total_chunks = Arc::new(AtomicUsize::new(0));
        let total_symbols = Arc::new(AtomicUsize::new(0));

        let processed_chunks = Arc::new(Mutex::new(HashMap::new()));

        let miner = Arc::new(Self {
            root,
            num_workers,
            database,
            tokenizer,
            language_model,
            queue,
            summaries,
            paths_loaded_from_cache,
            outstanding_chunks,
            outstanding_symbols,
            total_chunks,
            total_symbols,
            processed_chunks,
            processed_files: Arc::new(Mutex::new(HashSet::new())),
            fs,
            background_executor,
        });

        Ok(miner)
    }

    /// Summarizes a project by processing files and directories, generating summaries,
    /// and storing them in a database. This method coordinates the entire summarization
    /// process, including worker thread management and progress tracking.
    pub async fn summarize_project(self: &Arc<Self>) -> Result<()> {
        self.reset().await;

        println!("Starting project summarization");
        // Populate the queue with files and directories
        let mut entries = self.walk_directory(&self.root).await?;

        while let Some(entry) = entries.next().await {
            let entry = entry?;
            let path = entry.path.clone();

            if entry.metadata.is_dir {
                println!("Enqueueing directory: {:?}", path);
                let mut contents = Vec::new();
                let mut read_dir = self.fs.read_dir(&path).await?;
                while let Some(child_entry) = read_dir.next().await {
                    let child_entry = child_entry?;
                    if !self.is_ignored(&child_entry).await? {
                        contents.push(child_entry);
                    }
                }
                if !contents.is_empty() {
                    self.queue
                        .lock()
                        .await
                        .push_back(Entry::Directory(path.clone(), contents));
                }
            } else {
                println!("Enqueueing file: {:?}", path);
                self.queue.lock().await.push_back(Entry::File(path.clone()));
            }
        }

        println!("Initial queue population complete");

        let workers: Vec<_> = (0..self.num_workers)
            .map(|worker_id| {
                let this = self.clone();
                self.background_executor.spawn(async move {
                    println!("Worker {} starting", worker_id);
                    let result = this.worker().await;
                    println!("Worker {} finished", worker_id);
                    result
                })
            })
            .collect();

        for (worker_id, worker) in workers.into_iter().enumerate() {
            worker.await?;
            println!("Worker {} completed successfully", worker_id);
        }

        println!("All workers have completed");

        // Remove deleted entries from the database
        println!("Removing deleted entries from the database");

        // Read all keys from the database
        let keys: Vec<String> = self
            .database
            .transact(|db, txn| db.iter(&txn)?.map(|item| Ok(item?.0.to_string())).collect())
            .await?;

        // Filter keys that no longer exist
        let mut keys_to_delete = Vec::new();
        for key in keys {
            if let Some((_, path_str)) = key.split_once(':') {
                let path = PathBuf::from(path_str.split("::").next().unwrap_or(path_str));
                if !self.fs.is_file(&path).await && !self.fs.is_dir(&path).await {
                    println!("Marking for deletion: {:?}", key);
                    keys_to_delete.push(key);
                }
            }
        }

        // Delete filtered keys from the database
        self.database
            .transact(|db, mut txn| {
                for key in keys_to_delete {
                    println!("Deleting from database: {:?}", key);
                    db.delete(&mut txn, &key)?;
                }
                Ok(())
            })
            .await?;

        println!("Project summarization completed successfully");
        Ok(())
    }

    async fn reset(self: &Arc<Self>) {
        self.queue.lock().await.clear();
        self.summaries.lock().await.clear();
        self.paths_loaded_from_cache.lock().await.clear();
        self.outstanding_chunks
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.outstanding_symbols
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.total_chunks
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.total_symbols
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.processed_chunks.lock().await.clear();
        self.processed_files.lock().await.clear();
    }

    async fn is_ignored(&self, path: &Path) -> Result<bool> {
        let gitignore = self.build_gitignore(&self.root).await?;
        let relative_path = path.strip_prefix(&self.root).unwrap_or(path);
        let is_dir = self.fs.is_dir(path).await;
        if path.file_name().map_or(false, |name| name == ".git") {
            return Ok(true);
        }
        Ok(gitignore.matched(relative_path, is_dir).is_ignore())
    }

    async fn walk_directory(
        &self,
        root: &Path,
    ) -> Result<Pin<Box<dyn Send + Stream<Item = Result<DirEntry>>>>> {
        let fs = self.fs.clone();
        let root = root.to_owned();
        let gitignore = self.build_gitignore(&root).await?;

        let stream = stream::unfold(
            (vec![root.clone()], fs, gitignore),
            move |(mut stack, fs, gitignore)| {
                let root = root.clone();
                async move {
                    while let Some(path) = stack.pop() {
                        if path.file_name().map_or(false, |name| name == ".git") {
                            continue;
                        }

                        let relative_path = path.strip_prefix(&root).unwrap_or(&path);
                        if gitignore
                            .matched(relative_path, fs.is_dir(&path).await)
                            .is_ignore()
                        {
                            continue;
                        }

                        match fs.metadata(&path).await {
                            Ok(Some(metadata)) => {
                                let entry = DirEntry {
                                    path: path.clone(),
                                    metadata,
                                };

                                if metadata.is_dir {
                                    if let Ok(mut read_dir) = fs.read_dir(&path).await {
                                        while let Some(Ok(child)) = read_dir.next().await {
                                            stack.push(child);
                                        }
                                    }
                                }

                                return Some((Ok(entry), (stack, fs, gitignore)));
                            }
                            Ok(None) => {
                                return Some((
                                    Err(anyhow!("No metadata available for {:?}", path)),
                                    (stack, fs, gitignore),
                                ))
                            }
                            Err(e) => {
                                return Some((Err(anyhow::Error::from(e)), (stack, fs, gitignore)))
                            }
                        }
                    }
                    None
                }
            },
        );

        Ok(Box::pin(stream))
    }

    /// Processes entries from the queue, handling files, directories, chunks,
    /// and Rust symbols. This method runs in a loop until the queue is empty,
    /// coordinating the summarization of project components.
    async fn worker(self: &Arc<Self>) -> Result<()> {
        loop {
            let entry = {
                let mut queue_lock = self.queue.lock().await;
                queue_lock.pop_front()
            };

            match entry {
                Some(Entry::File(path)) => {
                    println!("Worker processing file: {:?}", path);
                    let content = self.fs.load(&path).await.unwrap_or_default();
                    if let Err(e) = self.scan_file(path.clone(), content).await {
                        eprintln!("Error processing file {:?}: {}", path, e);
                    }
                }
                Some(Entry::Directory(path, contents)) => {
                    println!("Worker processing directory: {:?}", path);
                    if let Err(e) = self.process_directory(path.clone(), contents).await {
                        eprintln!("Error processing directory {:?}: {}", path, e);
                    }
                }
                Some(Entry::Chunk(path, content, index)) => {
                    println!("Worker processing chunk {} of file {:?}", index, path);
                    if let Err(e) = self.process_chunk(path.clone(), content, index).await {
                        eprintln!("Error processing chunk {} of file {:?}: {}", index, path, e);
                    }
                }
                Some(Entry::RustSymbol(path, name, content, parsed_file)) => {
                    println!("Worker processing Rust symbol {} in file {:?}", name, path);
                    if let Err(e) = self
                        .process_rust_symbol(path.clone(), name.clone(), content, parsed_file)
                        .await
                    {
                        eprintln!(
                            "Error processing Rust symbol {} in file {:?}: {}",
                            name, path, e
                        );
                    }
                }
                None => {
                    println!("Worker queue empty, exiting");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Processes a directory by summarizing its contents and combining summaries.
    /// If any entries are not yet summarized, it re-enqueues the directory for later processing.
    /// The combined summary is stored in the database upon completion.
    async fn process_directory(&self, path: PathBuf, contents: Vec<PathBuf>) -> Result<()> {
        println!("Processing directory: {:?}", path);

        let mut summaries = Vec::new();
        let mut pending_entries = Vec::new();

        for entry_path in contents {
            let key = format!("path:{}", entry_path.to_string_lossy());
            match self
                .database
                .transact(move |db, txn| Ok(db.get(&txn, &key)?))
                .await?
            {
                Some(cached_summary) => {
                    summaries.push(cached_summary.summary);
                }
                None => {
                    pending_entries.push(entry_path);
                }
            }
        }

        if !pending_entries.is_empty() {
            // Re-enqueue the directory with remaining entries
            self.queue
                .lock()
                .await
                .push_back(Entry::Directory(path, pending_entries));
            return Ok(());
        }

        // All entries are summarized, combine them
        let combined_summary = self.combine_summaries(&summaries).await?;

        // Save the combined summary for the directory
        let key = format!("path:{}", path.to_string_lossy());
        let metadata = self.fs.metadata(&path).await?.ok_or_else(|| {
            anyhow!(
                "Failed to get metadata because path does not exist: {:?}",
                path
            )
        })?;
        let mtime = metadata.mtime;
        let cached_summary = CachedSummary {
            summary: combined_summary,
            mtime,
        };
        self.database
            .transact(move |db, mut txn| {
                db.put(&mut txn, &key, &cached_summary)?;
                Ok(())
            })
            .await?;

        println!("Finished processing and summarizing directory: {:?}", path);
        Ok(())
    }

    /// Combines multiple summaries into a single summary.
    ///
    /// This method takes a slice of summary strings and combines them into a single
    /// coherent summary. Currently, it uses a simple concatenation approach, but
    /// future implementations may use more sophisticated techniques, such as
    /// leveraging an AI model to generate a summary of summaries.
    ///
    /// # Arguments
    ///
    /// * `summaries` - A slice of strings, each representing a summary to be combined.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing the combined summary as a `String`, or an error
    /// if the combination process fails.
    async fn combine_summaries(&self, summaries: &[String]) -> Result<String> {
        if summaries.is_empty() {
            return Ok(String::new());
        }

        if summaries.len() == 1 {
            return Ok(summaries[0].clone());
        }

        let chunk_size = CHUNK_SIZE;
        let mut chunked_summaries = Vec::new();
        let mut current_chunk = String::new();

        for summary in summaries {
            if self.count_tokens(&(current_chunk.clone() + summary)) > chunk_size {
                if !current_chunk.is_empty() {
                    chunked_summaries.push(current_chunk);
                }
                current_chunk = summary.clone();
            } else {
                if !current_chunk.is_empty() {
                    current_chunk.push_str("\n\n");
                }
                current_chunk.push_str(summary);
            }
        }

        if !current_chunk.is_empty() {
            chunked_summaries.push(current_chunk);
        }

        let mut final_summary = String::new();

        for (index, chunk) in chunked_summaries.iter().enumerate() {
            let chunk_prompt = format!(
                "Create a concise summary for part {} of {}:\n\n{}",
                index + 1,
                chunked_summaries.len(),
                chunk
            );

            let messages = vec![Message {
                role: "user".to_string(),
                content: chunk_prompt,
            }];

            let mut receiver = self.stream_completion(messages).await?;
            let mut chunk_summary = String::new();

            while let Some(content) = receiver.next().await {
                chunk_summary.push_str(&content);
            }

            if !chunk_summary.is_empty() {
                if !final_summary.is_empty() {
                    final_summary.push_str("\n\n");
                }
                final_summary.push_str(&chunk_summary);
            }
        }

        if chunked_summaries.len() > 1 {
            let final_prompt = format!(
                "Create a final concise summary that combines the following {} summaries:\n\n{}",
                chunked_summaries.len(),
                final_summary
            );

            let messages = vec![Message {
                role: "user".to_string(),
                content: final_prompt,
            }];

            let mut receiver = self.stream_completion(messages).await?;
            let mut combined_summary = String::new();

            while let Some(content) = receiver.next().await {
                combined_summary.push_str(&content);
            }

            final_summary = combined_summary;
        }

        if final_summary.is_empty() {
            println!("Warning: Language model returned an empty combined summary");
            Ok(format!(
                "Summary: Combined summary of {} items",
                summaries.len()
            ))
        } else {
            Ok(final_summary)
        }
    }

    /// Streams the completion of a language model request.
    ///
    /// This method sends the given messages to the language model and returns
    /// a receiver that can be used to stream the generated response.
    ///
    /// # Arguments
    ///
    /// * `messages` - A vector of `Message` structs representing the conversation history.
    ///
    /// # Returns
    ///
    /// A `Result` containing a `mpsc::Receiver<String>` that yields chunks of the generated response.
    async fn stream_completion(
        &self,
        mut messages: Vec<Message>,
    ) -> Result<mpsc::Receiver<String>> {
        self.truncate_messages(&mut messages);
        self.language_model.stream_completion(messages).await
    }

    /// Truncates the given list of messages to fit within the maximum token limit.
    ///
    /// This method starts by truncating the last message and removes empty messages.
    /// It repeats this process until the total token count of all messages fits
    /// within the specified limit.
    ///
    /// # Arguments
    ///
    /// * `messages` - A mutable vector of Message structs to be truncated.
    ///
    /// # Returns
    ///
    /// Returns the total number of tokens in the truncated messages.
    fn truncate_messages(&self, messages: &mut Vec<Message>) -> usize {
        let mut total_tokens = messages.iter().map(|m| self.count_tokens(&m.content)).sum();

        while total_tokens > CHUNK_SIZE && !messages.is_empty() {
            if let Some(last_message) = messages.last_mut() {
                let tokens_to_remove = total_tokens - CHUNK_SIZE;
                let message_tokens = self.count_tokens(&last_message.content);

                if message_tokens <= tokens_to_remove {
                    total_tokens -= message_tokens;
                    messages.pop();
                } else {
                    let truncated_content = self
                        .truncate_string(&last_message.content, message_tokens - tokens_to_remove);
                    total_tokens -= tokens_to_remove;
                    last_message.content = truncated_content;
                }
            }
        }

        total_tokens
    }

    /// Truncates a string to the specified number of tokens.
    ///
    /// # Arguments
    ///
    /// * `s` - The string to truncate.
    /// * `max_tokens` - The maximum number of tokens to keep.
    ///
    /// # Returns
    ///
    /// Returns the truncated string.
    fn truncate_string(&self, s: &str, max_tokens: usize) -> String {
        let encoded = self.tokenizer.encode(s, false).unwrap();
        let truncated_tokens = encoded
            .get_ids()
            .iter()
            .take(max_tokens)
            .cloned()
            .collect::<Vec<_>>();
        self.tokenizer.decode(&truncated_tokens, true).unwrap()
    }

    /// Scans a file, processes its content, and generates a summary.
    ///
    /// This method handles file processing by either parsing Rust symbols,
    /// splitting the file into chunks, or summarizing it directly based on its size.
    /// It also manages caching of summaries and updates progress indicators.
    async fn scan_file(&self, path: PathBuf, content: String) -> Result<()> {
        println!("Scanning file: {:?}", path);

        // Check if the file has already been processed
        let mut processed_files = self.processed_files.lock().await;
        if processed_files.contains(&path) {
            println!("File already processed: {:?}", path);
            return Ok(());
        }
        processed_files.insert(path.clone());
        drop(processed_files);

        let metadata = self.fs.metadata(&path).await?.ok_or_else(|| {
            anyhow!(
                "Failed to get metadata because path does not exist: {:?}",
                path
            )
        })?;
        let mtime = metadata.mtime;
        let key = format!("path:{}", path.to_string_lossy());

        let cached = self
            .database
            .transact({
                let key = key.clone();
                move |db, txn| Ok(db.get(&txn, &key)?)
            })
            .await?;
        if let Some(cached) = cached {
            if cached.mtime == mtime {
                println!("Loading cached summary for: {:?}", path);
                self.paths_loaded_from_cache
                    .lock()
                    .await
                    .insert(path.clone(), true);
                self.summaries
                    .lock()
                    .await
                    .insert(path.clone(), cached.summary);
                return Ok(());
            }
        }

        if path.extension().map_or(false, |ext| ext == "rs") {
            println!("Parsing Rust symbols for: {:?}", path);
            match self
                .parse_and_enqueue_rust_symbols(path.clone(), &content)
                .await
            {
                Ok(_) => {
                    println!("Successfully parsed Rust symbols for: {:?}", path);
                    return Ok(());
                }
                Err(e) => {
                    eprintln!(
                        "Error parsing Rust symbols for {}: {}\nProcessing as text instead",
                        path.display(),
                        e
                    );
                }
            }
        }

        if self.count_tokens(&content) > CHUNK_SIZE {
            println!("Splitting file into chunks: {:?}", path);
            let chunk_count = self.split_and_enqueue_chunks(path.clone(), content).await?;
            println!("File split into {} chunks: {:?}", chunk_count, path);
        } else {
            println!("Summarizing file directly: {:?}", path);
            let summary = self.summarize_file(&path, &content).await?;
            let cached_summary = CachedSummary {
                summary: summary.clone(),
                mtime,
            };
            self.database
                .transact(move |db, mut txn| {
                    db.put(&mut txn, &key, &cached_summary)?;
                    Ok(())
                })
                .await?;
            self.summaries.lock().await.insert(path.clone(), summary);
            println!("File summarized directly: {:?}", path);
        }

        println!("Finished scanning file: {:?}", path);
        Ok(())
    }

    /// Parses Rust symbols from a file's content and enqueues them for processing.
    ///
    /// This method uses the tree-sitter parser to extract Rust symbols from the file content,
    /// and then enqueues each symbol for further processing. It also updates the progress
    /// indicators for the file and overall project.
    async fn parse_and_enqueue_rust_symbols(&self, path: PathBuf, content: &str) -> Result<()> {
        let parsed_file = Arc::new(ParsedFile::new(content.to_string())?);
        let root_node = parsed_file.root_node();

        let export_query = tree_sitter::Query::new(
            &tree_sitter_rust::language(),
            include_str!("./rust_exports.scm"),
        )?;

        let mut export_cursor = tree_sitter::QueryCursor::new();
        let mut symbols = Vec::new();
        for m in export_cursor.matches(&export_query, root_node, parsed_file.content.as_bytes()) {
            if let Some(capture) = m.captures.first() {
                let symbol_name = parsed_file.content[capture.node.byte_range()].to_string();
                let symbol_content = parsed_file.content
                    [capture.node.start_byte()..capture.node.end_byte()]
                    .to_string();
                symbols.push((symbol_name, symbol_content));
            }
        }

        // Update progress before enqueueing
        let symbol_count = symbols.len();
        if symbol_count == 0 {
            return Err(anyhow!("no symbols found for path: {}", path.display()));
        }

        self.outstanding_symbols
            .fetch_add(symbol_count, std::sync::atomic::Ordering::SeqCst);
        self.total_symbols
            .fetch_add(symbol_count, std::sync::atomic::Ordering::SeqCst);

        let mut queue = self.queue.lock().await;
        for (symbol_name, symbol_content) in symbols {
            queue.push_back(Entry::RustSymbol(
                path.clone(),
                symbol_name,
                symbol_content,
                Arc::clone(&parsed_file),
            ));
        }

        Ok(())
    }

    /// Processes a chunk of a file, generating a summary and updating progress indicators.
    ///
    /// This method handles the summarization of a single chunk from a file, updates the database
    /// with the chunk's summary, and manages progress tracking for both the individual file
    /// and the overall project summarization process.
    async fn process_chunk(&self, path: PathBuf, content: String, index: usize) -> Result<()> {
        let chunk_id = (path.clone(), index);

        // Check if the chunk has already been processed
        let mut processed_chunks = self.processed_chunks.lock().await;
        if processed_chunks.contains_key(&chunk_id) {
            println!("Chunk already processed: {:?}", chunk_id);
            return Ok(());
        }

        // Mark the chunk as being processed
        processed_chunks.insert(chunk_id.clone(), true);
        drop(processed_chunks);

        println!("Processing chunk: {:?}", chunk_id);

        let summary = self.summarize_file(&path, &content).await?;
        let key = format!("chunk:{}_{}", path.to_string_lossy(), index);
        let metadata = self.fs.metadata(&path).await?.ok_or_else(|| {
            anyhow!(
                "Failed to get metadata because path does not exist: {:?}",
                path
            )
        })?;
        let mtime = metadata.mtime;
        let cached_summary = CachedSummary {
            summary: summary.clone(),
            mtime,
        };
        self.database
            .transact(move |db, mut txn| {
                db.put(&mut txn, &key, &cached_summary)?;
                Ok(())
            })
            .await?;
        self.summaries
            .lock()
            .await
            .entry(path.clone())
            .or_insert_with(String::new)
            .push_str(&summary);

        println!("Finished processing chunk: {:?}", chunk_id);

        Ok(())
    }

    /// Splits the given content into chunks and enqueues them for processing.
    ///
    /// This method takes the content of a file, splits it into manageable chunks,
    /// and enqueues each chunk for further processing. It also updates the progress
    /// indicators for both the individual file and the overall project summarization.
    async fn split_and_enqueue_chunks(&self, path: PathBuf, content: String) -> Result<usize> {
        let chunks = self.split_into_chunks(&content);
        let chunk_count = chunks.len();
        println!("Splitting file {:?} into {} chunks", path, chunk_count);

        for (index, chunk) in chunks.into_iter().enumerate() {
            println!("Enqueueing chunk {} for file {:?}", index, path);
            self.queue
                .lock()
                .await
                .push_back(Entry::Chunk(path.clone(), chunk, index));
        }
        self.outstanding_chunks
            .fetch_add(chunk_count, std::sync::atomic::Ordering::SeqCst);
        self.total_chunks
            .fetch_add(chunk_count, std::sync::atomic::Ordering::SeqCst);

        println!(
            "Total outstanding chunks after enqueueing: {}",
            self.outstanding_chunks
                .load(std::sync::atomic::Ordering::SeqCst)
        );

        Ok(chunk_count)
    }

    /// Processes a Rust symbol, generating a summary and updating progress indicators.
    ///
    /// This method handles the summarization of a single Rust symbol, updates the database
    /// with the symbol's summary, and manages progress tracking for both the individual file
    /// and the overall project summarization process.
    async fn process_rust_symbol(
        &self,
        path: PathBuf,
        name: String,
        content: String,
        parsed_file: Arc<ParsedFile>,
    ) -> Result<()> {
        let context = parsed_file.extract_symbol_context(&name);
        let summary = self
            .summarize_rust_symbol(&name, &content, &context)
            .await?;

        // Save the symbol summary
        let key = format!("symbol:{}::{}", path.to_string_lossy(), name);
        let metadata = self.fs.metadata(&path).await?.ok_or_else(|| {
            anyhow!(
                "Failed to get metadata because path does not exist: {:?}",
                path
            )
        })?;
        let mtime = metadata.mtime;
        let cached_summary = CachedSummary {
            summary: summary.clone(),
            mtime,
        };
        self.database
            .transact(move |db, mut txn| {
                db.put(&mut txn, &key, &cached_summary)?;
                Ok(())
            })
            .await?;

        self.summaries
            .lock()
            .await
            .entry(path.clone())
            .or_default()
            .push_str(&summary);
        self.outstanding_symbols
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

        Ok(())
    }

    /// Summarizes a Rust symbol by generating a brief description of its functionality and purpose.
    ///
    /// This method uses the AI model to create a concise summary of the given Rust symbol,
    /// focusing on its main functionality and purpose.
    async fn summarize_rust_symbol(
        &self,
        name: &str,
        content: &str,
        context: &str,
    ) -> Result<String> {
        let messages = vec![Message {
            role: "user".to_string(),
            content: format!(
                "You are a code summarization assistant. \
                Provide a brief summary of the given Rust symbol, \
                focusing on its main functionality and purpose. \
                Be terse and start your response directly with \"Summary: \".\n\
                Symbol name: {}\n\
                Symbol content:\n{}\n\
                Symbol context:\n{}",
                name, content, context
            ),
        }];

        let mut receiver = self.stream_completion(messages).await?;

        let mut summary = String::new();
        while let Some(content) = receiver.next().await {
            summary.push_str(&content);
        }

        Ok(summary)
    }

    fn count_tokens(&self, content: &str) -> usize {
        self.tokenizer
            .encode(content, false)
            .unwrap()
            .get_ids()
            .len()
    }

    /// Summarizes the content of a file, generating a brief description of its functionality and purpose.
    async fn summarize_file(&self, path: &Path, content: &str) -> Result<String> {
        let messages = vec![Message {
            role: "user".to_string(),
            content: format!(
                "You are a code summarization assistant. \
                Provide a brief summary of the given file, \
                focusing on its main functionality and purpose. \
                Be terse and start your response directly with \"Summary: \".\n\
                File path: {}\n\
                File content:\n{}",
                path.display(),
                content
            ),
        }];

        let mut receiver = self.stream_completion(messages).await?;

        let mut summary = String::new();
        while let Some(content) = receiver.next().await {
            summary.push_str(&content);
        }

        Ok(summary)
    }

    /// Splits the given content into chunks of roughly equal size based on token count.
    ///
    /// This method tokenizes the input content and creates chunks that do not exceed
    /// the specified CHUNK_SIZE. It attempts to maintain line integrity where possible,
    /// but will truncate lines if necessary to fit within the chunk size limit.
    fn split_into_chunks(&self, content: &str) -> Vec<String> {
        let mut chunks = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        let mut current_chunk = String::new();
        let mut current_chunk_token_count = 0;

        for line in lines {
            let encoded = self.tokenizer.encode(line, false).unwrap();
            let line_tokens = encoded.get_ids();
            let line_token_count = line_tokens.len();

            if current_chunk_token_count + line_token_count > CHUNK_SIZE {
                // Flush the current chunk
                chunks.push(current_chunk.clone());
                current_chunk.clear();
                current_chunk_token_count = 0;
            }

            if line_token_count > CHUNK_SIZE {
                // Truncate the line and append it
                for token in encoded.get_tokens().into_iter().take(CHUNK_SIZE) {
                    current_chunk.push_str(token);
                }
                chunks.push(current_chunk.clone());
                current_chunk.clear();
                current_chunk_token_count = 0;
            } else {
                // Add the line to the current chunk
                current_chunk.push_str(line);
                current_chunk.push('\n');
                current_chunk_token_count += line_token_count;
            }
        }

        if !current_chunk.is_empty() {
            chunks.push(current_chunk);
        }

        chunks
    }

    /// Retrieves the summary for a given file path from the database or in-memory cache.
    async fn summary_for_path(&self, path: &Path) -> Result<Option<String>> {
        let key = format!("path:{}", path.to_string_lossy());
        let cached_summary = self
            .database
            .transact(move |db, txn| Ok(db.get(&txn, &key)?))
            .await?;

        if let Some(cached) = cached_summary {
            return Ok(Some(cached.summary));
        }

        if let Some(summary) = self.summaries.lock().await.get(path) {
            return Ok(Some(summary.clone()));
        }

        Ok(None)
    }

    /// Retrieves the summary for a given Rust symbol from the database.
    pub async fn summary_for_symbol(
        &self,
        path: &Path,
        symbol_name: &str,
    ) -> Result<Option<String>> {
        let key = format!("symbol:{}::{}", path.to_string_lossy(), symbol_name);
        let cached_summary = self
            .database
            .transact(move |db, txn| Ok(db.get(&txn, &key)?))
            .await?;

        if let Some(cached) = cached_summary {
            return Ok(Some(cached.summary));
        }

        Ok(None)
    }

    /// Exports the contents of the database to stdout in JSON format.
    ///
    /// This method iterates through all entries in the database and prints
    /// them as formatted JSON objects, including type, path, summary, and
    /// modification time information for each entry.
    pub async fn export_database(&self) -> Result<()> {
        self.database
            .transact(|db, txn| {
                for item in db.iter(&txn)? {
                    let (key, value) = item?;
                    let (prefix, path) = key.split_once(':').unwrap_or(("unknown", key));
                    let entry = serde_json::json!({
                        "type": prefix,
                        "path": path,
                        "summary": value.summary,
                        "mtime": value.mtime.duration_since(SystemTime::UNIX_EPOCH)?.as_secs()
                    });
                    println!("{}", serde_json::to_string_pretty(&entry)?);
                }
                Ok(())
            })
            .await
    }

    /// Builds a gitignore matcher for the given root directory.
    ///
    /// This method traverses the directory tree, reading .gitignore files
    /// and constructing a gitignore matcher that can be used to filter files
    /// and directories based on gitignore rules.
    async fn build_gitignore(&self, root: &Path) -> Result<ignore::gitignore::Gitignore> {
        let mut builder = GitignoreBuilder::new(root);

        let mut dir_stack = vec![root.to_path_buf()];
        while let Some(dir) = dir_stack.pop() {
            let gitignore_path = dir.join(".gitignore");
            if self.fs.is_file(&gitignore_path).await {
                if let Ok(content) = self.fs.load(&gitignore_path).await {
                    for line in content.lines() {
                        builder.add_line(Some(gitignore_path.clone()), line)?;
                    }
                }
            }

            let mut read_dir = self.fs.read_dir(&dir).await?;
            while let Some(entry) = read_dir.next().await {
                let entry = entry?;
                if self.fs.is_dir(&entry).await {
                    dir_stack.push(entry);
                }
            }
        }

        Ok(builder.build()?)
    }
}

#[derive(Debug)]
struct DirEntry {
    path: PathBuf,
    metadata: fs::Metadata,
}

#[derive(Debug)]
struct ParsedFile {
    content: Arc<str>,
    tree: Arc<Tree>,
}

impl ParsedFile {
    fn new(content: String) -> Result<Self> {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_rust::language())?;
        let tree = parser
            .parse(&content, None)
            .ok_or_else(|| anyhow!("Failed to parse file content"))?;

        Ok(Self {
            content: Arc::from(content),
            tree: Arc::new(tree),
        })
    }

    fn root_node(&self) -> Node {
        self.tree.root_node()
    }

    fn extract_symbol_context(&self, symbol_name: &str) -> String {
        let tree_sitter_context = self.extract_symbol_context_tree_sitter(symbol_name);
        let module_structure = self.extract_module_structure(symbol_name);
        let nearby_functions = self.extract_nearby_functions(symbol_name);

        format!(
            "Full symbol context:\n{}\n\n{}\n\n{}",
            tree_sitter_context, module_structure, nearby_functions
        )
    }

    fn extract_symbol_context_tree_sitter(&self, symbol_name: &str) -> String {
        let query = tree_sitter::Query::new(
            &tree_sitter_rust::language(),
            &format!("((function_item name: (identifier) @func-name) @function (#eq? @func-name \"{}\"))", symbol_name)
        ).unwrap();

        let mut query_cursor = tree_sitter::QueryCursor::new();
        let matches = query_cursor.matches(&query, self.root_node(), self.content.as_bytes());

        for m in matches {
            if let Some(func_node) = m.captures.iter().find(|c| c.index == 1) {
                let start_byte = func_node.node.start_byte();
                let end_byte = func_node.node.end_byte();
                return self.content[start_byte..end_byte].to_string();
            }
        }

        String::new()
    }

    fn extract_module_structure(&self, _symbol_name: &str) -> String {
        let mut module_path = Vec::new();
        let mut current_node = self.root_node();

        while let Some(parent) = current_node.parent() {
            if parent.kind() == "mod_item" {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    module_path.push(self.content[name_node.byte_range()].to_string());
                }
            }
            current_node = parent;
        }

        module_path.reverse();
        format!("Module path: {}", module_path.join("::"))
    }

    fn extract_nearby_functions(&self, symbol_name: &str) -> String {
        let query = tree_sitter::Query::new(
            &tree_sitter_rust::language(),
            "(function_item name: (identifier) @func-name)",
        )
        .unwrap();

        let mut query_cursor = tree_sitter::QueryCursor::new();
        let matches = query_cursor.matches(&query, self.root_node(), self.content.as_bytes());

        let mut nearby_functions = Vec::new();
        for m in matches {
            if let Some(func_name_node) = m.captures.iter().find(|c| c.index == 0) {
                let func_name = self.content[func_name_node.node.byte_range()].to_string();
                if func_name != symbol_name {
                    nearby_functions.push(func_name);
                }
            }
        }

        format!("Nearby functions: {}", nearby_functions.join(", "))
    }
}

#[derive(Debug)]
enum Entry {
    File(PathBuf),
    Directory(PathBuf, Vec<PathBuf>),
    Chunk(PathBuf, String, usize),
    RustSymbol(PathBuf, String, String, Arc<ParsedFile>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use futures::{channel::mpsc, future::BoxFuture, FutureExt, SinkExt};
    use gpui::TestAppContext;
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct FakeLanguageModel;

    impl LanguageModel for FakeLanguageModel {
        fn stream_completion(
            &self,
            messages: Vec<Message>,
        ) -> BoxFuture<Result<mpsc::Receiver<String>>> {
            let content = messages[0].content.clone();
            let summary = if content.contains("Rust symbol") {
                "Summary: Detailed Rust symbol summary".to_string()
            } else {
                "Summary: Generic file summary".to_string()
            };
            async move {
                let (mut tx, rx) = mpsc::channel(1);
                tx.send(summary).await?;
                Ok(rx)
            }
            .boxed()
        }
    }

    #[gpui::test]
    async fn test_miner(cx: &mut TestAppContext) {
        // Create a temporary directory for the database
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().to_path_buf();

        // Set up a fake file system
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/project",
            json!({
                "src": {
                    "main.rs": "fn main() { println!(\"Hello, world!\"); }",
                    "lib.rs": "pub fn add(a: i32, b: i32) -> i32 { a + b }",
                },
                "tests": {
                    "test_main.rs": "#[test] fn test_main() { assert!(true); }",
                },
                ".gitignore": "*.log\ntarget/",
                "README.md": "# Project README",
                "ignored.log": "This file should be ignored",
                "target": {
                    "debug": {
                        "build": "This directory should be ignored",
                    },
                },
            }),
        )
        .await;

        // Create a Miner instance
        let miner = Miner::new(
            db_path,
            PathBuf::from("/project"),
            4,
            fs.clone(),
            cx.background_executor.clone(),
            Arc::new(FakeLanguageModel),
        )
        .await
        .unwrap();

        // Run the summarization
        miner.summarize_project().await.unwrap();

        // Check summaries for files and directories
        let expected_summaries = vec![
            "/project",
            "/project/src",
            "/project/src/main.rs",
            "/project/src/lib.rs",
            "/project/tests",
            "/project/tests/test_main.rs",
            "/project/README.md",
        ];

        for path in expected_summaries {
            let summary = miner.summary_for_path(Path::new(path)).await.unwrap();
            assert!(summary.is_some(), "Missing summary for path: {}", path);
            let summary = summary.unwrap();
            assert!(
                !summary.is_empty(),
                "Summary should not be empty for path: {}",
                path
            );
        }

        // Check summaries for Rust symbols
        let expected_symbol_summaries = vec![
            ("/project/src/main.rs", "main"),
            ("/project/src/lib.rs", "add"),
            ("/project/tests/test_main.rs", "test_main"),
        ];

        for (file_path, symbol_name) in expected_symbol_summaries {
            let symbol_summary = miner
                .summary_for_symbol(Path::new(file_path), symbol_name)
                .await
                .unwrap();
            assert!(
                symbol_summary.is_some(),
                "Missing summary for symbol: {} in {}",
                symbol_name,
                file_path
            );
            let symbol_summary = symbol_summary.unwrap();
            assert!(
                !symbol_summary.is_empty(),
                "Summary should not be empty for symbol: {} in {}",
                symbol_name,
                file_path
            );
        }

        // Check that ignored files/directories don't have summaries
        let ignored_paths = vec![
            "/project/ignored.log",
            "/project/target",
            "/project/target/debug",
            "/project/target/debug/build",
        ];

        for path in ignored_paths {
            let summary = miner.summary_for_path(Path::new(path)).await.unwrap();
            assert!(
                summary.is_none(),
                "Ignored path should not have a summary: {}",
                path
            );
        }

        // Delete a file and verify its summary is removed
        fs.remove_file(Path::new("/project/src/lib.rs"), Default::default())
            .await
            .unwrap();

        // Run the summarization again
        miner.summarize_project().await.unwrap();

        // Verify the deleted file's summary is gone
        let deleted_file_summary = miner
            .summary_for_path(Path::new("/project/src/lib.rs"))
            .await
            .unwrap();
        assert!(
            deleted_file_summary.is_none(),
            "Summary for deleted file should be None"
        );

        // Verify the deleted file's symbol summary is gone
        let deleted_symbol_summary = miner
            .summary_for_symbol(Path::new("/project/src/lib.rs"), "add")
            .await
            .unwrap();
        assert!(
            deleted_symbol_summary.is_none(),
            "Symbol summary for deleted file should be None"
        );

        // Verify other summaries still exist
        let existing_file_summary = miner
            .summary_for_path(Path::new("/project/src/main.rs"))
            .await
            .unwrap();
        assert!(
            existing_file_summary.is_some(),
            "Summary for existing file should still be present"
        );

        let existing_symbol_summary = miner
            .summary_for_symbol(Path::new("/project/src/main.rs"), "main")
            .await
            .unwrap();
        assert!(
            existing_symbol_summary.is_some(),
            "Symbol summary for existing file should still be present"
        );
    }
}
