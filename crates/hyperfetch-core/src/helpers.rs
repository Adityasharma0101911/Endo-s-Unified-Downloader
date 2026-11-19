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

/// Serializes installs of helper programs.
static INSTALLING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The `tool` to run (see [`find`]), its pinned release installed now if it is not there.
pub async fn ensure(tool: &'static Tool, proxy: Option<&str>) -> Result<PathBuf, String> {
    if let Ok(Some(found)) = tokio::task::spawn_blocking(move || find(tool)).await {
        return Ok(found);
    }
    install(tool, proxy).await
}

/// Installs `tool`'s pinned release for this system into the managed folder (see
/// [`install_release`]), unless it is there already. Returns the program to run.
pub async fn install(tool: &'static Tool, proxy: Option<&str>) -> Result<PathBuf, String> {
    let release = tool
        .release()
        .ok_or_else(|| format!("No {0} build is published for this system: install {0} yourself and put it on PATH ({1})", tool.program, tool.home))?;
    let _guard = INSTALLING.lock().await;
    if let Ok(Some(found)) = tokio::task::spawn_blocking(move || find_managed(tool)).await {
        return Ok(found);
    }
    let bin_dir = crate::media::managed_bin_dir().ok_or("Cannot determine a per-user folder to install helper programs into")?;
    install_release(&crate::media::http_client(proxy)?, tool, release, &bin_dir).await
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
/// the proxy in its environment (see `media::ytdlp_command`) and its output piped.
pub(crate) fn command(bin: &Path, args: &[String], proxy: Option<&str>, work_dir: &Path) -> Command {
    #[allow(unused_mut)]
    let mut cmd = crate::media::ytdlp_command(bin, args, proxy, work_dir);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    cmd
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

/// A random string of `bytes` random bytes in hex: a session's password for a local server.
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // Without the system's randomness, the time and process make one unique enough to differ.
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut buf).is_err() {
        let seed = Sha256::digest(crate::media::unique_suffix().as_bytes());
        buf.iter_mut().zip(seed.iter().cycle()).for_each(|(b, s)| *b = *s);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
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
        assert_eq!(random_hex(16).len(), 32);
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
        let cmd = command(&bin, &[], None, dir.path());
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
        let e = run(command(&failing, &[], None, dir.path()), "failing", dir.path(), parse, None, CancellationToken::new()).await.unwrap_err();
        assert!(e.starts_with("failing exited with") && e.ends_with("broken"), "{e}");

        let slow = fake_tool(dir.path(), "slow", "echo 1 of 100\nping -n 30 127.0.0.1 >nul", "echo 1 of 100\nsleep 30");
        let cancel = CancellationToken::new();
        let (tx, mut rx) = broadcast::channel(16);
        let stop = async {
            rx.recv().await.unwrap();
            cancel.cancel();
        };
        let started = Instant::now();
        let (result, ()) = tokio::join!(run(command(&slow, &[], None, dir.path()), "slow", dir.path(), parse, Some(tx), cancel.clone()), stop);
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
}
