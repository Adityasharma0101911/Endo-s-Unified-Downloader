//! The helper programs the app runs for what it has no code of its own for: cloud drives
//! (rclone), Epic Games and Steam libraries (legendary, DepotDownloader) and live streams yt-dlp
//! cannot follow (streamlink). Each is the one on PATH, else a managed install of the release
//! pinned here, fetched from the project's own release only once a feature needs it and checked
//! against the SHA-256 pinned here; none is bundled. They run without a console window, at
//! below-normal priority, in a process tree a cancel kills, and keep what they sign in with in a
//! folder of the app's own (see [`account_dir`]), never in the tool's usual one, so a separate
//! install of the same tool is left alone.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::engine::{EngineSnapshot, CANCELLED};
use crate::media::{exe_name, ProcessTree};

/// A helper program.
#[derive(Debug)]
pub struct Tool {
    /// Its program's name without `.exe`, as looked for on PATH.
    pub program: &'static str,
    /// Its licence, as its project gives it.
    pub licence: &'static str,
    /// Its project's page.
    pub home: &'static str,
    /// What makes it print its version.
    pub version_args: &'static [&'static str],
    /// The builds this app installs, one per system it has one for.
    pub releases: &'static [Release],
}

/// A build of a helper this app installs: a file of the project's own release.
#[derive(Debug)]
pub struct Release {
    /// The system it runs on: `std::env::consts::OS`, a dash, `ARCH` ("windows-x86_64").
    pub target: &'static str,
    pub version: &'static str,
    pub url: &'static str,
    /// Its SHA-256, from the project's published checksums (or the release's own digest).
    pub sha256: &'static str,
    pub size: u64,
}

impl Tool {
    /// The build this app installs on this system, if it has one.
    pub fn release(&self) -> Option<&'static Release> {
        let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        self.releases.iter().find(|r| r.target == target)
    }
}

/// What the app knows of a helper on this computer, for the front ends to show.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolState {
    pub program: &'static str,
    /// Where it is (on PATH, or the managed install); None when it is not there.
    pub path: Option<PathBuf>,
    /// The first line it prints for its version.
    pub version: Option<String>,
    pub licence: &'static str,
    pub home: &'static str,
    /// The version this app would install here and the size of its download; None when it
    /// installs none on this system (install it yourself, on PATH).
    pub installs: Option<(&'static str, u64)>,
}

/// The folder of the app's data where the account of `id` (a helper's config, its sign-in) is
/// kept: beside the download history (see `history::DownloadHistoryManager`), so a portable copy
/// keeps its own. Unit tests get one of their own.
pub fn account_dir(id: &str) -> PathBuf {
    let history = crate::history::DownloadHistoryManager::default_history_path();
    let base = if cfg!(test) { history.with_extension("accounts") } else { history.with_file_name("accounts") };
    base.join(id)
}

/// [`account_dir`], created, readable by this user alone where the system has such modes.
pub(crate) fn own_dir(id: &str) -> Result<PathBuf, String> {
    let dir = account_dir(id);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Writes `bytes` (a sign-in) to `path` in [`own_dir`] in one rename, readable by this user alone
/// where the system has such modes. Blocking.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let temp = path.with_extension(format!("{}.tmp", crate::media::unique_suffix()));
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    let written = file.open(&temp).and_then(|mut f| f.write_all(bytes)).and_then(|()| std::fs::rename(&temp, path));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("Cannot write {}: {e}", path.display())
    })
}

/// The folder in `bin_dir` a release of `tool` is installed in.
fn release_dir(bin_dir: &Path, tool: &Tool, release: &Release) -> PathBuf {
    bin_dir.join(format!("{}-{}", tool.program, release.version))
}

/// `tool`'s program in `dir` or a folder in it, three levels down at most (a release ZIP's
/// `name-version/bin/`). Blocking.
fn program_in(dir: &Path, tool: &Tool) -> Option<PathBuf> {
    let name = exe_name(tool.program);
    let mut level = vec![dir.to_path_buf()];
    for _ in 0..4 {
        if let Some(found) = level.iter().map(|d| d.join(&name)).find(|p| p.is_file()) {
            return Some(found);
        }
        level = level.iter().flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    }
    None
}

/// The `tool` to run: the one on PATH, else the managed install of its pinned release. Installs
/// nothing. Blocking.
pub fn find(tool: &Tool) -> Option<PathBuf> {
    crate::media::find_in_path(&exe_name(tool.program)).or_else(|| find_managed(tool))
}

/// The managed install of `tool`'s pinned release for this system, if it is there. Blocking.
fn find_managed(tool: &Tool) -> Option<PathBuf> {
    let (bin_dir, release) = (crate::media::managed_bin_dir()?, tool.release()?);
    program_in(&release_dir(&bin_dir, tool, release), tool)
}

/// The first line `program` prints with `args` (its version), within 10 s.
pub(crate) async fn version(program: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await.ok()?.ok()?;
    let text = [output.stdout, output.stderr].concat();
    String::from_utf8_lossy(&text).lines().map(str::trim).find(|l| !l.is_empty()).map(|l| crate::ingest::truncate_chars(l, 80))
}

/// What the app knows of `tool` here (see [`ToolState`]).
pub async fn state(tool: &'static Tool) -> ToolState {
    let path = tokio::task::spawn_blocking(move || find(tool)).await.ok().flatten();
    let version = match &path {
        Some(path) => version(path, tool.version_args).await,
        None => None,
    };
    ToolState { program: tool.program, path, version, licence: tool.licence, home: tool.home, installs: tool.release().map(|r| (r.version, r.size)) }
}

/// The proxy setting a helper's traffic is to follow, and the app's own requests for it (an
/// install, GOG's catalogue).
#[derive(Debug, Clone, Copy, Default)]
pub struct Proxy<'a> {
    /// The one proxy set: a helper gets it in its environment.
    pub url: Option<&'a str>,
    /// A pool of proxies is set, which no helper can take turns through.
    pub pool: bool,
    /// No proxy at all, not even the environment's (`http_proxy` and the like).
    pub direct: bool,
}

/// The proxy variables a program reads from its environment.
const PROXY_VARS: [&str; 8] = ["http_proxy", "https_proxy", "all_proxy", "no_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];

impl<'a> Proxy<'a> {
    /// The setting of a download's options.
    pub fn of(options: &'a crate::engine::DownloadOptions) -> Self {
        Self { url: options.proxy.as_deref(), pool: !options.proxy_pool.is_empty(), direct: options.no_proxy }
    }

    /// One proxy, or none (then the environment's, if it names one).
    pub fn single(url: Option<&'a str>) -> Self {
        Self { url, ..Self::default() }
    }

    /// Whether `tool` can follow this setting; else why not, said before it makes any request.
    /// Every helper is given the one proxy or none (see [`command`]), and reads the environment's
    /// itself; none can take turns through a pool. `takes_proxy` is false for a tool that goes
    /// through no proxy at all (DepotDownloader), which then runs only without one.
    pub fn check(self, tool: &str, takes_proxy: bool) -> Result<(), String> {
        let system = crate::sysproxy::seen().is_some_and(|seen| seen.taken);
        self.check_in(tool, takes_proxy, crate::engine::proxy_in_env(), system)
    }

    /// [`Proxy::check`], `env` telling whether the environment names a proxy, and `system`
    /// whether it is the one the system's settings name (see `sysproxy::start`).
    fn check_in(self, tool: &str, takes_proxy: bool, env: bool, system: bool) -> Result<(), String> {
        if self.pool {
            return Err(format!("{tool} cannot take turns through a proxy pool: set one proxy for it, or none"));
        }
        if !takes_proxy && self.url.is_some() {
            return Err(format!("{tool} cannot go through a proxy: turn the proxy off for it"));
        }
        if !takes_proxy && !self.direct && env && system {
            return Err(format!("{tool} cannot go through a proxy, and the one this computer's settings name is in use: {}, for it", crate::sysproxy::TURN_OFF));
        }
        if !takes_proxy && !self.direct && env {
            return Err(format!("{tool} cannot go through a proxy, and one is set in the environment (http_proxy or the like): unset it, or choose no proxy, for it"));
        }
        Ok(())
    }

    /// A client for the app's own requests on a helper's behalf, following this setting.
    pub(crate) fn client(self, timeout: Duration) -> Result<reqwest::Client, String> {
        let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).timeout(timeout).user_agent(crate::resolver::APP_USER_AGENT);
        if let Some(url) = self.url {
            builder = builder.proxy(reqwest::Proxy::all(url).map_err(|e| format!("Invalid proxy {url}: {e}"))?);
        } else if self.direct {
            builder = builder.no_proxy();
        }
        builder.build().map_err(|e| format!("Failed to build HTTP client: {e}"))
    }
}

/// Serializes installs of helper programs.
static INSTALLING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The `tool` to run (see [`find`]), its pinned release installed now if it is not there.
pub async fn ensure(tool: &'static Tool, proxy: Proxy<'_>) -> Result<PathBuf, String> {
    if let Ok(Some(found)) = tokio::task::spawn_blocking(move || find(tool)).await {
        return Ok(found);
    }
    install(tool, proxy).await
}

/// Installs `tool`'s pinned release for this system into the managed folder (see
/// [`install_release`]), unless it is there already. Returns the program to run.
pub async fn install(tool: &'static Tool, proxy: Proxy<'_>) -> Result<PathBuf, String> {
    let release = tool
        .release()
        .ok_or_else(|| format!("No {0} build is published for this system: install {0} yourself and put it on PATH ({1})", tool.program, tool.home))?;
    proxy.check(tool.program, true)?;
    let _guard = INSTALLING.lock().await;
    if let Ok(Some(found)) = tokio::task::spawn_blocking(move || find_managed(tool)).await {
        return Ok(found);
    }
    let bin_dir = crate::media::managed_bin_dir().ok_or("Cannot determine a per-user folder to install helper programs into")?;
    install_release(&proxy.client(Duration::from_secs(1800))?, tool, release, &bin_dir).await
}

/// Downloads `release` of `tool`, checks it against its pinned size and SHA-256, and only then
/// unpacks it (a ZIP) or writes it (the program itself) into a folder of its own in `bin_dir`,
/// which appears complete in one rename. Returns the program to run.
async fn install_release(client: &reqwest::Client, tool: &'static Tool, release: &'static Release, bin_dir: &Path) -> Result<PathBuf, String> {
    tracing::warn!("Installing {} {} from {} ({} bytes)", tool.program, release.version, release.url, release.size);
    let mut resp = crate::media::http_get(client, release.url, Duration::from_secs(1800)).await?;
    let mut bytes = Vec::with_capacity(release.size as usize);
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("Failed to download {}: {e}", release.url))? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > release.size {
            return Err(format!("{} is larger than the {} bytes of the release this app installs", release.url, release.size));
        }
    }
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != release.sha256 {
        return Err(format!("Downloaded {} failed checksum verification (expected {}, got {actual})", tool.program, release.sha256));
    }
    let bin_dir = bin_dir.to_path_buf();
    tokio::task::spawn_blocking(move || unpack_release(&bin_dir, tool, release, &bytes))
        .await
        .map_err(|e| format!("{} install task failed: {e}", tool.program))?
}

/// Puts the checked `bytes` of `release` into its folder in `bin_dir` (see [`install_release`]),
/// and deletes the folders of other releases of `tool` that no program runs from. Blocking.
fn unpack_release(bin_dir: &Path, tool: &Tool, release: &Release, bytes: &[u8]) -> Result<PathBuf, String> {
    let target = release_dir(bin_dir, tool, release);
    let temp = bin_dir.join(format!(".{}-{}.tmp", tool.program, crate::media::unique_suffix()));
    std::fs::create_dir_all(&temp).map_err(|e| format!("Cannot create {}: {e}", temp.display()))?;
    let unpacked = match release.url.ends_with(".zip") {
        true => crate::zipdir::unzip(bytes, &temp),
        false => std::fs::write(temp.join(exe_name(tool.program)), bytes).map_err(|e| format!("Cannot write {}: {e}", temp.display())),
    };
    let program = unpacked.and_then(|()| program_in(&temp, tool).ok_or_else(|| format!("The {} release has no {}", tool.program, exe_name(tool.program))));
    #[cfg(unix)]
    let program = program.and_then(|p| crate::media::make_executable(&p).map(|()| p).map_err(|e| e.to_string()));
    if let Err(e) = program {
        let _ = std::fs::remove_dir_all(&temp);
        return Err(e);
    }
    if let Err(e) = crate::media::rename_patiently(&temp, &target) {
        let _ = std::fs::remove_dir_all(&temp);
        // Another process installed it meanwhile.
        if program_in(&target, tool).is_none() {
            return Err(format!("Failed to install {} to {}: {e}", tool.program, target.display()));
        }
    }
    let prefix = format!("{}-", tool.program);
    for entry in std::fs::read_dir(bin_dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && entry.path() != target && entry.path().is_dir() {
            // A running program's folder cannot go on Windows; it goes the next time.
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    tracing::warn!("Installed {} {} in {}", tool.program, release.version, target.display());
    program_in(&target, tool).ok_or_else(|| format!("The {} release has no {}", tool.program, exe_name(tool.program)))
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// `bin` with `args`, run in `work_dir` without a console window, at below-normal priority, with
/// the proxy in its environment (see `media::ytdlp_command`; with no proxy at all, none of the
/// environment's either) and its output piped.
pub(crate) fn command(bin: &Path, args: &[String], proxy: Proxy<'_>, work_dir: &Path) -> Command {
    let mut cmd = crate::media::ytdlp_command(bin, args, proxy.url, work_dir);
    if proxy.direct && proxy.url.is_none() {
        for var in PROXY_VARS {
            cmd.env_remove(var);
        }
    }
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    cmd
}

/// What `cmd` (made by [`command`]) prints, run to its end within `limit`; past it, it is killed
/// with every process it started (a launcher's child too), and that is an error.
pub(crate) async fn output(mut cmd: Command, name: &str, limit: Duration) -> Result<std::process::Output, String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("Failed to run {name}: {e}"))?;
    lower_priority(&child);
    let mut tree = ProcessTree::attach(&child);
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let waited = tokio::time::timeout(limit, async {
        let read = |pipe: Option<Box<dyn AsyncRead + Unpin + Send>>| async move {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut bytes).await;
            }
            bytes
        };
        let (stdout, stderr) = tokio::join!(read(out.map(|o| Box::new(o) as _)), read(err.map(|e| Box::new(e) as _)));
        child.wait().await.map(|status| std::process::Output { status, stdout, stderr })
    })
    .await;
    match waited {
        Ok(done) => {
            tree.disarm();
            done.map_err(|e| format!("Failed to run {name}: {e}"))
        }
        Err(_) => {
            tree.kill_and_reap(&mut child).await;
            Err(format!("{name} gave no answer within {} s", limit.as_secs()))
        }
    }
}

/// Gives the process just started for `child` a lower priority than the user's own programs
/// (Windows starts it so, see [`command`]).
pub(crate) fn lower_priority(child: &tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: a plain call about the process just started; a failure leaves its priority.
        unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, 10) };
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// How far a helper's download is, as it tells it.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Progress {
    /// Bytes done and in all (0: not told).
    pub done: u64,
    pub total: u64,
    /// The share done (0 to 1), when the tool tells only that.
    pub ratio: Option<f64>,
    /// Bytes a second, when the tool tells it.
    pub speed: Option<f64>,
}

/// Lines of a helper's output an error quotes.
const TAIL: usize = 5;

/// Runs `cmd` (see [`command`]) for a download into `target`, reading what it prints on both its
/// outputs line by line (a line it redraws with `\r` counts each time): `parse` updates the
/// progress from each, which goes out as snapshots, at most every 200 ms. Cancelling kills its
/// process tree. A run that exits with a failure is an error quoting its last lines; one that
/// succeeds gives them.
pub(crate) async fn run(
    mut cmd: Command,
    name: &str,
    target: &Path,
    mut parse: impl FnMut(&str, &mut Progress) + Send,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<Vec<String>, String> {
    let mut child = cmd.spawn().map_err(|e| format!("Failed to run {name}: {e}"))?;
    lower_priority(&child);
    let mut tree = ProcessTree::attach(&child);
    let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    let outputs: [Option<Box<dyn AsyncRead + Unpin + Send>>; 2] =
        [child.stdout.take().map(|o| Box::new(o) as _), child.stderr.take().map(|e| Box::new(e) as _)];
    for out in outputs.into_iter().flatten() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let (mut reader, mut line) = (BufReader::new(out), Vec::new());
            while matches!(reader.read_until(b'\n', &mut line).await, Ok(n) if n > 0) {
                for part in String::from_utf8_lossy(&line).split(['\r', '\n']).filter(|p| !p.trim().is_empty()) {
                    let _ = tx.send(part.trim_end().to_string());
                }
                line.clear();
            }
        });
    }
    drop(tx);
    let (mut progress, mut tail, mut sent) = (Progress::default(), std::collections::VecDeque::new(), None::<Instant>);
    let emit = |progress: &Progress| {
        if let Some(tx) = &snapshots {
            let _ = tx.send(snapshot(progress, target));
        }
    };
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tree.kill_and_reap(&mut child).await;
                return Err(CANCELLED.to_string());
            }
            line = lines.recv() => {
                let Some(line) = line else { break };
                parse(&line, &mut progress);
                tail.push_back(line);
                if tail.len() > TAIL {
                    tail.pop_front();
                }
                if sent.is_none_or(|at| at.elapsed() >= Duration::from_millis(200)) {
                    emit(&progress);
                    sent = Some(Instant::now());
                }
            }
        }
    }
    let status = tokio::select! {
        status = child.wait() => status.map_err(|e| format!("Failed to wait on {name}: {e}"))?,
        () = cancel.cancelled() => {
            tree.kill_and_reap(&mut child).await;
            return Err(CANCELLED.to_string());
        }
    };
    tree.disarm();
    let tail: Vec<String> = tail.into();
    if !status.success() {
        return Err(match tail.is_empty() {
            true => format!("{name} exited with {status}"),
            false => format!("{name} exited with {status}: {}", tail.join("\n")),
        });
    }
    emit(&Progress { ratio: Some(1.0), done: progress.total.max(progress.done), ..progress });
    Ok(tail)
}

/// The snapshot of a helper's download into `target` that has come as far as `progress`.
pub(crate) fn snapshot(progress: &Progress, target: &Path) -> EngineSnapshot {
    let share = match progress.ratio {
        Some(ratio) => ratio,
        None if progress.total > 0 => progress.done as f64 / progress.total as f64,
        None => 0.0,
    };
    EngineSnapshot {
        total_bytes: progress.total,
        downloaded_bytes: progress.done,
        speed_bytes_per_sec: progress.speed.unwrap_or(0.0),
        progress_ratio: share.clamp(0.0, 1.0),
        active_workers: 1,
        target_path: Some(target.to_path_buf()),
        ..Default::default()
    }
}

/// Bytes in a size a helper prints: "12.34 MiB", "1.2 GB", "512 KiB", "300 B" (binary or decimal
/// units, as written).
pub(crate) fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))?;
    let number: f64 = text[..split].replace(',', "").parse().ok()?;
    let unit = text[split..].trim().trim_end_matches("/s").to_ascii_lowercase();
    let scale: f64 = match unit.as_str() {
        "b" | "bytes" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" | "k" => 1024.0,
        "mib" | "m" => 1024.0 * 1024.0,
        "gib" | "g" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024f64.powi(4),
        _ => return None,
    };
    Some((number * scale).round() as u64)
}

/// A string of `bytes` random bytes in hex: a session's password for a local server. `Err`
/// without the system's randomness: anything else could be guessed.
pub(crate) fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut buf).map_err(|_| "The system gave no random numbers".to_string())?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stand-in for a helper: a script named `name` in `dir` that runs `body` (lines of a
    /// Windows batch file or a POSIX shell script, as the system takes).
    pub(crate) fn fake_tool(dir: &Path, name: &str, windows: &str, unix: &str) -> PathBuf {
        #[cfg(windows)]
        let (bin, script) = (dir.join(format!("{name}.cmd")), format!("@echo off\r\n{}\r\n", windows.replace('\n', "\r\n")));
        #[cfg(not(windows))]
        let (bin, script) = (dir.join(name), format!("#!/bin/sh\n{unix}\n"));
        let _ = (windows, unix);
        std::fs::write(&bin, script).unwrap();
        #[cfg(not(windows))]
        crate::media::make_executable(&bin).unwrap();
        bin
    }

    #[test]
    fn sizes_are_read_in_either_units() {
        assert_eq!(parse_size("12.50 MiB"), Some(13_107_200));
        assert_eq!(parse_size("1.2 GB"), Some(1_200_000_000));
        assert_eq!(parse_size("512 KiB"), Some(524_288));
        assert_eq!(parse_size("300 B"), Some(300));
        assert_eq!(parse_size("6.75 MiB/s"), Some(7_077_888));
        assert_eq!(parse_size("1,024 bytes"), Some(1024));
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("5 parsecs"), None);
        assert_eq!(random_hex(16).unwrap().len(), 32);
        assert_ne!(random_hex(16), random_hex(16));
    }

    static FAKE: Tool = Tool { program: "hf-fake-helper", licence: "MIT", home: "https://example.com", version_args: &["--version"], releases: &[] };

    /// The progress a fake helper prints reaches the snapshots, its file is where it says, and a
    /// failure quotes its last lines; a cancel stops a long one at once.
    #[tokio::test]
    async fn a_helper_runs_with_its_progress_and_is_stopped_by_a_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_tool(
            dir.path(),
            "fake",
            "echo 25 of 100\necho 100 of 100\necho done>out.bin",
            "echo 25 of 100\necho 100 of 100\necho done > out.bin",
        );
        let (tx, mut rx) = broadcast::channel(16);
        let parse = |line: &str, p: &mut Progress| {
            if let Some((done, total)) = line.split_once(" of ") {
                (p.done, p.total) = (done.trim().parse().unwrap_or(0), total.trim().parse().unwrap_or(0));
            }
        };
        let cmd = command(&bin, &[], Proxy::default(), dir.path());
        let tail = run(cmd, "fake", dir.path(), parse, Some(tx), CancellationToken::new()).await.unwrap();
        assert_eq!(tail.last().map(String::as_str), Some("100 of 100"));
        assert!(dir.path().join("out.bin").is_file());
        let mut last = EngineSnapshot::default();
        while let Ok(s) = rx.try_recv() {
            last = s;
        }
        assert_eq!((last.downloaded_bytes, last.total_bytes, last.progress_ratio), (100, 100, 1.0));
        assert_eq!(last.target_path.as_deref(), Some(dir.path()));

        let failing = fake_tool(dir.path(), "failing", "echo broken>&2\nexit /b 3", "echo broken >&2\nexit 3");
        let e = run(command(&failing, &[], Proxy::default(), dir.path()), "failing", dir.path(), parse, None, CancellationToken::new()).await.unwrap_err();
        assert!(e.starts_with("failing exited with") && e.ends_with("broken"), "{e}");

        let slow = fake_tool(dir.path(), "slow", "echo 1 of 100\nping -n 30 127.0.0.1 >nul", "echo 1 of 100\nsleep 30");
        let cancel = CancellationToken::new();
        let (tx, mut rx) = broadcast::channel(16);
        let stop = async {
            rx.recv().await.unwrap();
            cancel.cancel();
        };
        let started = Instant::now();
        let (result, ()) = tokio::join!(run(command(&slow, &[], Proxy::default(), dir.path()), "slow", dir.path(), parse, Some(tx), cancel.clone()), stop);
        assert_eq!(result.unwrap_err(), CANCELLED);
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
        assert!(FAKE.release().is_none(), "a tool without builds installs none");
    }

    /// An install takes only the build whose SHA-256 is the pinned one, and leaves no file behind
    /// when it is not; a ZIP is unpacked and its program found inside.
    #[tokio::test]
    async fn an_install_takes_only_the_pinned_build() {
        let program = b"helper program".to_vec();
        let good = format!("{:x}", Sha256::digest(&program));
        let base = crate::media::tests::serve_files(vec![("/tool".to_string(), program.clone())], None).await;
        let url: &'static str = Box::leak(format!("{}/tool", base.trim_end_matches("/release")).into_boxed_str());
        let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let target: &'static str = Box::leak(target.into_boxed_str());
        let pinned = |sha256: String, size: u64| -> &'static Release { Box::leak(Box::new(Release { target, version: "1.0", url, sha256: Box::leak(sha256.into_boxed_str()), size })) };
        let tool = |release: &'static Release| -> &'static Tool {
            Box::leak(Box::new(Tool { program: "hf-test-tool", licence: "MIT", home: "https://example.com", version_args: &[], releases: std::slice::from_ref(release) }))
        };
        let client = crate::media::http_client(None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wrong = pinned(format!("{:x}", Sha256::digest(b"other")), program.len() as u64);
        let e = install_release(&client, tool(wrong), wrong, dir.path()).await.unwrap_err();
        assert!(e.contains("checksum"), "{e}");
        let small = pinned(good.clone(), 3);
        let e = install_release(&client, tool(small), small, dir.path()).await.unwrap_err();
        assert!(e.contains("larger"), "{e}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing is left behind");
        let right = pinned(good, program.len() as u64);
        let installed = install_release(&client, tool(right), right, dir.path()).await.unwrap();
        assert_eq!(installed, dir.path().join("hf-test-tool-1.0").join(exe_name("hf-test-tool")));
        assert_eq!(std::fs::read(&installed).unwrap(), program);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temporary folder is left");
    }

    /// A download of a release cut off before its end is an error and leaves nothing behind.
    #[tokio::test]
    async fn an_install_cut_short_leaves_nothing() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                // 1000 bytes said, 10 sent, then the connection ends.
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n0123456789").await;
            }
        });
        let url: &'static str = Box::leak(format!("http://{addr}/tool").into_boxed_str());
        let target: &'static str = Box::leak(format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH).into_boxed_str());
        let release: &'static Release = Box::leak(Box::new(Release { target, version: "1.0", url, sha256: "00", size: 1000 }));
        let tool: &'static Tool = Box::leak(Box::new(Tool { program: "hf-test-cut", licence: "MIT", home: "https://example.com", version_args: &[], releases: std::slice::from_ref(release) }));
        let dir = tempfile::tempdir().unwrap();
        let e = install_release(&Proxy::default().client(Duration::from_secs(20)).unwrap(), tool, release, dir.path()).await.unwrap_err();
        assert!(e.contains("Failed to download") || e.contains("checksum"), "{e}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing is left behind");
    }

    /// A helper on PATH is the one run, nothing installed; one with no build for this system is
    /// not installed but named for the user to install; a pool of proxies is refused first.
    #[tokio::test]
    async fn a_helper_on_path_comes_first_and_one_without_a_build_is_named() {
        static ON_PATH: Tool = Tool { program: if cfg!(windows) { "cmd" } else { "sh" }, licence: "-", home: "-", version_args: &[], releases: &[] };
        let found = ensure(&ON_PATH, Proxy::default()).await.unwrap();
        assert!(found.is_file(), "{}", found.display());
        let e = install(&FAKE, Proxy::default()).await.unwrap_err();
        assert!(e.contains("No hf-fake-helper build is published for this system") && e.contains("PATH"), "{e}");
    }

    /// Each helper follows the proxy setting or refuses: none takes turns through a pool; one that
    /// goes through no proxy refuses one set, or one the environment names unless no proxy at all
    /// is asked for.
    #[test]
    fn helpers_refuse_a_proxy_setting_they_cannot_follow() {
        let one = Proxy::single(Some("http://127.0.0.1:8080"));
        let pool = Proxy { pool: true, ..Proxy::default() };
        let direct = Proxy { direct: true, ..Proxy::default() };
        assert!(pool.check_in("rclone", true, false, false).unwrap_err().contains("proxy pool"));
        assert!(pool.check_in("DepotDownloader", false, false, false).unwrap_err().contains("proxy pool"));
        assert!(one.check_in("rclone", true, true, false).is_ok() && Proxy::default().check_in("rclone", true, true, true).is_ok());
        assert!(one.check_in("DepotDownloader", false, false, false).unwrap_err().contains("cannot go through a proxy"));
        assert!(Proxy::default().check_in("DepotDownloader", false, true, false).unwrap_err().contains("environment"));
        // The system's proxy: the refusal says how to turn it off.
        let e = Proxy::default().check_in("DepotDownloader", false, true, true).unwrap_err();
        assert!(e.contains("this computer's settings") && e.contains("Use the system's proxy") && e.contains("--system-proxy"), "{e}");
        assert!(direct.check_in("DepotDownloader", false, true, true).is_ok() && Proxy::default().check_in("DepotDownloader", false, false, false).is_ok());
        // With no proxy at all, a helper gets none of the environment's either.
        let cmd = command(Path::new("helper"), &[], direct, Path::new("."));
        let removed: Vec<_> = cmd.as_std().get_envs().filter(|(_, value)| value.is_none()).map(|(name, _)| name.to_string_lossy().into_owned()).collect();
        // Windows takes their names in either case as one.
        assert!(PROXY_VARS.iter().all(|var| removed.iter().any(|r| r.eq_ignore_ascii_case(var))), "{removed:?}");
    }

    /// Past its time limit, a helper's run is ended at once (with what it started).
    #[tokio::test]
    async fn a_helper_that_never_answers_is_ended_at_its_time_limit() {
        let dir = tempfile::tempdir().unwrap();
        let slow = fake_tool(dir.path(), "silent", "ping -n 30 127.0.0.1 >nul", "sleep 30");
        let started = Instant::now();
        let e = output(command(&slow, &[], Proxy::default(), dir.path()), "silent", Duration::from_secs(1)).await.unwrap_err();
        assert!(e.contains("no answer within 1 s"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
        let quick = fake_tool(dir.path(), "quick", "echo hello", "echo hello");
        let out = output(command(&quick, &[], Proxy::default(), dir.path()), "quick", Duration::from_secs(20)).await.unwrap();
        assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("hello"));
    }
}
