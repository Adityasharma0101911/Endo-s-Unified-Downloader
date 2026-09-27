mod cli;
mod download;

use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use hyperfetch_core::engine::DownloadOptions;
use hyperfetch_core::history::{is_redacted, DownloadHistoryManager, HistoryEntry, HistoryStatus, REDACTED_LINK};
use hyperfetch_core::ingest::{decode_text, descriptor_client, http_url, ingest, input_tokens, ListOptions, Task};
use hyperfetch_core::media::BrowserCookieSource;
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
    let http = match descriptor_client(args.proxy.as_deref()) {
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

/// Saves `task` under `dir`: as -o, else as its name in its folder, else into its folder under
/// the name the server or yt-dlp gives.
fn job(args: &Args, connections: u64, dir: &Path, task: Task) -> Job {
    let label = task.label();
    let folder = task.folder.as_ref().map_or_else(|| dir.to_path_buf(), |folder| dir.join(folder));
    let output = match (&args.output, &task.name) {
        (Some(file), _) => dir.join(file),
        (None, Some(name)) => folder.join(name),
        // The trailing separator keeps it a directory even if it disappears mid-batch; a
        // download then fails instead of being saved as a file with the directory's name.
        (None, None) => folder.join(""),
    };
    let media = args.media_preset.is_some() || task.urls.iter().any(hyperfetch_core::media::is_supported_media_site);
    let options = DownloadOptions {
        output_path: Some(output),
        // --checksum is for the file a document lists, not for the document downloaded itself.
        expected_checksum: args.checksum.clone().filter(|_| !task.document_itself).or(task.checksum),
        cookies_path: args.load_cookies.clone(),
        // --header is for the hosts the user named, not those a .metalink or .torrent lists.
        auth_header: args.auth_header.clone().filter(|_| !task.from_document),
        proxy: args.proxy.clone(),
        media_preset: args.media_preset.clone(),
        browser_cookies: args.cookies_from_browser.map(Into::into),
        install_ffmpeg: !args.no_install_ffmpeg,
        subtitles: args.subs.clone(),
        embed_metadata: !args.no_embed_metadata,
        live_from_start: args.live_from_start,
        wait_for_video: args.wait_for_video,
        ..tuning(args, if media { args.concurrent_fragments } else { connections })
    };
    Job { label, urls: task.urls, options, line: None }
}

/// How a link that lists many downloads (a folder, feed, playlist or channel) is read.
fn list_options(args: &Args) -> ListOptions {
    let cookies = match (args.cookies_from_browser, &args.load_cookies) {
        (Some(browser), _) => browser.into(),
        (None, Some(file)) => BrowserCookieSource::File(file.clone()),
        (None, None) => BrowserCookieSource::None,
    };
    ListOptions {
        google_api_key: args.google_api_key.clone(),
        whole_playlist: args.yes_playlist,
        latest: args.latest,
        only_new: !args.all_items,
        cookies,
        proxy: args.proxy.clone(),
    }
}

/// The engine settings every download of this run shares, with `connections` per download.
fn tuning(args: &Args, connections: u64) -> DownloadOptions {
    DownloadOptions {
        num_connections: connections as usize,
        base_chunk_size: args.chunk_size_mb * 1024 * 1024,
        max_speed: args.max_speed.filter(|&s| s > 0),
        max_retries: args.max_retries,
        stall_timeout_secs: args.stall_timeout,
        fsync_on_complete: args.fsync,
        max_connections_per_host: args.max_connections_per_host,
        ..Default::default()
    }
}

/// The meaningful lines of a batch file with their 1-based line numbers: trimmed, without
/// blanks and # comments.
fn batch_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .map(str::trim)
        .enumerate()
        .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
        .map(|(i, l)| (i + 1, l))
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

/// The downloads `inputs` list, each with the input-file line it came from (None for the
/// command-line URLs), and how many inputs could not be read into downloads (those are reported).
async fn read_tasks(
    inputs: &[(Option<usize>, Vec<String>)],
    ui: &Ui,
    http: &reqwest::Client,
    list: &ListOptions,
) -> (Vec<(Option<usize>, Task)>, usize) {
    let mut tasks = Vec::new();
    let mut failed = 0;
    for (line, tokens) in inputs {
        match ingest(tokens, http, list).await {
            Ok(found) => tasks.extend(found.into_iter().map(|task| (*line, task))),
            Err(e) => {
                ui.error(&format!("[FAILED] {}: {}", truncate(&tokens.join(" "), 60), e));
                failed += 1;
            }
        }
    }
    (tasks, failed)
}

async fn batch(args: &Args, ui: &Ui, shutdown: &Shutdown, http: &reqwest::Client) -> i32 {
    let mut inputs: Vec<(Option<usize>, Vec<String>)> = Vec::new();
    if let Some(path) = &args.input_file {
        match read_input(path).await {
            Ok(text) => {
                for (number, line) in batch_lines(&text) {
                    inputs.push((Some(number), input_tokens(line).await));
                }
            }
            Err(e) => return usage(&e),
        }
    }
    if !args.urls.is_empty() {
        inputs.push((None, args.urls.clone()));
    }

    let (tasks, mut failed) = read_tasks(&inputs, ui, http, &list_options(args)).await;
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

    let jobs = tasks.into_iter().map(|(line, t)| Job { line, ..job(args, args.connections, &dir, t) }).collect();
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
        let tasks = match ingest(&tokens, http, &list_options(args)).await {
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
/// else the URLs history recorded for exactly this path, less links saved without their secret.
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
    let usable: Vec<&String> = candidates.iter().filter(|c| !is_redacted(c)).collect();
    if usable.is_empty() && !candidates.is_empty() {
        return Err(format!("{}: --verify FILE --repair URL", REDACTED_LINK));
    }
    let mut urls = Vec::new();
    for candidate in usable {
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
        &tuning(args, args.connections),
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

    /// A task in a folder is saved in that folder under the download dir: as its name, else as a
    /// directory the server or yt-dlp names the file in.
    #[test]
    fn a_task_in_a_folder_is_saved_in_it() {
        let args = parse(&["https://a.example/list"]);
        let urls = vec![Url::parse("https://a.example/ep1").unwrap()];
        let named = Task { urls: urls.clone(), folder: Some("Show".into()), name: Some("ep1.mp3".into()), ..Default::default() };
        let output = job(&args, 4, Path::new("d"), named).options.output_path.unwrap();
        assert_eq!(output, Path::new("d").join("Show").join("ep1.mp3"));
        let unnamed = Task { urls, folder: Some("Show".into()), ..Default::default() };
        let output = job(&args, 4, Path::new("d"), unnamed).options.output_path.unwrap();
        assert_eq!(output, Path::new("d").join("Show").join(""));
        assert!(std::path::is_separator(*output.as_os_str().as_encoded_bytes().last().unwrap() as char));
    }

    #[test]
    fn listing_and_media_flags_reach_ingest_and_the_engine() {
        let defaults = list_options(&parse(&["u"]));
        assert!(defaults.only_new && !defaults.whole_playlist && defaults.latest.is_none());
        assert_eq!(defaults.cookies, BrowserCookieSource::None);
        let args = parse(&[
            "--yes-playlist", "--latest", "3", "--all-items", "--cookies-from-browser", "firefox", "--proxy",
            "socks5h://127.0.0.1:9050", "--no-install-ffmpeg", "--subs", "all", "--no-embed-metadata", "--live-from-start",
            "--wait-for-video", "u",
        ]);
        let list = list_options(&args);
        assert!(list.whole_playlist && !list.only_new);
        assert_eq!((list.latest, list.cookies, list.proxy.as_deref()), (Some(3), BrowserCookieSource::Firefox, Some("socks5h://127.0.0.1:9050")));
        let task = Task { urls: vec![Url::parse("https://a.example/f").unwrap()], ..Default::default() };
        let options = job(&args, 4, Path::new("d"), task).options;
        assert!(!options.install_ffmpeg && !options.embed_metadata && options.live_from_start && options.wait_for_video);
        assert_eq!(options.subtitles.as_deref(), Some("all"));
        let task = Task { urls: vec![Url::parse("https://a.example/f").unwrap()], ..Default::default() };
        let options = job(&parse(&["u"]), 4, Path::new("d"), task).options;
        assert!(options.install_ffmpeg && options.embed_metadata && !options.live_from_start && !options.wait_for_video);
    }

    /// --header goes to the hosts the user named, never to the mirrors a .metalink or .torrent
    /// lists.
    #[test]
    fn the_authorization_header_skips_the_hosts_a_document_lists() {
        let args = parse(&["--header", "Authorization: Bearer ghp_x", "https://a.example/list.meta4"]);
        let urls = vec![Url::parse("https://mirror.example/f.iso").unwrap()];
        let typed = Task { urls: urls.clone(), ..Default::default() };
        let listed = Task { urls, from_document: true, ..Default::default() };
        assert_eq!(job(&args, 4, Path::new("d"), typed).options.auth_header.as_deref(), Some("Bearer ghp_x"));
        assert_eq!(job(&args, 4, Path::new("d"), listed).options.auth_header, None);
    }

    /// --checksum is for the file a document lists: the document downloaded itself (a torrent
    /// without web seeds) is not checked against it.
    #[test]
    fn the_checksum_skips_a_document_downloaded_itself() {
        let checksum = format!("sha256:{}", "ab".repeat(32));
        let args = parse(&["--checksum", &checksum, "https://a.example/x.torrent"]);
        let urls = vec![Url::parse("https://a.example/x.torrent").unwrap()];
        let file = Task { urls: urls.clone(), ..Default::default() };
        let itself = Task { urls, document_itself: true, ..Default::default() };
        assert_eq!(job(&args, 4, Path::new("d"), file).options.expected_checksum, Some(checksum));
        assert_eq!(job(&args, 4, Path::new("d"), itself).options.expected_checksum, None);
    }

    #[test]
    fn every_download_gets_the_disk_and_host_settings() {
        let args = parse(&["--fsync", "--max-connections-per-host", "6", "-s", "3", "https://a.example/f"]);
        let task = Task { urls: vec![Url::parse("https://a.example/f").unwrap()], ..Default::default() };
        let options = job(&args, args.connections, Path::new("d"), task).options;
        assert_eq!((options.fsync_on_complete, options.max_connections_per_host, options.num_connections), (true, 6, 3));
        let defaults = tuning(&parse(&["u"]), 16);
        assert_eq!((defaults.fsync_on_complete, defaults.max_connections_per_host), (false, 64));
    }

    /// Each download keeps the line of the input file that listed it, however many downloads
    /// the lines before it turned into.
    #[tokio::test]
    async fn downloads_keep_their_input_line() {
        let dir = test_dir("lines");
        let metalink = dir.join("two.meta4");
        std::fs::write(
            &metalink,
            r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink">
            <file name="a.bin"><url>https://m.example/a.bin</url></file>
            <file name="b.bin"><url>https://m.example/b.bin</url></file>
            </metalink>"#,
        )
        .unwrap();
        let text = format!("# queue\n{}\nnot-a-url\n\nhttps://h.example/c.bin\n", metalink.display());
        let mut inputs = Vec::new();
        for (number, line) in batch_lines(&text) {
            inputs.push((Some(number), input_tokens(line).await));
        }
        inputs.push((None, vec!["https://h.example/d.bin".to_string()]));

        let (tasks, failed) = read_tasks(&inputs, &Ui::new(true), &reqwest::Client::new(), &ListOptions::default()).await;
        assert_eq!(failed, 1);
        let lines: Vec<(Option<usize>, String)> = tasks.iter().map(|(line, task)| (*line, task.label())).collect();
        assert_eq!(
            lines,
            [(Some(2), "a.bin".into()), (Some(2), "b.bin".into()), (Some(5), "c.bin".into()), (None, "d.bin".into())]
        );
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
    fn repair_never_uses_a_link_saved_without_its_secret() {
        let file = Path::new("/nowhere/a.iso");
        let redacted = "https://h.example/a.iso?token=REDACTED".to_string();
        let err = repair_candidates(std::slice::from_ref(&redacted), file).unwrap_err();
        assert!(err.starts_with(REDACTED_LINK), "{}", err);
        let mirror = "https://m.example/a.iso".to_string();
        assert_eq!(repair_candidates(&[redacted, mirror.clone()], file).unwrap(), [Url::parse(&mirror).unwrap()]);
    }

    #[test]
    fn batch_lines_skip_comments_and_blanks() {
        let lines: Vec<(usize, &str)> = batch_lines("# queue\r\n\r\n  https://a  https://b \r\n#x\nhttps://c").collect();
        assert_eq!(lines, [(3, "https://a  https://b"), (5, "https://c")]);
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
        // The repair runs through the engine, which records the finished file in history.
        std::env::set_var("ENDO_HISTORY_PATH", dir.join("history.json"));
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
