mod cli;
mod download;
mod ingest;

use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use hyperfetch_core::engine::DownloadOptions;
use hyperfetch_core::history::{DownloadHistoryManager, HistoryEntry, HistoryStatus};
use hyperfetch_core::resolver::SmartResolver;
use hyperfetch_core::state::DownloadState;
use hyperfetch_core::verify::{self, BuildVerificationResult};
use indicatif::HumanBytes;
use url::Url;

use cli::Args;
use download::{
    printable, run_jobs, stderr_line, stdout_line, stop_requested, truncate, Job, Shutdown, Ui, EXIT_FAILED, EXIT_OK,
    EXIT_USAGE,
};
use ingest::{batch_lines, decode_text, http_url, ingest, input_tokens, Task};

fn main() {
    let args = Args::parse();
    let code = run_detached(app(args)).unwrap_or_else(|e| {
        stderr_line(&format!("error: cannot start the async runtime: {}", e));
        EXIT_FAILED
    });
    let _ = std::io::stdout().flush();
    // Exit without waiting for a blocking stdin read that may still be pending.
    std::process::exit(code);
}

/// Runs `app` to completion, then shuts the runtime down without waiting for blocking work that
/// was abandoned: a final hash or disk sync still running past the stop timeout, or a stdin read.
/// Dropping the runtime normally would wait for it, with the signal listener already gone.
fn run_detached(app: impl Future<Output = i32>) -> std::io::Result<i32> {
    let runtime = tokio::runtime::Runtime::new()?;
    let code = runtime.block_on(app);
    runtime.shutdown_background();
    Ok(code)
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
    stderr_line(&format!("error: {}", message));
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
            stderr_line(&format!("warning: ignoring invalid RUST_LOG '{}': {}", spec, e));
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
    if output.is_some() && tasks > 1 {
        return Err(format!("-o names one file, but the input has {} downloads; use -d DIR instead", tasks));
    }
    if has_checksum && tasks > 1 {
        return Err(format!("--checksum applies to one file, but the input has {} downloads", tasks));
    }
    Ok(())
}

/// Rejects an -o the engine would take as a directory: one ending with a path separator, or one
/// that is an existing directory where it will be written (under `dir`).
async fn check_output_file(dir: &Path, output: Option<&Path>) -> Result<(), String> {
    let Some(output) = output else { return Ok(()) };
    let path = dir.join(output);
    let trailing_separator =
        output.as_os_str().as_encoded_bytes().last().is_some_and(|&b| std::path::is_separator(b as char));
    if trailing_separator || tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) {
        return Err(format!("-o expects a file path, but {} is a directory; use -d DIR", path.display()));
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
        // The trailing separator keeps it a directory even if it disappears mid-batch; a
        // download then fails instead of being saved as a file with the directory's name.
        (None, None) => dir.join(""),
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
            Ok(text) => {
                for line in batch_lines(&text) {
                    inputs.push(input_tokens(line).await);
                }
            }
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
        // An empty queue is the idle state of a queue file (and of the systemd service), not an error.
        if failed == 0 && !ui.quiet() {
            stderr_line("Nothing to download: the input lists no downloads.");
        }
        return exit_code(None, failed);
    }
    let dir = match prepare_dir(args.dir.as_deref().unwrap_or(Path::new("."))).await {
        Ok(dir) => dir,
        Err(e) => return usage(&e),
    };
    if let Err(e) = check_output_file(&dir, args.output.as_deref()).await {
        return usage(&e);
    }

    let jobs = tasks.into_iter().map(|t| job(args, args.connections, &dir, t)).collect();
    failed += run_jobs(jobs, args.jobs as usize, ui, shutdown).await;
    exit_code(shutdown.requested(), failed)
}

/// Prints `message` and reads one trimmed line; None at end of input.
async fn prompt(message: String) -> Option<String> {
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "{}", message).and_then(|_| stdout.flush());
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
    stdout_line(&format!("Endo's Unified Downloader {}", env!("CARGO_PKG_VERSION")));
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
        let tokens = input_tokens(&input).await;
        let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
        let tasks = match ingest(&tokens, http).await {
            Ok(tasks) => tasks,
            Err(e) => {
                stderr_line(&format!("[ERROR] {}", e));
                continue;
            }
        };
        if let Err(e) = check_single_file_options(args.output.as_deref(), args.checksum.is_some(), tasks.len()) {
            stderr_line(&format!("[ERROR] {}", e));
            continue;
        }

        let answer = prompt(format!("Connections per download [{}]: ", args.connections)).await.unwrap_or_default();
        let connections = match answer.parse::<u64>() {
            Ok(n @ 1..=64) => n,
            _ if answer.is_empty() => args.connections,
            _ => {
                stdout_line(&format!("Using {} (enter a number from 1 to 64)", args.connections));
                args.connections
            }
        };
        let answer = prompt(format!("Save directory [{}]: ", default_dir.display())).await.unwrap_or_default();
        let prepared = prepare_dir(if answer.is_empty() { &default_dir } else { Path::new(&answer) }).await;
        let dir = match prepared {
            Ok(dir) => match check_output_file(&dir, args.output.as_deref()).await {
                Ok(()) => dir,
                Err(e) => {
                    stderr_line(&format!("[ERROR] {}", e));
                    continue;
                }
            },
            Err(e) => {
                stderr_line(&format!("[ERROR] {}", e));
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
    let row =
        format!("{:<14} {:>11}  {:<40}  {:<24} {}", status, size, truncate(&entry.file_name, 40), truncate(&host, 24), note);
    printable(row.trim_end())
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
        stderr_line("error: cannot read the download history");
        return EXIT_FAILED;
    };
    // A reader that stops early (`--history | head`) is not an error.
    let _ = write_history(&mut std::io::stdout().lock(), &path, &entries);
    EXIT_OK
}

fn write_history(out: &mut impl Write, path: &Path, entries: &[HistoryEntry]) -> std::io::Result<()> {
    if entries.is_empty() {
        return writeln!(out, "No downloads in history ({}).", path.display());
    }
    writeln!(out, "{:<14} {:>11}  {:<40}  {:<24} NOTE", "STATUS", "SIZE", "FILE", "HOST")?;
    for entry in entries {
        writeln!(out, "{}", history_row(entry))?;
    }
    writeln!(out, "\n{} entries, newest first ({})", entries.len(), path.display())
}

fn print_verification(res: &BuildVerificationResult) {
    let _ = write_verification(&mut std::io::stdout().lock(), res);
}

fn write_verification(out: &mut impl Write, res: &BuildVerificationResult) -> std::io::Result<()> {
    writeln!(out, "File:     {}", printable(&res.file_path.display().to_string()))?;
    writeln!(out, "Status:   {}", printable(&res.status_message))?;
    match res.expected_size {
        Some(expected) => writeln!(out, "Size:     {} bytes on disk, {} expected", res.actual_size, expected)?,
        None => writeln!(out, "Size:     {} bytes on disk, expected size unknown", res.actual_size)?,
    }
    if !res.missing_ranges.is_empty() {
        let bytes: u64 = res.missing_ranges.iter().map(|r| r.len()).sum();
        writeln!(out, "Missing:  {} range(s), {}", res.missing_ranges.len(), HumanBytes(bytes))?;
    }
    match res.checksum_match {
        Some(true) => writeln!(out, "Checksum: match"),
        Some(false) => writeln!(out, "Checksum: MISMATCH"),
        None => Ok(()),
    }
}

/// Verifies `path`; `expected` is the file size when the caller knows it better than the evidence
/// on disk (after a repair, whose state file is gone once the file is complete).
async fn run_verify(
    path: PathBuf,
    expected: Option<u64>,
    checksum: Option<String>,
) -> Result<BuildVerificationResult, String> {
    tokio::task::spawn_blocking(move || verify::verify_build_file(&path, expected, checksum.as_deref()))
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
fn repair_candidates(explicit: &[String], data_path: &Path) -> Result<Vec<Url>, String> {
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

/// The sources among `candidates`, with landing pages resolved to their direct links, that answer
/// a byte-range request. The resume state lists every URL the user gave, including mirrors the
/// download dropped for lacking range support; the repair takes a full-file answer from one of
/// those as proof that the file changed and gives up.
async fn ranged_mirrors(candidates: &[Url]) -> Result<Vec<Url>, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .default_headers(SmartResolver::default_anti_qos_headers())
        .build()
        .map_err(|e| format!("cannot create HTTP client: {}", e))?;
    let probes = candidates.iter().map(|url| async {
        let mut ranged = Vec::new();
        for url in SmartResolver::resolve_mirrors(&client, url).await {
            let response = client.get(url.clone()).header(reqwest::header::RANGE, "bytes=0-0").send().await;
            if response.is_ok_and(|r| r.status() == reqwest::StatusCode::PARTIAL_CONTENT) {
                ranged.push(url);
            } else {
                tracing::warn!("Not repairing from {}: it does not answer byte-range requests", url);
            }
        }
        ranged
    });
    let mut urls: Vec<Url> = Vec::new();
    for url in futures_util::future::join_all(probes).await.into_iter().flatten() {
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    Ok(urls)
}

/// The message and exit code for a repair that did not finish; `stopped` is the signal's code.
fn repair_failure(error: &str, stopped: Option<i32>) -> (String, i32) {
    if let Some(code) = stopped {
        return (format!("[STOPPED] {}; repaired ranges are saved, run the command again to continue", error), code);
    }
    // The repair takes the same claim as a running download and refuses while one holds it.
    if error.contains("is still being downloaded") {
        return (
            format!("Cannot repair: {}. Let that download finish or stop it, then run the command again.", error),
            EXIT_USAGE,
        );
    }
    (format!("[FAILED] Repair: {}", error), EXIT_FAILED)
}

async fn verify_file(args: &Args, path: &Path, ui: &Ui, shutdown: &Shutdown) -> i32 {
    if !args.repair && !args.urls.is_empty() {
        return usage("URLs after --verify are only used together with --repair");
    }
    let res = match run_verify(path.to_path_buf(), None, args.checksum.clone()).await {
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
        stderr_line("Nothing to repair: missing ranges are unknown or the contents are wrong; download the file again.");
        return EXIT_USAGE;
    }
    let Some(total_size) = res.expected_size else {
        stderr_line("Cannot repair: the expected file size is unknown.");
        return EXIT_USAGE;
    };
    let candidates = {
        let (explicit, data_path) = (args.urls.clone(), res.file_path.clone());
        match tokio::task::spawn_blocking(move || repair_candidates(&explicit, &data_path)).await {
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
    let urls = match ranged_mirrors(&candidates).await {
        Ok(urls) if !urls.is_empty() => urls,
        Ok(_) => {
            stderr_line(&format!(
                "[FAILED] Repair: none of the {} known URL(s) answers byte-range requests; pass one that does: --verify FILE --repair URL",
                candidates.len()
            ));
            return EXIT_FAILED;
        }
        Err(e) => return usage(&e),
    };

    stdout_line(&format!("\nRepairing from {} mirror(s)...", urls.len()));
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
        let (message, code) = repair_failure(&e, shutdown.requested());
        stderr_line(&message);
        return code;
    }

    // A repaired .part is renamed to its final name, and its state file is removed with it.
    let recheck = if res.file_path.exists() { res.file_path.clone() } else { final_path(&res.file_path) };
    stdout_line("\nRepair finished; verifying again...");
    match run_verify(recheck, Some(total_size), args.checksum.clone()).await {
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
    use hyperfetch_core::ByteRange;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn parse(args: &[&str]) -> Args {
        Args::parse_from(std::iter::once("cli").chain(args.iter().copied()))
    }

    /// A fresh, empty directory for one test.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hf-cli-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(None, 0), 0);
        assert_eq!(exit_code(None, 3), 1);
        assert_eq!(exit_code(Some(130), 0), 130);
        assert_eq!(exit_code(Some(143), 2), 143);
    }

    #[test]
    fn exit_does_not_wait_for_abandoned_blocking_work() {
        let started = std::time::Instant::now();
        let code = run_detached(async {
            // Like a final hash still running after the stop timeout gave up on it.
            drop(tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_secs(30))));
            130
        });
        assert_eq!(code.unwrap(), 130);
        assert!(started.elapsed() < Duration::from_secs(10), "waited {:?}", started.elapsed());
    }

    #[test]
    fn single_file_options_need_a_single_task() {
        let file = Path::new("does-not-exist-hf-cli/out.bin");
        assert!(check_single_file_options(Some(file), true, 1).is_ok());
        assert!(check_single_file_options(Some(file), false, 2).unwrap_err().contains("-d DIR"));
        assert!(check_single_file_options(None, true, 2).unwrap_err().contains("--checksum"));
        assert!(check_single_file_options(None, false, 5).is_ok());
    }

    #[tokio::test]
    async fn output_file_is_checked_under_the_download_dir() {
        let dir = test_dir("output");
        std::fs::create_dir(dir.join("sub")).unwrap();
        let err = check_output_file(&dir, Some(Path::new("sub"))).await.unwrap_err();
        assert!(err.contains("is a directory"), "{}", err);
        assert!(check_output_file(&dir, Some(Path::new("new/"))).await.is_err());
        assert!(check_output_file(&dir, Some(Path::new("file.bin"))).await.is_ok());
        assert!(check_output_file(&dir, None).await.is_ok());
        // "src" is a directory relative to the working directory, but not under -d.
        assert!(check_output_file(&dir, Some(Path::new("src"))).await.is_ok());
    }

    #[test]
    fn the_download_dir_is_always_passed_as_a_directory() {
        let args = parse(&["-d", "gone", "https://a.example/f"]);
        let task = Task { urls: vec![Url::parse("https://a.example/f").unwrap()], ..Default::default() };
        let output = job(&args, 4, Path::new("gone"), task).options.output_path.unwrap();
        let last = *output.as_os_str().as_encoded_bytes().last().unwrap();
        assert!(std::path::is_separator(last as char), "{}", output.display());
    }

    #[test]
    fn an_empty_queue_is_not_an_error() {
        let dir = test_dir("queue");
        let queue = dir.join("queue.txt");
        std::fs::write(&queue, "# One download per line\n\n").unwrap();
        let args = parse(&["-q", "-i", queue.to_str().unwrap(), "-d", dir.to_str().unwrap()]);
        let code = tokio::runtime::Runtime::new().unwrap().block_on(async {
            batch(&args, &Ui::new(true), &Shutdown::install(), &reqwest::Client::new()).await
        });
        assert_eq!(code, EXIT_OK);
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

        entry.file_name = "\u{1b}]0;owned\u{7}.bin".to_string();
        assert!(!history_row(&entry).contains('\u{1b}'));
    }

    /// Stdout of `| head` after head exited.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_pipe_stops_output_without_panicking() {
        let entries = vec![HistoryEntry::new("a".to_string(), PathBuf::from("/d/a"), 1, vec![]); 3];
        assert!(write_history(&mut ClosedPipe, Path::new("h.json"), &entries).is_err());
        let res = BuildVerificationResult {
            file_path: PathBuf::from("f"),
            is_complete: false,
            missing_ranges: vec![ByteRange { start: 0, end: 9 }],
            expected_size: Some(10),
            actual_size: 0,
            has_state_file: true,
            checksum_match: Some(false),
            status_message: "incomplete".to_string(),
        };
        assert!(write_verification(&mut ClosedPipe, &res).is_err());
    }

    #[test]
    fn repair_failures_map_to_exit_codes() {
        let (message, code) = repair_failure("/d/x.iso is still being downloaded", None);
        assert_eq!(code, EXIT_USAGE);
        assert!(message.starts_with("Cannot repair: /d/x.iso is still being downloaded."), "{}", message);
        assert_eq!(repair_failure("/d/x.iso is still being downloaded", Some(130)).1, 130);
        assert_eq!(repair_failure("connection reset", None), ("[FAILED] Repair: connection reset".to_string(), EXIT_FAILED));
    }

    #[test]
    fn repair_prefers_explicit_urls_and_rejects_bad_ones() {
        let urls = repair_candidates(&["https://a.example/f".to_string(), "https://a.example/f".to_string()], Path::new("f"));
        assert_eq!(urls.unwrap().len(), 1);
        assert!(repair_candidates(&["file.bin".to_string()], Path::new("f")).is_err());
    }

    /// Serves `body` over HTTP/1.1: "/ranged" answers Range requests with 206, every other path
    /// answers 200 with a page, like a landing page or a mirror without range support.
    async fn serve(body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                    let range = head.lines().find_map(|l| l.strip_prefix("range: bytes=")).and_then(|r| {
                        let (start, end) = r.trim().split_once('-')?;
                        Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
                    });
                    let response = match range {
                        Some((start, end)) if head.starts_with("get /ranged ") => {
                            let end = end.min(body.len() - 1);
                            let mut r = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                start,
                                end,
                                body.len(),
                                end + 1 - start
                            )
                            .into_bytes();
                            r.extend_from_slice(&body[start..=end]);
                            r
                        }
                        _ => b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npage".to_vec(),
                    };
                    let _ = socket.write_all(&response).await;
                });
            }
        });
        format!("http://{}", addr)
    }

    /// A download killed hard (no history entry) whose state lists a mirror without range support
    /// first: the repair must skip that mirror and report the repaired file as complete.
    #[tokio::test(flavor = "multi_thread")]
    async fn repair_skips_range_less_mirrors_and_verifies_the_result() {
        let data: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
        let server = serve(data.clone()).await;
        let dir = test_dir("repair");
        let final_file = dir.join("data.bin");
        let part = dir.join("data.bin.part");
        let mut on_disk = data.clone();
        on_disk[16 * 1024..32 * 1024].fill(0);
        std::fs::write(&part, &on_disk).unwrap();
        let mut state = DownloadState::new(
            "data.bin.part".to_string(),
            data.len() as u64,
            4 << 20,
            vec![format!("{}/landing", server), format!("{}/ranged", server)],
        );
        state.completed_ranges = vec![ByteRange { start: 0, end: 16 * 1024 - 1 }, ByteRange { start: 32 * 1024, end: 64 * 1024 - 1 }];
        state.etag = Some("\"v1\"".to_string());
        state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();

        let args = parse(&["-q", "--verify", final_file.to_str().unwrap(), "--repair"]);
        let code = verify_file(&args, &final_file, &Ui::new(true), &Shutdown::install()).await;
        assert_eq!(code, EXIT_OK);
        assert_eq!(std::fs::read(&final_file).unwrap(), data);
        assert!(!part.exists());
    }
}
