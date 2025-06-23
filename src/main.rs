use std::{
    collections::HashSet,
    fs::{self, *},
    io::*,
    path::Path,
    process,
    sync::{Arc, Mutex},
};

use clap::Parser;
use indicatif::ProgressBar;
use notify_rust::{Notification, Timeout};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use walkdir::WalkDir;

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
}

fn move_file<P: AsRef<Path>>(from: P, to: P) -> std::io::Result<()> {
    let from_path = from.as_ref();
    let to_path = to.as_ref();

    if !from_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "[ERROR]: Source file does not exist",
        ));
    }

    if let Some(parent) = to_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::rename(from_path, to_path)?;

    Ok(())
}

fn copy_file(source: &str, dest: &str, clean: bool) -> std::io::Result<()> {
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
            "`parmove` has finished {} the directory",
            operation
        ))
        .icon("vivaldi")
        .timeout(Timeout::Milliseconds(1000))
        .show()
    {
        eprintln!("Warning: Failed to display notification: {}", e);
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
        let content = std::fs::read_to_string(file_path)
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
            println!("Using {} threads for parallel processing", count);
        }
        None => {
            let default_threads = rayon::current_num_threads();
            println!(
                "Using {} threads for parallel processing (default)",
                default_threads
            );
        }
    }
    Ok(())
}

fn collect_files(
    max_depth: Option<usize>,
) -> std::result::Result<Vec<walkdir::DirEntry>, Box<dyn std::error::Error>> {
    let mut walker = WalkDir::new(".");

    // Configure depth limit
    if let Some(depth) = max_depth {
        walker = walker.max_depth(depth);
        match depth {
            0 => println!("Collecting files from current directory only"),
            1 => println!("Collecting files from current directory and immediate subdirectories"),
            _ => println!("Collecting files with maximum depth of {} levels", depth),
        }
    } else {
        println!("Collecting files from all subdirectories (unlimited depth)");
    }

    let mut entries = Vec::new();
    let mut directories_scanned = 0;

    for entry in walker {
        let item = match entry {
            Ok(item) => item,
            Err(e) => {
                eprintln!("Warning: Failed to read directory entry: {}", e);
                continue;
            }
        };

        if item.path().is_file() {
            entries.push(item);
        } else if item.path().is_dir() {
            directories_scanned += 1;
        }
    }

    println!(
        "Scanned {} directories, found {} files",
        directories_scanned,
        entries.len()
    );
    Ok(entries)
}

fn process_file(
    entry: &walkdir::DirEntry,
    out_dir: &str,
    use_move: bool,
    blacklist: &HashSet<String>,
    errors: &Arc<Mutex<Vec<String>>>,
    skipped: &Arc<Mutex<u64>>,
) {
    // Check if file is blacklisted
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
            let ext = ext.to_str().ok_or("Invalid extension encoding")?;
            let target_dir = Path::new(out_dir).join(ext);
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

    if let Err(e) = setup_thread_pool(args.threads) {
        eprintln!("Error configuring threads: {}", e);
        process::exit(1);
    }

    let blacklist = match load_blacklist(&args) {
        Ok(blacklist) => blacklist,
        Err(e) => {
            eprintln!("Error loading blacklist: {}", e);
            process::exit(1);
        }
    };

    if !blacklist.is_empty() {
        println!(
            "Blacklisted extensions: {}",
            blacklist
                .iter()
                .map(|s| format!(".{}", s))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let entries = match collect_files(args.max_depth) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Error collecting files: {}", e);
            process::exit(1);
        }
    };

    if entries.is_empty() {
        println!("No files found to process.");
        return;
    }

    let progress = ProgressBar::new(entries.len() as u64);
    let out_dir = args.output_dir.unwrap_or_else(|| "sorted".to_string());
    let errors = Arc::new(Mutex::new(Vec::new()));
    let skipped = Arc::new(Mutex::new(0u64));

    // Create output directory
    if let Err(e) = create_dir_all(&out_dir) {
        eprintln!(
            "Error: Failed to create output directory '{}': {}",
            out_dir, e
        );
        process::exit(1);
    }

    let operation = if args.mv { "moving" } else { "copying" };
    println!(
        "Starting {} {} files to '{}'...",
        operation,
        entries.len(),
        out_dir
    );

    entries.par_iter().for_each(|entry| {
        process_file(entry, &out_dir, args.mv, &blacklist, &errors, &skipped);
        progress.inc(1);
    });

    progress.finish();

    let skipped_count = match skipped.lock() {
        Ok(count) => *count,
        Err(_) => {
            eprintln!("Warning: Failed to get skipped file count");
            0
        }
    };
    let processed_count = entries.len() as u64 - skipped_count;

    if let Ok(errors_vec) = errors.lock() {
        if !errors_vec.is_empty() {
            eprintln!("\nErrors encountered during processing:");
            for error in errors_vec.iter() {
                eprintln!("  {}", error);
            }
            eprintln!("\nProcessing completed with {} errors.", errors_vec.len());
        }
    }

    println!("\nSummary:");
    println!("  Files processed: {}", processed_count);
    if skipped_count > 0 {
        println!("  Files skipped (blacklisted): {}", skipped_count);
    }
    println!("  Total files found: {}", entries.len());

    if args.notify {
        let operation = if args.mv { "moving" } else { "sorting" };
        send_finished_notif(operation);
    }
}
