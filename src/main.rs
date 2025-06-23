use {
    clap::Parser,
    clap_markdown::help_markdown,
    indicatif::ProgressBar,
    notify_rust::{Notification, Timeout},
    prettylogger::Logger,
    rayon::iter::{IntoParallelRefIterator, ParallelIterator},
    serde::{Deserialize, Serialize},
    std::{
        collections::{HashMap, HashSet},
        fs::{self, *},
        io::*,
        path::Path,
        process,
        sync::{Arc, LazyLock, Mutex},
    },
    walkdir::WalkDir,
};

const DEFAULT_CATEGORY_CONFIG: &str = r#"
[categories]
Images = ["gif", "ico", "jpeg", "jpg", "jpg~", "png", "png~", "webp"]
Videos = ["mp4", "mkv", "ogv", "webm"]
Documents = ["pdf", "docx", "doc", "txt", "md"]
Audio = ["mp3", "wav", "flac", "ogg"]
Archives = ["zip", "tar", "gz", "rar"]
"#;

static LOGGER_INTERFACE: LazyLock<Logger> = LazyLock::new(|| Logger::default());

#[derive(clap::Parser)]
struct Cli {
    /// The directory to sort the files into
    #[arg(short, long)]
    output_dir: Option<String>,

    /// Send a notification when finished
    #[arg(short, long)]
    notify: bool,

    /// Move files instead of copying them
    #[arg(short, long = "move")]
    mv: bool,

    /// Extensions to exclude from sorting (comma-separated, e.g., 'txt,log,tmp')
    #[arg(short, long)]
    blacklist: Option<String>,

    /// Path to file containing blacklisted extensions (one per line)
    #[arg(long = "blacklist-file")]
    blacklist_file: Option<String>,

    /// Number of threads to use for parallel processing (default: number of CPU cores)
    #[arg(short = 'j', long = "threads")]
    threads: Option<usize>,

    /// Maximum depth to recurse into directories (0 = current directory only, default: unlimited)
    #[arg(short = 'd', long = "max-depth")]
    max_depth: Option<usize>,

    #[arg(short = 'c', long = "config")]
    config: Option<String>,

    #[arg(short, long, hide = true)]
    gen_docs: bool,
}

#[derive(Serialize, Deserialize)]
struct SorterConfig {
    categories: HashMap<String, Vec<String>>,
}

fn move_file<P: AsRef<Path>>(from: P, to: P) -> Result<()> {
    let from_path = from.as_ref();
    let to_path = to.as_ref();

    if !from_path.exists() {
        return Err(Error::new(
            ErrorKind::NotFound,
            "[ERROR]: Source file does not exist",
        ));
    }

    if let Some(parent) = to_path.parent() {
        create_dir_all(parent)?;
    }

    rename(from_path, to_path)?;

    Ok(())
}

fn load_categories(
    path: &Option<String>,
) -> std::result::Result<HashMap<String, Vec<String>>, Box<dyn std::error::Error>> {
    let content = if let Some(path_str) = path {
        match fs::read_to_string(path_str) {
            Ok(contents) => contents,
            Err(e) => {
                LOGGER_INTERFACE.warning(
                    format!(
                        "Failed to read config file '{}': {}\nFalling back to default.",
                        path_str, e
                    )
                    .as_str(),
                );
                DEFAULT_CATEGORY_CONFIG.to_string()
            }
        }
    } else {
        DEFAULT_CATEGORY_CONFIG.to_string()
    };

    let config: SorterConfig = toml::from_str(&content)?;
    let normalized = config
        .categories
        .into_iter()
        .map(|(k, v)| {
            let cleaned_exts = v
                .into_iter()
                .map(|ext| ext.trim_start_matches('.').to_lowercase())
                .collect();
            (k, cleaned_exts)
        })
        .collect();

    Ok(normalized)
}

fn get_category<'a>(ext: &str, categories: &'a HashMap<String, Vec<String>>) -> Option<&'a str> {
    for (cat, exts) in categories {
        if exts.contains(&ext.to_lowercase()) {
            return Some(cat);
        }
    }

    None
}

fn copy_file(source: &str, dest: &str, clean: bool) -> Result<()> {
    let src = Path::new(source);
    let destination = Path::new(dest);

    if clean && destination.exists() {
        remove_file(destination)?;
    }

    let mut reader = File::open(src)?;
    let mut buffer = Vec::new();

    reader.read_to_end(&mut buffer)?;
    let mut writer = File::create(destination)?;

    writer.write_all(&buffer)?;

    Ok(())
}

fn send_finished_notif(operation: &str) {
    if let Err(e) = Notification::new()
        .summary(&format!("Finished {}", operation))
        .body(&format!(
            "`dirsort` has finished {} the directory",
            operation
        ))
        .icon("vivaldi")
        .timeout(Timeout::Milliseconds(1000))
        .show()
    {
        LOGGER_INTERFACE.warning(format!("Failed to display notification: {}", e).as_str());
    }
}

fn load_blacklist(argv: &Cli) -> std::result::Result<HashSet<String>, Box<dyn std::error::Error>> {
    let mut blacklist = HashSet::new();

    if let Some(ref blacklist_str) = argv.blacklist {
        for ext in blacklist_str.split(',') {
            let ext = ext.trim().to_lowercase();

            if !ext.is_empty() {
                let ext = if ext.starts_with('.') {
                    ext.strip_prefix('.').unwrap().to_string()
                } else {
                    ext
                };

                blacklist.insert(ext);
            }
        }
    }

    if let Some(ref file_path) = argv.blacklist_file {
        let content = fs::read_to_string(file_path)
            .map_err(|e| format!("Failed to read blacklist file '{}': {}", file_path, e))?;

        for line in content.lines() {
            let ext = line.trim().to_lowercase();
            if !ext.is_empty() && !ext.starts_with('#') {
                let ext = if ext.starts_with('.') {
                    ext.strip_prefix('.').unwrap().to_string()
                } else {
                    ext
                };

                blacklist.insert(ext);
            }
        }
    }

    Ok(blacklist)
}

fn is_blacklisted(file_path: &Path, blacklist: &HashSet<String>) -> bool {
    if blacklist.is_empty() {
        return false;
    }

    if let Some(extension) = file_path.extension() {
        if let Some(ext_str) = extension.to_str() {
            return blacklist.contains(&ext_str.to_lowercase());
        }
    }
    false
}

fn setup_thread_pool(
    thread_count: Option<usize>,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    match thread_count {
        Some(count) => {
            if count == 0 {
                return Err("Thread count must be greater than 0".into());
            }
            rayon::ThreadPoolBuilder::new()
                .num_threads(count)
                .build_global()
                .map_err(|e| format!("Failed to configure thread pool: {}", e))?;

            LOGGER_INTERFACE.info(format!("Using {} threads", count).as_str());
        }
        None => {
            let default_threads = rayon::current_num_threads();
            LOGGER_INTERFACE.info(format!("Using {} threads", default_threads).as_str());
        }
    }
    Ok(())
}

fn collect_files(
    max_depth: Option<usize>,
) -> std::result::Result<Vec<walkdir::DirEntry>, Box<dyn std::error::Error>> {
    let mut walker = WalkDir::new(".");

    if let Some(depth) = max_depth {
        walker = walker.max_depth(depth);
        match depth {
            0 => LOGGER_INTERFACE.info("Collecting files from current directory only"),
            1 => LOGGER_INTERFACE
                .info("Collecting files from current directory and immediate subdirectories"),
            _ => LOGGER_INTERFACE
                .info(format!("Collecting files with maximum depth of {} levels", depth).as_str()),
        }
    } else {
        LOGGER_INTERFACE.info("Collecting files from all subdirectories (unlimited depth)");
    }

    let mut entries = Vec::new();
    let mut directories_scanned = 0;

    for entry in walker {
        let item = match entry {
            Ok(item) => item,
            Err(e) => {
                LOGGER_INTERFACE.error(format!("Failed to read directory entry: {}", e).as_str());
                continue;
            }
        };

        if item.path().is_file() {
            entries.push(item);
        } else if item.path().is_dir() {
            directories_scanned += 1;
        }
    }

    LOGGER_INTERFACE.info(
        format!(
            "Scanned {} directories, found {} files",
            directories_scanned,
            entries.len()
        )
        .as_str(),
    );
    Ok(entries)
}

fn process_file(
    entry: &walkdir::DirEntry,
    out_dir: &str,
    use_move: bool,
    blacklist: &HashSet<String>,
    categories: &HashMap<String, Vec<String>>,
    errors: &Arc<Mutex<Vec<String>>>,
    skipped: &Arc<Mutex<u64>>,
) {
    if is_blacklisted(entry.path(), blacklist) {
        if let Ok(mut skipped_count) = skipped.lock() {
            *skipped_count += 1;
        }
        return;
    }

    let result = || -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let file_name = entry
            .file_name()
            .to_str()
            .ok_or("Invalid filename encoding")?;

        let source_path = entry.path().display().to_string();

        let (target_dir, dest_path) = if let Some(ext) = entry.path().extension() {
            let ext_str = ext.to_str().ok_or("Invalid extension encoding")?;
            let category = get_category(ext_str, categories);
            let subfolder = category.unwrap_or(ext_str);
            let target_dir = Path::new(out_dir).join(subfolder);
            let dest_path = target_dir.join(file_name);
            (target_dir, dest_path)
        } else {
            let target_dir = Path::new(out_dir).join("unknown");
            let dest_path = target_dir.join(file_name);
            (target_dir, dest_path)
        };

        create_dir_all(&target_dir)?;

        if use_move {
            move_file(
                &source_path.to_string(),
                &dest_path.to_str().unwrap().to_string(),
            )?;
        } else {
            copy_file(&source_path, dest_path.to_str().unwrap(), false)?;
        }

        Ok(())
    };

    if let Err(e) = result() {
        let error_msg = format!("Failed to process '{}': {}", entry.path().display(), e);
        if let Ok(mut errors_vec) = errors.lock() {
            errors_vec.push(error_msg);
        }
    }
}

fn main() {
    let args = Cli::parse();

    if args.gen_docs {
        println!("{}", help_markdown::<Cli>());
        process::exit(1);
    }

    if let Err(e) = setup_thread_pool(args.threads) {
        LOGGER_INTERFACE.error(format!("Error configuring threads: {}", e).as_str());
        process::exit(1);
    }

    let blacklist = match load_blacklist(&args) {
        Ok(blacklist) => blacklist,
        Err(e) => {
            LOGGER_INTERFACE.error(format!("Error loading blacklist: {}", e).as_str());
            process::exit(1);
        }
    };

    if !blacklist.is_empty() {
        LOGGER_INTERFACE.info(
            format!(
                "Blacklisted extensions: {}",
                blacklist
                    .iter()
                    .map(|s| format!(".{}", s))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .as_str(),
        );
    }

    let entries = match collect_files(args.max_depth) {
        Ok(entries) => entries,
        Err(e) => {
            LOGGER_INTERFACE.error(format!("Error collecting files: {}", e).as_str());
            process::exit(1);
        }
    };

    if entries.is_empty() {
        LOGGER_INTERFACE.warning("No files found to process.");
        return;
    }

    let progress = ProgressBar::new(entries.len() as u64);
    let out_dir = args.output_dir.unwrap_or_else(|| "sorted".to_string());
    let errors = Arc::new(Mutex::new(Vec::new()));
    let skipped = Arc::new(Mutex::new(0u64));

    if let Err(e) = create_dir_all(&out_dir) {
        LOGGER_INTERFACE
            .error(format!("Failed to create output directory '{}': {}", out_dir, e).as_str());
        process::exit(1);
    }

    let operation = if args.mv { "moving" } else { "copying" };
    LOGGER_INTERFACE.info(
        format!(
            "Starting {} {} files to '{}'...",
            operation,
            entries.len(),
            out_dir
        )
        .as_str(),
    );

    let category_map = match load_categories(&args.config) {
        Ok(map) => map,
        Err(e) => {
            LOGGER_INTERFACE.error(format!("Error loading categories: {}", e).as_str());
            process::exit(1);
        }
    };

    if !category_map.is_empty() {
        LOGGER_INTERFACE.info("Loaded categories:");
        for (cat, exts) in &category_map {
            LOGGER_INTERFACE.info(format!("  {}: {:?}", cat, exts).as_str());
        }
    }

    entries.par_iter().for_each(|entry| {
        process_file(
            entry,
            &out_dir,
            args.mv,
            &blacklist,
            &category_map,
            &errors,
            &skipped,
        );
        progress.inc(1);
    });

    progress.finish();

    let skipped_count = match skipped.lock() {
        Ok(count) => *count,
        Err(_) => {
            LOGGER_INTERFACE.warning("Failed to get skipped file count");
            0
        }
    };
    let processed_count = entries.len() as u64 - skipped_count;

    if let Ok(errors_vec) = errors.lock() {
        if !errors_vec.is_empty() {
            LOGGER_INTERFACE.error("\nErrors encountered during processing:");
            for error in errors_vec.iter() {
                LOGGER_INTERFACE.error(format!("  {}", error).as_str());
            }
            LOGGER_INTERFACE
                .info(format!("\nProcessing completed with {} errors.", errors_vec.len()).as_str());
        }
    }

    LOGGER_INTERFACE.info("\nSummary:");
    LOGGER_INTERFACE.info(format!("  Files processed: {}", processed_count).as_str());
    if skipped_count > 0 {
        LOGGER_INTERFACE.info(format!("  Files skipped (blacklisted): {}", skipped_count).as_str());
    }

    LOGGER_INTERFACE.info(format!("  Total files found: {}", entries.len()).as_str());

    if args.notify {
        let operation = if args.mv { "moving" } else { "sorting" };
        send_finished_notif(operation);
    }
}
