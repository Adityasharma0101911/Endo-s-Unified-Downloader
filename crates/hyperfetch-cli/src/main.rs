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

    /// Read URLs from file (one download per line, supports multiple mirrors per line, # for comments)
    #[arg(short = 'i', long = "input-file")]
    input_file: Option<PathBuf>,

    /// Number of concurrent connections/workers
    #[arg(short = 's', long = "split", default_value_t = 16)]
    connections: usize,

    /// Base chunk size in MB
    #[arg(short = 'c', long = "chunk-size", default_value_t = 4)]
    chunk_size_mb: u64,

    /// Output file path or directory
    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,

    /// Target directory for downloaded files (alias for -o if directory)
    #[arg(short = 'd', long = "dir")]
    dir: Option<PathBuf>,

    /// Quiet mode: suppress interactive progress bar for headless servers and cron jobs
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,

    /// Expected file checksum (sha256:..., md5:..., blake3:..., or hex)
    #[arg(long = "checksum")]
    checksum: Option<String>,

    /// Path to Netscape cookies.txt file
    #[arg(long = "load-cookies")]
    load_cookies: Option<PathBuf>,

    /// Custom authorization header (e.g. "Bearer <token>")
    #[arg(long = "header")]
    header: Option<String>,

    /// Proxy server URL (e.g. "http://127.0.0.1:8080" or "socks5://127.0.0.1:1080")
    #[arg(long = "proxy")]
    proxy: Option<String>,

    /// Media quality preset: "best", "1080p", "720p", "mp3", "m4a"
    #[arg(long = "media-preset")]
    media_preset: Option<String>,

    /// Extract cookies from browser: "chrome", "edge", "firefox", "brave", "opera", "vivaldi"
    #[arg(long = "cookies-from-browser")]
    cookies_from_browser: Option<String>,

    /// Concurrent fragment downloads for media streams (1-32)
    #[arg(long = "concurrent-fragments", default_value_t = 8)]
    concurrent_fragments: usize,

    /// Display past download history
    #[arg(long = "history")]
    history: bool,

    /// Verify chunk and build integrity of a local file
    #[arg(long = "verify")]
    verify: Option<PathBuf>,

    /// Automatically repair missing chunks if verification detects gaps (requires URL or history)
    #[arg(long = "repair")]
    repair: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.history {
        let manager = hyperfetch_core::history::DownloadHistoryManager::load();
        let entries = manager.entries();
        if entries.is_empty() {
            println!("No past downloads found in history.");
        } else {
            println!("\n{:<30} {:<14} {:<15} {:<40}", "FILE NAME", "SIZE", "STATUS", "URL");
            println!("{:-<100}", "");
            for entry in entries {
                let status_str = match entry.status {
                    hyperfetch_core::history::HistoryStatus::Completed => "Completed",
                    hyperfetch_core::history::HistoryStatus::Failed(_) => "Failed",
                    hyperfetch_core::history::HistoryStatus::Cancelled => "Cancelled",
                };
                let url_str = entry.urls.first().map(|s| s.as_str()).unwrap_or("");
                let size_str = format!("{:.2} MB", entry.file_size as f64 / (1024.0 * 1024.0));
                println!("{:<30} {:<14} {:<15} {:<40}", entry.file_name, size_str, status_str, url_str);
            }
            println!();
        }
        return Ok(());
    }

    if let Some(ref verify_path) = args.verify {
        println!("\nVerifying build file: {:?}", verify_path);
        let res = hyperfetch_core::verify::verify_build_file(verify_path, None, args.checksum.as_deref())
            .map_err(|e| Box::<dyn std::error::Error>::from(e))?;
        println!("Status: {}", res.status_message);
        println!("File Size on Disk: {} bytes", res.actual_size);
        if let Some(exp) = res.expected_size {
            println!("Expected File Size: {} bytes", exp);
        }
        println!("Missing / Incomplete Chunks: {}", res.missing_ranges.len());

        if !res.is_complete && args.repair {
            println!("\nAttempting automatic chunk repair...");
            let history = hyperfetch_core::history::DownloadHistoryManager::load();
            let urls: Vec<url::Url> = if !args.urls.is_empty() {
                args.urls.iter().filter_map(|u| url::Url::parse(u).ok()).collect()
            } else {
                history.entries()
                    .iter()
                    .find(|e| e.file_path == *verify_path || e.file_name == verify_path.file_name().unwrap_or_default().to_string_lossy())
                    .map(|e| e.urls.iter().filter_map(|u| url::Url::parse(u).ok()).collect())
                    .unwrap_or_default()
            };

            if urls.is_empty() {
                println!("Error: No download URL provided or found in history for repair.");
            } else {
                use std::io::Write;
                let total_size = res.expected_size.unwrap_or(res.actual_size);
                hyperfetch_core::verify::repair_missing_ranges(
                    verify_path,
                    total_size,
                    &res.missing_ranges,
                    &urls,
                    None,
                    |cur, tot| {
                        print!("\rRepaired {} / {} bytes ({:.1}%)", cur, tot, (cur as f64 / tot as f64) * 100.0);
                        let _ = std::io::stdout().flush();
                    },
                ).await.map_err(|e| Box::<dyn std::error::Error>::from(e))?;
                println!("\nBuild chunk repair successful! All chunks verified.");
            }
        }
        return Ok(());
    }

    if args.urls.is_empty() && args.input_file.is_none() {
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

    let default_download_dir = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
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
            if u.starts_with("blob:") || (u.contains("youtube.com") && u.split('/').last().map_or(false, |s| s.len() == 36 && s.matches('-').count() == 4)) {
                println!("\n[NOTICE] The URL entered is a browser-internal blob memory buffer.");
                println!("Browser blob: URLs exist only in temporary browser memory and cannot be downloaded by external tools.");
                println!("Please copy the standard video URL from your browser address bar (e.g. https://www.youtube.com/watch?v=... or https://youtu.be/...).");
                has_error = true;
                break;
            }

            if hyperfetch_core::torrent::is_magnet_uri(u) {
                match hyperfetch_core::torrent::parse_magnet_uri(u) {
                    Ok(magnet) => {
                        println!("[MAGNET] Ingested magnet URI: {}", magnet.info_hash);
                        if let Some(ref dn) = magnet.display_name {
                            println!("         Name: {}", dn);
                        }
                        if !magnet.web_seeds.is_empty() {
                            println!("         Discovered {} web seed mirror(s) for HTTP acceleration", magnet.web_seeds.len());
                            parsed_urls.extend(magnet.web_seeds);
                            continue;
                        }
                    }
                    Err(e) => {
                        eprintln!("[ERROR] Invalid magnet URI: {}", e);
                        has_error = true;
                        break;
                    }
                }
            }

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
            ..Default::default()
        };

        println!("\nProbing mirrors and initializing chunk pipeline...");
        let engine = DownloadEngine::new(parsed_urls, options);
        let _ = execute_download(engine, false).await;

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
    let mut tasks: Vec<Vec<String>> = Vec::new();

    // 1. Ingest batch file if provided
    if let Some(ref input_path) = args.input_file {
        let content = std::fs::read_to_string(input_path)
            .map_err(|e| format!("Failed to read input file {:?}: {}", input_path, e))?;
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                let mirrors: Vec<String> = trimmed.split_whitespace().map(|s| s.to_string()).collect();
                if !mirrors.is_empty() {
                    tasks.push(mirrors);
                }
            }
        }
    }

    // 2. Ingest command-line URLs if provided
    if !args.urls.is_empty() {
        tasks.push(args.urls.clone());
    }

    if tasks.is_empty() {
        println!("No valid download URLs provided.");
        return Ok(());
    }

    let output_target = args.output.clone().or_else(|| args.dir.clone());
    let total_tasks = tasks.len();
    let mut failed_tasks = 0;

    for (task_idx, task_urls) in tasks.into_iter().enumerate() {
        if total_tasks > 1 && !args.quiet {
            println!("\n=======================================================");
            println!("   [Task {}/{}] Processing download", task_idx + 1, total_tasks);
            println!("=======================================================");
        }

        let mut parsed_urls = Vec::new();
        let mut task_error = false;

        for u in &task_urls {
            let trimmed = u.trim();
            if trimmed.starts_with("blob:") || (trimmed.contains("youtube.com") && trimmed.split('/').last().map_or(false, |s| s.len() == 36 && s.matches('-').count() == 4)) {
                eprintln!("\n[NOTICE] The URL entered is a browser-internal blob memory buffer.");
                eprintln!("Browser blob: URLs exist only in temporary browser memory and cannot be downloaded by external tools.");
                task_error = true;
                break;
            }

            if hyperfetch_core::torrent::is_magnet_uri(trimmed) {
                match hyperfetch_core::torrent::parse_magnet_uri(trimmed) {
                    Ok(magnet) => {
                        if !args.quiet {
                            println!("[MAGNET] Ingested magnet URI: {}", magnet.info_hash);
                            if let Some(ref dn) = magnet.display_name {
                                println!("         Name: {}", dn);
                            }
                        }
                        if !magnet.web_seeds.is_empty() {
                            if !args.quiet {
                                println!("         Discovered {} web seed mirror(s) for HTTP acceleration", magnet.web_seeds.len());
                            }
                            parsed_urls.extend(magnet.web_seeds);
                            continue;
                        }
                    }
                    Err(e) => {
                        eprintln!("[ERROR] Invalid magnet URI: {}", e);
                        task_error = true;
                        break;
                    }
                }
            }

            match Url::parse(trimmed) {
                Ok(url) => parsed_urls.push(url),
                Err(e) => {
                    eprintln!("[ERROR] Invalid URL '{}': {}", u, e);
                    task_error = true;
                    break;
                }
            }
        }

        if task_error || parsed_urls.is_empty() {
            failed_tasks += 1;
            continue;
        }

        let media_preset = parse_media_preset(args.media_preset.as_deref());
        let browser_cookies = parse_browser_cookie(args.cookies_from_browser.as_deref());

        let options = DownloadOptions {
            num_connections: args.connections,
            base_chunk_size: args.chunk_size_mb * 1024 * 1024,
            min_steal_threshold: 1024 * 1024,
            output_path: output_target.clone(),
            expected_checksum: args.checksum.clone(),
            cookies_path: args.load_cookies.clone(),
            auth_header: args.header.clone(),
            proxy: args.proxy.clone(),
            media_preset,
            browser_cookies,
            ..Default::default()
        };

        let engine = DownloadEngine::new(parsed_urls, options);
        if !args.quiet && total_tasks == 1 {
            println!("Endo's Unified Downloader v0.1.0");
            println!("Probing mirrors and preparing dynamic chunk pipeline...");
        }

        if execute_download(engine, args.quiet).await.is_err() {
            failed_tasks += 1;
        }
    }

    if failed_tasks > 0 {
        return Err(format!("{} of {} download tasks failed or were interrupted", failed_tasks, total_tasks).into());
    }

    Ok(())
}

fn parse_media_preset(preset: Option<&str>) -> Option<hyperfetch_core::media::MediaQualityPreset> {
    match preset?.to_ascii_lowercase().as_str() {
        "best" => Some(hyperfetch_core::media::MediaQualityPreset::BestVideoAudio),
        "1080p" | "1080" | "fhd" => Some(hyperfetch_core::media::MediaQualityPreset::Fhd1080p),
        "720p" | "720" | "hd" => Some(hyperfetch_core::media::MediaQualityPreset::Hd720p),
        "mp3" => Some(hyperfetch_core::media::MediaQualityPreset::AudioMp3),
        "m4a" | "aac" => Some(hyperfetch_core::media::MediaQualityPreset::AudioM4a),
        custom => Some(hyperfetch_core::media::MediaQualityPreset::Custom(custom.to_string())),
    }
}

fn parse_browser_cookie(browser: Option<&str>) -> Option<hyperfetch_core::media::BrowserCookieSource> {
    match browser?.to_ascii_lowercase().as_str() {
        "chrome" => Some(hyperfetch_core::media::BrowserCookieSource::Chrome),
        "edge" => Some(hyperfetch_core::media::BrowserCookieSource::Edge),
        "firefox" => Some(hyperfetch_core::media::BrowserCookieSource::Firefox),
        "brave" => Some(hyperfetch_core::media::BrowserCookieSource::Brave),
        "opera" => Some(hyperfetch_core::media::BrowserCookieSource::Opera),
        "vivaldi" => Some(hyperfetch_core::media::BrowserCookieSource::Vivaldi),
        _ => None,
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn execute_download(engine: DownloadEngine, quiet: bool) -> Result<PathBuf, String> {
    let (snapshot_tx, mut snapshot_rx) = broadcast::channel::<EngineSnapshot>(64);

    let pb = if quiet {
        None
    } else {
        let bar = ProgressBar::new(100);
        bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})")
                .unwrap()
                .progress_chars("=>-"),
        );
        Some(bar)
    };

    let pb_clone = pb.clone();
    let monitor_handle = tokio::spawn(async move {
        loop {
            match snapshot_rx.recv().await {
                Ok(snapshot) => {
                    if let Some(ref bar) = pb_clone {
                        bar.set_length(snapshot.total_bytes);
                        bar.set_position(snapshot.downloaded_bytes);
                    }
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

    let cancel_engine = engine.clone();
    let result = tokio::select! {
        res = engine.run(Some(snapshot_tx)) => res,
        _ = wait_for_shutdown_signal() => {
            cancel_engine.cancel();
            if let Some(ref bar) = pb {
                bar.abandon_with_message("Paused");
            }
            eprintln!("\n[PAUSED] Download interrupted by signal (SIGINT/SIGTERM). State preserved for safe resume.");
            return Err("Download interrupted by signal".to_string());
        }
    };

    let _ = monitor_handle.await;

    match result {
        Ok(path) => {
            if let Some(ref bar) = pb {
                bar.finish_with_message("Complete");
            }
            if !quiet {
                println!("\n[OK] Downloaded to: {}", path.display());
            }
            Ok(path)
        }
        Err(err) => {
            if let Some(ref bar) = pb {
                bar.abandon_with_message("Failed");
            }
            eprintln!("\n[ERROR] Download failed: {}", err);
            Err(err)
        }
    }
}
