use {
    actix_files::Files,
    actix_web::{App, HttpServer},
    clap::Parser,
    clap_markdown::help_markdown,
    indicatif::ProgressBar,
    notify_rust::{Notification, Timeout},
    prettylogger::Logger,
    rayon::iter::{IntoParallelRefIterator, ParallelIterator},
    serde::{Deserialize, Serialize},
    std::{
        collections::{HashMap, HashSet},
        error::{self, Error},
        fs::{self, File, create_dir_all, remove_file, rename},
        hash::RandomState,
        io::{Result, Write},
        path::{Path, PathBuf},
        process,
        sync::{
            Arc, LazyLock, Mutex,
            atomic::{AtomicU64, Ordering},
        },
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

static LOGGER_INTERFACE: LazyLock<Logger> = LazyLock::new(Logger::default);

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

    /// Path to a config file containing extension categories
    #[arg(short = 'c', long = "config")]
    config: Option<String>,

    /// Generate an HTML index file after sorting
    #[arg(short = 'i', long = "index")]
    gen_html: bool,

    /// Serves the resulting sorted directory
    #[arg(short, long)]
    serve: bool,

    #[arg(short, long)]
    verbose: bool,

    #[arg(short, long, hide = true)]
    gen_docs: bool,
}

#[derive(Serialize, Deserialize)]
struct SorterConfig {
    categories: HashMap<String, Vec<String>>,
}

fn move_file(from: &Path, to: &Path) -> Result<()> {
    rename(from, to)
}

fn gen_html_index(output_dir: &Path) -> Result<()> {
    let index_path = output_dir.join("index.html");
    let mut file = File::create(&index_path)?;

    let html = format!(
        "<!DOCTYPE html>
<html>
<head>
    <title>Directory Index</title>
    <style>
        body {{ font-family: Arial, sans-serif; margin: 20px; }}
        h1 {{ color: #333; }}
        ul {{ list-style-type: none; padding: 0; }}
        li {{ margin: 5px 0; }}
        a {{ color: #0066cc; text-decoration: none; }}
        a:hover {{ text-decoration: underline; }}
        .dir {{ font-weight: bold; color: #009933; }}
    </style>
</head>
<body>
    <h1>Directory Index: {}</h1>
    <ul>
",
        output_dir.display(),
    );

    file.write_all(html.as_bytes())?;

    for entry in WalkDir::new(output_dir)
        .min_depth(1)
        .sort_by(|a, b| a.file_name().cmp(b.file_name()))
    {
        let entry = entry?;
        let path = entry.path();
        let relative_path = path.strip_prefix(output_dir).expect("AAAAAAA");

        if path.is_dir() {
            writeln!(
                file,
                r#"        <li><span class="dir">📁 {}/</span></li>"#,
                relative_path.display()
            )?;
        } else {
            let abs_path = path.canonicalize()?;
            writeln!(
                file,
                r#"        <li><a href="file://{}" target="_blank">📄  {}</a></li>"#,
                abs_path.display(),
                relative_path.display()
            )?;
        }
    }

    writeln!(
        file,
        "    </ul>
</body>
</html>"
    )?;

    LOGGER_INTERFACE.info(format!("Generated HTML index at {}", index_path.display()).as_str());

    Ok(())
}

fn load_categories(
    path: Option<&String>,
) -> std::result::Result<HashMap<String, Vec<String>>, Box<dyn error::Error>> {
    let content = path.map_or_else(
        || DEFAULT_CATEGORY_CONFIG.to_string(),
        |path_str| {
            fs::read_to_string(path_str).unwrap_or_else(|e| {
                LOGGER_INTERFACE.warning(
                    format!(
                        "Failed to read config file '{path_str}': {e}\nFalling back to default."
                    )
                    .as_str(),
                );
                DEFAULT_CATEGORY_CONFIG.to_string()
            })
        },
    );

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

fn copy_file(source: &str, dest: &str) -> Result<()> {
    if Path::new(dest).exists() {
        remove_file(dest)?;
    }

    fs::copy(source, dest)?;

    Ok(())
}

fn send_finished_notif(operation: &str) {
    if let Err(e) = Notification::new()
        .summary(&format!("Finished {operation}"))
        .body(&format!("`dirsort` has finished {operation} the directory"))
        .icon("vivaldi")
        .timeout(Timeout::Milliseconds(1000))
        .show()
    {
        LOGGER_INTERFACE.warning(format!("Failed to display notification: {e}").as_str());
    }
}

fn load_blacklist(argv: &Cli) -> std::result::Result<HashSet<String>, Box<dyn error::Error>> {
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
            .map_err(|e| format!("Failed to read blacklist file '{file_path}': {e}"))?;

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
    file_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| blacklist.contains(ext))
}

fn setup_thread_pool(
    thread_count: Option<usize>,
) -> std::result::Result<(), Box<dyn error::Error>> {
    if let Some(count) = thread_count {
        if count == 0 {
            return Err("Thread count must be greater than 0".into());
        }
        rayon::ThreadPoolBuilder::new()
            .num_threads(count)
            .build_global()
            .map_err(|e| format!("Failed to configure thread pool: {e}"))?;

        LOGGER_INTERFACE.info(format!("Using {count} threads").as_str());
    } else {
        let default_threads = rayon::current_num_threads();
        LOGGER_INTERFACE.info(format!("Using {default_threads} threads").as_str());
    }
    Ok(())
}

fn collect_files(max_depth: Option<usize>) -> Vec<walkdir::DirEntry> {
    let mut walker = WalkDir::new(".").follow_links(true);

    if let Some(depth) = max_depth {
        walker = walker.max_depth(depth);
    }

    let (entries, dir_count) = walker.into_iter().filter_map(std::result::Result::ok).fold(
        (Vec::new(), 0),
        |(mut files, mut dirs), entry| {
            if entry.file_type().is_dir() {
                dirs += 1;
            } else if entry.file_type().is_file() {
                files.push(entry);
            }
            (files, dirs)
        },
    );

    LOGGER_INTERFACE.info(
        format!(
            "Scanned {} directories, found {} files",
            dir_count,
            entries.len()
        )
        .as_str(),
    );

    entries
}

fn process_file(
    entry: &walkdir::DirEntry,
    out_dir: &Path,
    use_move: bool,
    blacklist: &HashSet<String>,
    categories: &HashMap<String, Vec<String>>,
    errors: &Arc<Mutex<Vec<String>>>,
    skipped: &Arc<AtomicU64>,
) {
    if is_blacklisted(entry.path(), blacklist) {
        skipped.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let result = || -> std::result::Result<(), Box<dyn error::Error + Send + Sync>> {
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
                source_path.as_ref(),
                dest_path.to_str().unwrap().to_string().as_ref(),
            )?;
        } else {
            copy_file(&source_path, dest_path.to_str().unwrap())?;
        }

        Ok(())
    };

    if let Err(e) = result() {
        let error_msg = format!("Failed to process '{}': {}", entry.path().display(), e);
        if let Ok(mut errors_vec) = errors.lock() {
            if Cli::parse().verbose {
                errors_vec.push(error_msg);
            }
        }
    }
}

fn get_blacklist(
    args: &Cli,
) -> std::result::Result<HashSet<String, RandomState>, Box<dyn error::Error>> {
    load_blacklist(args)
}

fn get_categories(
    path: &Option<String>,
) -> std::result::Result<HashMap<String, Vec<String>>, Box<dyn Error>> {
    load_categories(path.as_ref())
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let args = Cli::parse();

    if args.gen_docs {
        println!("{}", help_markdown::<Cli>());
        process::exit(1);
    }

    if let Err(e) = setup_thread_pool(args.threads) {
        LOGGER_INTERFACE.error(format!("Error configuring threads: {e}").as_str());
        process::exit(1);
    }

    let blacklist = get_blacklist(&args).expect("Failed to fetch blacklist");

    if !blacklist.is_empty() {
        LOGGER_INTERFACE.info(
            format!(
                "Blacklisted extensions: {}",
                blacklist
                    .iter()
                    .map(|s| format!(".{s}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .as_str(),
        );
    }

    let entries = collect_files(args.max_depth);

    if entries.is_empty() {
        LOGGER_INTERFACE.warning("No files found to process.");
        return Ok(());
    }

    let progress = Arc::new(Mutex::new(ProgressBar::new(entries.len() as u64)));
    let out_dir = PathBuf::from(args.output_dir.unwrap_or_else(|| "sorted".to_string()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let skipped = Arc::new(AtomicU64::new(0));

    if let Err(e) = create_dir_all(&out_dir) {
        LOGGER_INTERFACE.error(
            format!(
                "Failed to create output directory '{}': {}",
                out_dir.to_str().unwrap(),
                e
            )
            .as_str(),
        );
        process::exit(1);
    }

    let operation = if args.mv { "moving" } else { "copying" };
    LOGGER_INTERFACE.info(
        format!(
            "Starting {} {} files to '{}'...",
            operation,
            entries.len(),
            out_dir.to_str().unwrap()
        )
        .as_str(),
    );

    let category_map = get_categories(&args.config).expect("Failed to fetch categories");

    if !category_map.is_empty() {
        LOGGER_INTERFACE.info("Loaded categories:");
        for (cat, exts) in &category_map {
            LOGGER_INTERFACE.info(format!("  {cat}: {exts:?}").as_str());
        }
    }

    entries.par_iter().for_each(|entry| {
        process_file(
            entry,
            out_dir.as_ref(),
            args.mv,
            &blacklist,
            &category_map,
            &errors,
            &skipped,
        );
        progress.lock().unwrap().inc(1);
    });

    progress.lock().unwrap().finish();

    if args.gen_html {
        if let Err(e) = gen_html_index(out_dir.as_path()) {
            LOGGER_INTERFACE.error(format!("Failed to generate html index: {e}").as_str());
        }
    }

    let skipped_count = skipped.load(Ordering::Relaxed);
    let processed_count = entries.len() as u64 - skipped_count;

    if let Ok(errors_vec) = errors.lock() {
        if !errors_vec.is_empty() {
            LOGGER_INTERFACE.error("Errors encountered during processing:");
            for error in errors_vec.iter() {
                LOGGER_INTERFACE.error(format!("  {error}").as_str());
            }
            LOGGER_INTERFACE
                .info(format!("Processing completed with {} errors.", errors_vec.len()).as_str());
        }
    }

    LOGGER_INTERFACE.info("Summary:");
    LOGGER_INTERFACE.info(format!("  Files processed: {processed_count}").as_str());
    if skipped_count > 0 {
        LOGGER_INTERFACE.info(format!("  Files skipped (blacklisted): {skipped_count}").as_str());
    }

    LOGGER_INTERFACE.info(format!("  Total files found: {}", entries.len()).as_str());

    if args.serve {
        LOGGER_INTERFACE.info("Serving at 'http://127.0.0.1:6969'");
        return HttpServer::new(|| {
            App::new().service(
                Files::new("/", Cli::parse().output_dir.unwrap_or("sorted".to_string()))
                    .show_files_listing()
                    .index_file("index.html"),
            )
        })
        .bind("127.0.0.1:6969")?
        .run()
        .await;
    }

    if args.notify {
        let operation = if args.mv { "moving" } else { "sorting" };
        send_finished_notif(operation);
    }

    Ok(())
}
