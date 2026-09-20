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
    let expected_checksum = args.checksum;

    for u in &args.urls {
        let trimmed = u.trim();
        if trimmed.starts_with("blob:") || (trimmed.contains("youtube.com") && trimmed.split('/').last().map_or(false, |s| s.len() == 36 && s.matches('-').count() == 4)) {
            eprintln!("\n[NOTICE] The URL entered is a browser-internal blob memory buffer.");
            eprintln!("Browser blob: URLs exist only in temporary browser memory and cannot be downloaded by external tools.");
            eprintln!("Please copy the standard video URL from your browser address bar (e.g. https://www.youtube.com/watch?v=... or https://youtu.be/...).");
            return Err("Cannot download browser-internal blob URL".into());
        }

        if hyperfetch_core::torrent::is_magnet_uri(trimmed) {
            match hyperfetch_core::torrent::parse_magnet_uri(trimmed) {
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
                    return Err(e.into());
                }
            }
        }

        let url = Url::parse(trimmed).map_err(|e| format!("Invalid URL '{}': {}", u, e))?;
        parsed_urls.push(url);
    }

    let media_preset = parse_media_preset(args.media_preset.as_deref());
    let browser_cookies = parse_browser_cookie(args.cookies_from_browser.as_deref());

    let options = DownloadOptions {
        num_connections: args.connections,
        base_chunk_size: args.chunk_size_mb * 1024 * 1024,
        min_steal_threshold: 1024 * 1024,
        output_path: args.output,
        expected_checksum,
        cookies_path: args.load_cookies,
        auth_header: args.header,
        proxy: args.proxy,
        media_preset,
        browser_cookies,
    };

    let engine = DownloadEngine::new(parsed_urls, options);
    println!("Endo's Unified Downloader v0.1.0");
    println!("Probing mirrors and preparing dynamic chunk pipeline...");

    execute_download(engine).await;
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
