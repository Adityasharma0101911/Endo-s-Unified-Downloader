use std::path::PathBuf;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::sync::broadcast;
use url::Url;

use hyperfetch_core::engine::{DownloadEngine, DownloadOptions, EngineSnapshot};

#[derive(Parser, Debug)]
#[command(name = "Endo's Unified Downloader", author, version, about = "High-speed unified download accelerator & multi-source ingestion engine", long_about = None)]
struct Args {
    /// URLs to download (mirrors/sources for racing). If omitted, interactive UI mode is launched.
    #[arg(num_args = 0..)]
    urls: Vec<String>,

    /// Number of concurrent connections/workers
    #[arg(short = 's', long = "split", default_value_t = 16)]
    connections: usize,

    /// Base chunk size in MB
    #[arg(short = 'c', long = "chunk-size", default_value_t = 4)]
    chunk_size_mb: u64,

    /// Output file path or directory
    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.urls.is_empty() {
        run_interactive_ui().await?;
    } else {
        run_cli_download(args).await?;
    }

    Ok(())
}

async fn run_interactive_ui() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{self, Write};

    println!("\n===================================================================");
    println!("   Endo's Unified Downloader - High-Speed Ingestion Engine");
    println!("===================================================================");

    let default_download_dir = std::env::var("USERPROFILE")
        .map(|p| PathBuf::from(p).join("Downloads"))
        .unwrap_or_else(|_| PathBuf::from("."));

    loop {
        println!("\nEnter download URL(s) (space-separated for multiple mirrors),");
        print!("or press Enter to exit:\n> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim();

        if trimmed.is_empty() {
            println!("Exiting Endo's Unified Downloader. Goodbye.");
            break;
        }

        let raw_urls: Vec<&str> = trimmed.split_whitespace().collect();
        let mut parsed_urls = Vec::new();
        let mut has_error = false;

        for u in raw_urls {
            match Url::parse(u) {
                Ok(url) => parsed_urls.push(url),
                Err(e) => {
                    eprintln!("[ERROR] Invalid URL '{}': {}", u, e);
                    has_error = true;
                    break;
                }
            }
        }

        if has_error || parsed_urls.is_empty() {
            continue;
        }

        // Ask for connection count
        print!("Concurrent connections [default: 16]: ");
        io::stdout().flush()?;
        let mut conn_input = String::new();
        io::stdin().read_line(&mut conn_input)?;
        let connections = conn_input.trim().parse::<usize>().unwrap_or(16).max(1);

        // Ask for save folder
        print!("Save directory [default: {}]: ", default_download_dir.display());
        io::stdout().flush()?;
        let mut dir_input = String::new();
        io::stdin().read_line(&mut dir_input)?;
        let target_dir = if dir_input.trim().is_empty() {
            default_download_dir.clone()
        } else {
            PathBuf::from(dir_input.trim())
        };

        let options = DownloadOptions {
            num_connections: connections,
            base_chunk_size: 4 * 1024 * 1024,
            min_steal_threshold: 1024 * 1024,
            output_path: Some(target_dir),
        };

        println!("\nProbing mirrors and initializing chunk pipeline...");
        let engine = DownloadEngine::new(parsed_urls, options);
        execute_download(engine).await;

        println!("\n-------------------------------------------------------------------");
        print!("Download another file? [y/N]: ");
        io::stdout().flush()?;
        let mut again = String::new();
        io::stdin().read_line(&mut again)?;
        if !again.trim().eq_ignore_ascii_case("y") {
            println!("Goodbye.");
            break;
        }
    }

    Ok(())
}

async fn run_cli_download(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut parsed_urls = Vec::new();
    for u in &args.urls {
        let url = Url::parse(u).map_err(|e| format!("Invalid URL '{}': {}", u, e))?;
        parsed_urls.push(url);
    }

    let options = DownloadOptions {
        num_connections: args.connections,
        base_chunk_size: args.chunk_size_mb * 1024 * 1024,
        min_steal_threshold: 1024 * 1024,
        output_path: args.output,
    };

    let engine = DownloadEngine::new(parsed_urls, options);
    println!("Endo's Unified Downloader v0.1.0");
    println!("Probing mirrors and preparing dynamic chunk pipeline...");

    execute_download(engine).await;
    Ok(())
}

async fn execute_download(engine: DownloadEngine) {
    let (snapshot_tx, mut snapshot_rx) = broadcast::channel::<EngineSnapshot>(64);

    let pb = ProgressBar::new(100);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})")
            .unwrap()
            .progress_chars("=>-"),
    );

    let pb_clone = pb.clone();
    let monitor_handle = tokio::spawn(async move {
        loop {
            match snapshot_rx.recv().await {
                Ok(snapshot) => {
                    pb_clone.set_length(snapshot.total_bytes);
                    pb_clone.set_position(snapshot.downloaded_bytes);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    let result = engine.run(Some(snapshot_tx)).await;
    let _ = monitor_handle.await;

    match result {
        Ok(path) => {
            pb.finish_with_message("Complete");
            println!("\n[OK] Downloaded to: {}", path.display());
        }
        Err(err) => {
            pb.abandon_with_message("Failed");
            eprintln!("\n[ERROR] Download failed: {}", err);
        }
    }
}
