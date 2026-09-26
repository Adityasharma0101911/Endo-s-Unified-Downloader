mod cli;
mod download;
mod ingest;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use hyperfetch_core::engine::DownloadOptions;
use hyperfetch_core::history::{DownloadHistoryManager, HistoryEntry, HistoryStatus};
use hyperfetch_core::state::DownloadState;
use hyperfetch_core::verify::{self, BuildVerificationResult};
use indicatif::HumanBytes;
use url::Url;

use cli::Args;
use download::{run_jobs, stop_requested, truncate, Job, Shutdown, Ui, EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use ingest::{batch_lines, decode_text, http_url, ingest, Task};

fn main() {
    let args = Args::parse();
    let code = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime.block_on(app(args)),
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {}", e);
            EXIT_FAILED
        }
    };
    let _ = std::io::stdout().flush();
    // Exit without waiting for a blocking stdin read that may still be pending.
    std::process::exit(code);
}

async fn app(args: Args) -> i32 {
    let ui = Ui::new(args.quiet);
    init_tracing(args.verbose, &ui);
    let shutdown = Shutdown::install();

    if args.history {
        return show_history().await;
    }
    if let Some(path) = &args.verify {
        return verify_file(&args, path, &ui, &shutdown).await;
    }
    let http = match descriptor_client(&args) {
        Ok(client) => client,
        Err(e) => return usage(&e),
    };
    if args.urls.is_empty() && args.input_file.is_none() {
        interactive(&args, &ui, &shutdown, &http).await
    } else {
        batch(&args, &ui, &shutdown, &http).await
    }
}

fn usage(message: &str) -> i32 {
    eprintln!("error: {}", message);
    EXIT_USAGE
}

/// The exit code once every download has run.
fn exit_code(signal: Option<i32>, failed: usize) -> i32 {
    signal.unwrap_or(if failed > 0 { EXIT_FAILED } else { EXIT_OK })
}

/// Engine warnings go to stderr (above the progress bars); -v adds info/debug/trace, RUST_LOG wins.
fn init_tracing(verbose: u8, ui: &Ui) {
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::prelude::*;

    let level = match verbose {
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.trim().is_empty() => spec.parse::<Targets>().unwrap_or_else(|e| {
            eprintln!("warning: ignoring invalid RUST_LOG '{}': {}", spec, e);
            Targets::new().with_default(level)
        }),
        _ => Targets::new().with_default(level),
    };
    let writer = ui.log_writer();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.clone())
                .without_time()
                .with_ansi(std::io::stderr().is_terminal())
                .with_target(verbose >= 2),
        )
        .with(filter)
        .init();
}

/// Client for fetching remote .metalink/.torrent documents.
fn descriptor_client(args: &Args) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("Endos-Unified-Downloader/", env!("CARGO_PKG_VERSION")));
    if let Some(proxy) = &args.proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy.as_str()).map_err(|e| format!("invalid proxy: {}", e))?);
    }
    builder.build().map_err(|e| format!("cannot create HTTP client: {}", e))
}

/// Rejects options that name a single file when there are several downloads.
fn check_single_file_options(output: Option<&Path>, has_checksum: bool, tasks: usize) -> Result<(), String> {
    if let Some(output) = output {
        if output.is_dir() {
            return Err(format!("-o expects a file path, but {} is a directory; use -d DIR", output.display()));
        }
        if tasks > 1 {
            return Err(format!("-o names one file, but the input has {} downloads; use -d DIR instead", tasks));
        }
    }
    if has_checksum && tasks > 1 {
        return Err(format!("--checksum applies to one file, but the input has {} downloads", tasks));
    }
    Ok(())
}

/// The download directory, created if missing.
async fn prepare_dir(dir: &Path) -> Result<PathBuf, String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("cannot create directory {}: {}", dir.display(), e))?;
    Ok(dir.to_path_buf())
}

fn job(args: &Args, connections: u64, dir: &Path, task: Task) -> Job {
    let label = task.label();
    let output = match (&args.output, &task.name) {
        (Some(file), _) => dir.join(file),
        (None, Some(name)) => dir.join(name),
        (None, None) => dir.to_path_buf(),
    };
    let media = args.media_preset.is_some() || task.urls.iter().any(hyperfetch_core::media::is_supported_media_site);
    let options = DownloadOptions {
        num_connections: if media { args.concurrent_fragments } else { connections } as usize,
        base_chunk_size: args.chunk_size_mb * 1024 * 1024,
        output_path: Some(output),
        expected_checksum: args.checksum.clone().or(task.checksum),
        cookies_path: args.load_cookies.clone(),
        auth_header: args.auth_header.clone(),
        proxy: args.proxy.clone(),
        media_preset: args.media_preset.clone(),
        browser_cookies: args.cookies_from_browser.map(Into::into),
        max_speed: args.max_speed.filter(|&s| s > 0),
        max_retries: args.max_retries,
        stall_timeout_secs: args.stall_timeout,
        ..Default::default()
    };
    Job { label, urls: task.urls, options }
}

async fn read_input(path: &Path) -> Result<String, String> {
    let bytes = if path == Path::new("-") {
        tokio::task::spawn_blocking(|| {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf).map(|_| buf)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("cannot read stdin: {}", e))?
    } else {
        tokio::fs::read(path).await.map_err(|e| format!("cannot read {}: {}", path.display(), e))?
    };
    decode_text(&bytes).map_err(|e| format!("{}: {}", path.display(), e))
}

async fn batch(args: &Args, ui: &Ui, shutdown: &Shutdown, http: &reqwest::Client) -> i32 {
    let mut inputs: Vec<Vec<String>> = Vec::new();
    if let Some(path) = &args.input_file {
        match read_input(path).await {
            Ok(text) => inputs.extend(batch_lines(&text).map(|l| l.split_whitespace().map(String::from).collect())),
            Err(e) => return usage(&e),
        }
    }
    if !args.urls.is_empty() {
        inputs.push(args.urls.clone());
    }

    let mut tasks = Vec::new();
    let mut failed = 0;
    for input in &inputs {
        let tokens: Vec<&str> = input.iter().map(String::as_str).collect();
        match ingest(&tokens, http).await {
            Ok(found) => tasks.extend(found),
            Err(e) => {
                ui.error(&format!("[FAILED] {}: {}", truncate(&tokens.join(" "), 60), e));
                failed += 1;
            }
        }
    }
    if let Err(e) = check_single_file_options(args.output.as_deref(), args.checksum.is_some(), tasks.len()) {
        return usage(&e);
    }
    if tasks.is_empty() {
        return if failed > 0 { EXIT_FAILED } else { usage("the input contains no downloads") };
    }
    let dir = match prepare_dir(args.dir.as_deref().unwrap_or(Path::new("."))).await {
        Ok(dir) => dir,
        Err(e) => return usage(&e),
    };

    let jobs = tasks.into_iter().map(|t| job(args, args.connections, &dir, t)).collect();
    failed += run_jobs(jobs, args.jobs as usize, ui, shutdown).await;
    exit_code(shutdown.requested(), failed)
}

/// Prints `message` and reads one trimmed line; None at end of input.
async fn prompt(message: String) -> Option<String> {
    print!("{}", message);
    let _ = std::io::stdout().flush();
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim().to_string()),
        }
    })
    .await
    .ok()
    .flatten()
}

fn default_download_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Prompt loop used when no URL is given. Command-line options apply to every download.
async fn interactive(args: &Args, ui: &Ui, shutdown: &Shutdown, http: &reqwest::Client) -> i32 {
    println!("Endo's Unified Downloader {}", env!("CARGO_PKG_VERSION"));
    let default_dir = args.dir.clone().unwrap_or_else(default_download_dir);
    let mut failed = 0;
    loop {
        let Some(input) = prompt("\nURL(s) of one file (space-separated mirrors), magnet, metalink or torrent;\nEnter to exit:\n> ".to_string()).await
        else {
            break;
        };
        if input.is_empty() {
            break;
        }
        let tokens: Vec<&str> = input.split_whitespace().collect();
        let tasks = match ingest(&tokens, http).await {
            Ok(tasks) => tasks,
            Err(e) => {
                eprintln!("[ERROR] {}", e);
                continue;
            }
        };
        if let Err(e) = check_single_file_options(args.output.as_deref(), args.checksum.is_some(), tasks.len()) {
            eprintln!("[ERROR] {}", e);
            continue;
        }

        let answer = prompt(format!("Connections per download [{}]: ", args.connections)).await.unwrap_or_default();
        let connections = match answer.parse::<u64>() {
            Ok(n @ 1..=64) => n,
            _ if answer.is_empty() => args.connections,
            _ => {
                println!("Using {} (enter a number from 1 to 64)", args.connections);
                args.connections
            }
        };
        let answer = prompt(format!("Save directory [{}]: ", default_dir.display())).await.unwrap_or_default();
        let dir = match prepare_dir(if answer.is_empty() { &default_dir } else { Path::new(&answer) }).await {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("[ERROR] {}", e);
                continue;
            }
        };

        let jobs = tasks.into_iter().map(|t| job(args, connections, &dir, t)).collect();
        failed += run_jobs(jobs, args.jobs as usize, ui, shutdown).await;
        if let Some(code) = shutdown.requested() {
            return code;
        }
        let again = prompt("\nDownload another file? [y/N]: ".to_string()).await.unwrap_or_default();
        if !again.eq_ignore_ascii_case("y") {
            break;
        }
    }
    exit_code(None, failed)
}

fn history_row(entry: &HistoryEntry) -> String {
    let (status, note) = match &entry.status {
        HistoryStatus::Completed => ("Completed".to_string(), String::new()),
        HistoryStatus::Cancelled => (stopped_at(entry), String::new()),
        HistoryStatus::Failed(reason) => ("Failed".to_string(), truncate(reason, 60)),
    };
    let size = if entry.file_size > 0 { HumanBytes(entry.file_size).to_string() } else { "?".to_string() };
    let host = entry
        .urls
        .first()
        .and_then(|u| Url::parse(u).ok())
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "-".to_string());
    format!("{:<14} {:>11}  {:<40}  {:<24} {}", status, size, truncate(&entry.file_name, 40), truncate(&host, 24), note)
        .trim_end()
        .to_string()
}

/// "Stopped 42%": how far a stopped download got (its .part can be resumed or repaired).
fn stopped_at(entry: &HistoryEntry) -> String {
    match entry.downloaded_bytes.saturating_mul(100).checked_div(entry.file_size) {
        Some(percent) => format!("Stopped {}%", percent.min(100)),
        None => "Stopped".to_string(),
    }
}

async fn show_history() -> i32 {
    let loaded = tokio::task::spawn_blocking(|| {
        (DownloadHistoryManager::default_history_path(), DownloadHistoryManager::load().entries().to_vec())
    })
    .await;
    let Ok((path, entries)) = loaded else {
        eprintln!("error: cannot read the download history");
        return EXIT_FAILED;
    };
    if entries.is_empty() {
        println!("No downloads in history ({}).", path.display());
        return EXIT_OK;
    }
    println!("{:<14} {:>11}  {:<40}  {:<24} NOTE", "STATUS", "SIZE", "FILE", "HOST");
    for entry in &entries {
        println!("{}", history_row(entry));
    }
    println!("\n{} entries, newest first ({})", entries.len(), path.display());
    EXIT_OK
}

fn print_verification(res: &BuildVerificationResult) {
    println!("File:     {}", res.file_path.display());
    println!("Status:   {}", res.status_message);
    match res.expected_size {
        Some(expected) => println!("Size:     {} bytes on disk, {} expected", res.actual_size, expected),
        None => println!("Size:     {} bytes on disk, expected size unknown", res.actual_size),
    }
    if !res.missing_ranges.is_empty() {
        let bytes: u64 = res.missing_ranges.iter().map(|r| r.len()).sum();
        println!("Missing:  {} range(s), {}", res.missing_ranges.len(), HumanBytes(bytes));
    }
    match res.checksum_match {
        Some(true) => println!("Checksum: match"),
        Some(false) => println!("Checksum: MISMATCH"),
        None => {}
    }
}

async fn run_verify(path: PathBuf, checksum: Option<String>) -> Result<BuildVerificationResult, String> {
    tokio::task::spawn_blocking(move || verify::verify_build_file(&path, None, checksum.as_deref()))
        .await
        .map_err(|e| e.to_string())?
}

/// The final name of `<name>.part`, or the path itself.
fn final_path(path: &Path) -> PathBuf {
    if path.extension().is_some_and(|e| e == "part") {
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

/// Repair sources: URLs from the command line, else the mirrors in this file's resume state,
/// else the URLs history recorded for exactly this path.
fn repair_urls(explicit: &[String], data_path: &Path) -> Result<Vec<Url>, String> {
    let candidates: Vec<String> = if !explicit.is_empty() {
        explicit.to_vec()
    } else {
        let state = DownloadState::load_from_path(&DownloadState::state_file_path(data_path)).ok().flatten();
        match state.map(|s| s.mirrors).filter(|m| !m.is_empty()) {
            Some(mirrors) => mirrors,
            None => {
                let wanted = std::path::absolute(final_path(data_path)).map_err(|e| e.to_string())?;
                DownloadHistoryManager::load()
                    .entries()
                    .iter()
                    .find(|e| std::path::absolute(&e.file_path).is_ok_and(|p| p == wanted))
                    .map(|e| e.urls.clone())
                    .unwrap_or_default()
            }
        }
    };
    let mut urls = Vec::new();
    for candidate in &candidates {
        let url = http_url(candidate).ok_or_else(|| format!("'{}' is not an http(s) URL", candidate))?;
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    Ok(urls)
}

async fn verify_file(args: &Args, path: &Path, ui: &Ui, shutdown: &Shutdown) -> i32 {
    if !args.repair && !args.urls.is_empty() {
        return usage("URLs after --verify are only used together with --repair");
    }
    let res = match run_verify(path.to_path_buf(), args.checksum.clone()).await {
        Ok(res) => res,
        Err(e) => return usage(&e),
    };
    print_verification(&res);
    if res.is_complete {
        return EXIT_OK;
    }
    if !args.repair {
        return EXIT_USAGE;
    }
    if res.missing_ranges.is_empty() {
        eprintln!("Nothing to repair: missing ranges are unknown or the contents are wrong; download the file again.");
        return EXIT_USAGE;
    }
    let Some(total_size) = res.expected_size else {
        eprintln!("Cannot repair: the expected file size is unknown.");
        return EXIT_USAGE;
    };
    let urls = {
        let (explicit, data_path) = (args.urls.clone(), res.file_path.clone());
        match tokio::task::spawn_blocking(move || repair_urls(&explicit, &data_path)).await {
            Ok(Ok(urls)) if !urls.is_empty() => urls,
            Ok(Ok(_)) => {
                return usage(&format!(
                    "no download URL is known for {}; pass it: --verify FILE --repair URL",
                    final_path(&res.file_path).display()
                ))
            }
            Ok(Err(e)) => return usage(&e),
            Err(e) => return usage(&e.to_string()),
        }
    };

    println!("\nRepairing from {} mirror(s)...", urls.len());
    let cancel = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (cancel, mut stop) = (Arc::clone(&cancel), shutdown.subscribe());
        tokio::spawn(async move {
            stop_requested(&mut stop).await;
            cancel.store(true, Ordering::Relaxed);
        })
    };
    let bar = ui.byte_bar("repair", Some(0), None);
    let progress = bar.clone();
    let repaired = verify::repair_missing_ranges(
        &res.file_path,
        total_size,
        &res.missing_ranges,
        &urls,
        Some(cancel),
        move |done, total| {
            progress.set_length(total);
            progress.set_position(done);
        },
    )
    .await;
    watcher.abort();
    bar.finish_and_clear();

    if let Err(e) = repaired {
        if let Some(code) = shutdown.requested() {
            eprintln!("[STOPPED] {}; repaired ranges are saved, run the command again to continue", e);
            return code;
        }
        eprintln!("[FAILED] Repair: {}", e);
        return EXIT_FAILED;
    }

    // A repaired .part is renamed to its final name.
    let recheck = if res.file_path.exists() { res.file_path.clone() } else { final_path(&res.file_path) };
    println!("\nRepair finished; verifying again...");
    match run_verify(recheck, args.checksum.clone()).await {
        Ok(res) => {
            print_verification(&res);
            if res.is_complete { EXIT_OK } else { EXIT_USAGE }
        }
        Err(e) => usage(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(None, 0), 0);
        assert_eq!(exit_code(None, 3), 1);
        assert_eq!(exit_code(Some(130), 0), 130);
        assert_eq!(exit_code(Some(143), 2), 143);
    }

    #[test]
    fn single_file_options_need_a_single_task() {
        let file = Path::new("does-not-exist-hf-cli/out.bin");
        assert!(check_single_file_options(Some(file), true, 1).is_ok());
        assert!(check_single_file_options(Some(file), false, 2).unwrap_err().contains("-d DIR"));
        assert!(check_single_file_options(None, true, 2).unwrap_err().contains("--checksum"));
        assert!(check_single_file_options(Some(Path::new(".")), false, 1).unwrap_err().contains("directory"));
        assert!(check_single_file_options(None, false, 5).is_ok());
    }

    #[test]
    fn part_files_map_to_their_final_name() {
        assert_eq!(final_path(Path::new("dir/a.iso.part")), PathBuf::from("dir/a.iso"));
        assert_eq!(final_path(Path::new("dir/a.iso")), PathBuf::from("dir/a.iso"));
    }

    #[test]
    fn history_rows_are_compact() {
        let url = "https://cdn.example.com/very/long/path/file.iso?token=secret".to_string();
        let mut entry = HistoryEntry::new("x".repeat(60), PathBuf::from("/d/x"), 2048, vec![url]);
        entry.status = HistoryStatus::Failed("connection reset".to_string());
        entry.downloaded_bytes = 512;
        let row = history_row(&entry);
        assert!(row.starts_with("Failed "), "{}", row);
        assert!(row.contains("cdn.example.com") && !row.contains("secret"), "{}", row);
        assert!(row.contains(&format!("{}...", "x".repeat(37))), "{}", row);
        assert!(row.ends_with("connection reset"), "{}", row);

        entry.status = HistoryStatus::Cancelled;
        assert!(history_row(&entry).starts_with("Stopped 25%"));
        entry.file_size = 0;
        assert!(history_row(&entry).starts_with("Stopped "));
    }

    #[test]
    fn repair_prefers_explicit_urls_and_rejects_bad_ones() {
        let urls = repair_urls(&["https://a.example/f".to_string(), "https://a.example/f".to_string()], Path::new("f"));
        assert_eq!(urls.unwrap().len(), 1);
        assert!(repair_urls(&["file.bin".to_string()], Path::new("f")).is_err());
    }
}
