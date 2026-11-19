//! Live streams yt-dlp cannot follow, through streamlink (BSD-2-Clause): tried only when yt-dlp
//! failed on a media link and a streamlink already installed says it takes the link, or when the
//! user picks it (`DownloadOptions::streamlink`, which installs it if need be). The stream's best
//! quality is written to a `.ts` file in the download's folder, named by streamlink from the
//! stream's author, title and time, until it ends, or until the download's time or size limit
//! (`record_seconds`, `max_filesize`, by what streamlink says and by the file's size) or a stop,
//! which keep what was recorded; also when less than `media::MIN_FREE_SPACE` is left on the disk,
//! which keeps the file but fails, naming it, as a yt-dlp live recording does.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{DownloadOptions, EngineSnapshot, CANCELLED};
use crate::helpers::{self, Progress, Proxy, Release, Tool};

/// streamlink 8.6.3: its Windows portable build and Linux AppImages (none is published for
/// macOS), with the SHA-256 digests GitHub gives their release files.
pub static STREAMLINK: Tool = Tool {
    program: "streamlink",
    licence: "BSD-2-Clause",
    home: "https://streamlink.github.io",
    version_args: &["--version"],
    releases: &[
        Release {
            target: "windows-x86_64",
            version: "8.6.3-1",
            url: "https://github.com/streamlink/windows-builds/releases/download/8.6.3-1/streamlink-8.6.3-1-py315-x86_64.zip",
            sha256: "3b1f1470d4b135080a8107ca2917379ca3ff5acadbcdc215568b79eea1450497",
            size: 84_611_513,
        },
        Release {
            target: "linux-x86_64",
            version: "8.6.3-1",
            url: "https://github.com/streamlink/streamlink-appimage/releases/download/8.6.3-1/streamlink-8.6.3-1-cp315-cp315-manylinux_2_28_x86_64.AppImage",
            sha256: "ccee9ba370c607330a1c9fb7fd645c63972cc6cf7dda206ec9fc2c22e994429a",
            size: 39_791_096,
        },
        Release {
            target: "linux-aarch64",
            version: "8.6.3-1",
            url: "https://github.com/streamlink/streamlink-appimage/releases/download/8.6.3-1/streamlink-8.6.3-1-cp315-cp315-manylinux_2_28_aarch64.AppImage",
            sha256: "eacc8ccf35f1530b4aa565a2e6c37bf95319ecfe4d7e0114fd6a1d24a9c74aa0",
            size: 38_730_248,
        },
    ],
};

/// When streamlink is used.
pub const NOTE: &str = "Used only for a live stream yt-dlp cannot follow, or when picked for a download; no account.";

/// streamlink with `args`, run in `dir`, the proxy in its environment. An AppImage runs unpacked,
/// without FUSE, into a temporary folder in the app's own folder for it, which no other user of
/// the computer can open (in the shared one, another could put a program of theirs where it is
/// unpacked to, under the name it will have).
fn command(bin: &Path, args: &[String], proxy: Proxy<'_>, dir: &Path) -> Result<Command, String> {
    let mut cmd = helpers::command(bin, args, proxy, dir);
    cmd.env("APPIMAGE_EXTRACT_AND_RUN", "1");
    if cfg!(target_os = "linux") {
        cmd.env("TMPDIR", helpers::own_dir("streamlink")?);
    }
    Ok(cmd)
}

/// Whether a streamlink already installed takes `url` (one of its plugins does) and can follow
/// the download's proxy setting. Installs nothing.
pub(crate) async fn takes(url: &Url, proxy: Proxy<'_>) -> bool {
    if proxy.check(STREAMLINK.program, true).is_err() {
        return false;
    }
    let Some(bin) = tokio::task::spawn_blocking(|| helpers::find(&STREAMLINK)).await.ok().flatten() else { return false };
    let Ok(cmd) = command(&bin, &["--can-handle-url".to_string(), url.to_string()], proxy, &std::env::temp_dir()) else { return false };
    helpers::output(cmd, "streamlink", Duration::from_secs(30)).await.is_ok_and(|out| out.status.success())
}

/// streamlink's arguments that record the best quality of `url` into the folder it runs in: the
/// name is a template of streamlink's, so the folder's own name (which may hold `{` or `}`) stays
/// out of it.
fn args(url: &Url) -> Vec<String> {
    let name = "{author} - {title} {time:%Y-%m-%d %H-%M-%S}.ts";
    vec!["--progress=force".to_string(), "--output".into(), name.to_string(), url.to_string(), "best".into()]
}

/// Reads one line of streamlink's output into `progress` (`[download] Written 12.34 MiB to ...
/// (5s @ 1.23 MiB/s)`), and the file it writes into `file` (the line after "Writing output to").
fn parse(line: &str, progress: &mut Progress, file: &mut Option<PathBuf>, next_is_file: &mut bool) {
    if std::mem::take(next_is_file) {
        *file = Some(PathBuf::from(line.trim()));
        return;
    }
    if line.contains("Writing output to") {
        *next_is_file = true;
        return;
    }
    let Some((_, rest)) = line.split_once("[download] ") else { return };
    let rest = rest.strip_prefix("Written ").unwrap_or(rest);
    let written = rest.split([' ', '(']).take(2).collect::<Vec<_>>().join(" ");
    progress.done = helpers::parse_size(&written).unwrap_or(progress.done);
    if let Some((_, speed)) = rest.split_once(" @ ") {
        progress.speed = helpers::parse_size(speed.trim_end_matches(')')).map(|s| s as f64);
    }
}

/// Records the stream at `url` with streamlink into `save_dir` (see the module); returns the file
/// and whether anything was recorded.
pub(crate) async fn run(
    url: &Url,
    options: &DownloadOptions,
    save_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let proxy = Proxy::of(options);
    proxy.check(STREAMLINK.program, true)?;
    let bin = helpers::ensure(&STREAMLINK, proxy).await?;
    tokio::fs::create_dir_all(save_dir).await.map_err(|e| format!("Cannot create {}: {e}", save_dir.display()))?;
    record(command(&bin, &args(url), proxy, save_dir)?, options, save_dir, snapshots, cancel, crate::media::MIN_FREE_SPACE).await
}

/// Runs the recording `cmd` (see the module), ended as a stop ends it when less than `margin` is
/// left on its disk.
async fn record(
    cmd: Command,
    options: &DownloadOptions,
    save_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
    margin: u64,
) -> Result<(PathBuf, bool), String> {
    // The time and size limits, and a disk running full, end the recording as a stop does.
    let stop = cancel.child_token();
    let timer = options.record_seconds.map(|secs| {
        let stop = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            stop.cancel();
        })
    });
    let (writing, disk_full) = (Arc::new(parking_lot::Mutex::new(None::<PathBuf>)), Arc::new(parking_lot::Mutex::new(None::<u64>)));
    let watcher = {
        let (stop, writing, disk_full, limit) = (stop.clone(), Arc::clone(&writing), Arc::clone(&disk_full), options.max_filesize);
        tokio::spawn(async move {
            let mut every = tokio::time::interval(crate::media::DISK_CHECK_EVERY);
            loop {
                every.tick().await;
                let Some(file) = writing.lock().clone() else { continue };
                let Some((free, recorded)) = tokio::task::spawn_blocking(move || crate::media::room_left(&[file])).await.ok().flatten() else { continue };
                if free < margin {
                    tracing::warn!("Only {free} bytes are left on the disk of a live recording: ending it");
                    *disk_full.lock() = Some(free);
                }
                if free < margin || limit.is_some_and(|max| recorded >= max) {
                    stop.cancel();
                    break;
                }
            }
        })
    };
    let (mut file, mut next_is_file, limit, ended) = (None, false, options.max_filesize, stop.clone());
    let ran = helpers::run(
        cmd,
        "streamlink",
        save_dir,
        |line, p| {
            parse(line, p, &mut file, &mut next_is_file);
            // A name relative to the folder streamlink runs in.
            if let Some(name) = file.as_mut().filter(|f| f.is_relative()) {
                *name = save_dir.join(&*name);
            }
            writing.lock().clone_from(&file);
            if limit.is_some_and(|max| p.done >= max) {
                ended.cancel();
            }
        },
        snapshots,
        stop,
    )
    .await;
    if let Some(timer) = timer {
        timer.abort();
    }
    watcher.abort();
    let recorded = file.filter(|f| std::fs::metadata(f).is_ok_and(|m| m.len() > 0));
    if let (Some(free), Some(file)) = (*disk_full.lock(), &recorded) {
        return crate::media::ended(Ok(file.clone()), Some(free)).map(|file| (file, true));
    }
    match (ran, recorded) {
        (Ok(_), Some(file)) => Ok((file, true)),
        (Ok(tail), None) => Err(format!("streamlink recorded nothing: {}", tail.last().map(String::as_str).unwrap_or("no message"))),
        // A stop, or a limit, keeps what was recorded.
        (Err(e), Some(file)) if e == CANCELLED => Ok((file, true)),
        (Err(e), _) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamlinks_output_is_read() {
        let (mut p, mut file, mut next) = (Progress::default(), None, false);
        for line in [
            "[cli][info] Found matching plugin twitch for URL https://www.twitch.tv/someone",
            "[cli][info] Writing output to",
            "D:\\Videos\\someone - Live 2026-11-19 10-00-00.ts",
            "[download] Written 12.50 MiB to D:\\Videos\\someone - Li... (5s @ 2.50 MiB/s)",
        ] {
            parse(line, &mut p, &mut file, &mut next);
        }
        assert_eq!(file.as_deref(), Some(Path::new("D:\\Videos\\someone - Live 2026-11-19 10-00-00.ts")));
        assert_eq!((p.done, p.speed), (13_107_200, Some(2_621_440.0)));
        parse("[download] 1.00 GiB (1h02m03s)", &mut p, &mut file, &mut next);
        assert_eq!((p.done, p.speed), (1 << 30, Some(2_621_440.0)));
        let args = args(&Url::parse("https://www.twitch.tv/someone").unwrap());
        assert_eq!(args[0], "--progress=force");
        assert!(args[2] == "{author} - {title} {time:%Y-%m-%d %H-%M-%S}.ts" && args[4] == "best", "no folder in the template: {args:?}");
        // The AppImage unpacks into the app's own folder for it, never the shared temporary one.
        let cmd = command(Path::new("streamlink"), &[], Proxy::default(), Path::new(".")).unwrap();
        let tmp = cmd.as_std().get_envs().find(|(name, _)| *name == "TMPDIR").and_then(|(_, value)| value.map(Path::new));
        assert_eq!(tmp, cfg!(target_os = "linux").then(|| helpers::account_dir("streamlink")).as_deref());
    }

    /// A fake streamlink that writes a file and keeps recording: the size limit ends it, keeping
    /// the file; one that ends with nothing written is an error.
    #[tokio::test]
    async fn a_recording_ends_at_its_size_limit_and_keeps_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("live.ts");
        let bin = helpers::tests::fake_tool(
            dir.path(),
            "streamlink",
            &format!("echo [cli][info] Writing output to\necho {0}\necho recorded>\"{0}\"\necho [download] Written 2.00 MiB (1s @ 2.00 MiB/s)\nping -n 30 127.0.0.1 >nul", out.display()),
            &format!("echo '[cli][info] Writing output to'\necho '{0}'\necho recorded > '{0}'\necho '[download] Written 2.00 MiB (1s @ 2.00 MiB/s)'\nsleep 30", out.display()),
        );
        let options = DownloadOptions { max_filesize: Some(1 << 20), ..Default::default() };
        let started = std::time::Instant::now();
        let cmd = command(&bin, &[], Proxy::default(), dir.path()).unwrap();
        let (file, fresh) = record(cmd, &options, dir.path(), None, CancellationToken::new(), 0).await.unwrap();
        assert_eq!((file, fresh), (out.clone(), true));
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());

        let empty = helpers::tests::fake_tool(dir.path(), "empty", "echo error: No playable streams found on this URL", "echo 'error: No playable streams found on this URL'");
        let e = record(command(&empty, &[], Proxy::default(), dir.path()).unwrap(), &DownloadOptions::default(), dir.path(), None, CancellationToken::new(), 0).await.unwrap_err();
        assert!(e.contains("No playable streams"), "{e}");
    }

    /// A recording that says nothing of its size still ends at the size limit, by its file's size;
    /// one whose disk runs out of room ends too, keeping its file but failing, naming it. Its name
    /// is relative to the folder it runs in.
    #[tokio::test]
    async fn a_recording_ends_by_its_files_size_and_before_the_disk_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let bin = helpers::tests::fake_tool(
            dir.path(),
            "quiet",
            "echo [cli][info] Writing output to\necho live.ts\necho recorded>live.ts\nping -n 30 127.0.0.1 >nul",
            "echo '[cli][info] Writing output to'\necho 'live.ts'\necho recorded > live.ts\nsleep 30",
        );
        let started = std::time::Instant::now();
        let options = DownloadOptions { max_filesize: Some(3), ..Default::default() };
        let (file, _) = record(command(&bin, &[], Proxy::default(), dir.path()).unwrap(), &options, dir.path(), None, CancellationToken::new(), 0).await.unwrap();
        assert_eq!(file, dir.path().join("live.ts"));
        let full = record(command(&bin, &[], Proxy::default(), dir.path()).unwrap(), &DownloadOptions::default(), dir.path(), None, CancellationToken::new(), u64::MAX).await.unwrap_err();
        assert!(full.contains(crate::media::RECORDING_KEPT) && full.contains("live.ts"), "{full}");
        assert!(dir.path().join("live.ts").exists());
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
    }
}
