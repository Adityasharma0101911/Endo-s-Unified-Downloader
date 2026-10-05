//! Image galleries (imgur, pixiv, DeviantArt, ...) downloaded with gallery-dl: the one on PATH,
//! else a managed install of its latest release, checked against the release's SHA256SUMS.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{DownloadOptions, EngineSnapshot, CANCELLED};
use crate::media::{exe_name, http_get, ProcessTree};

/// gallery-dl's newest release, as Codeberg's API describes it (its GitHub releases have had no
/// files since it moved there).
const LATEST: &str = "https://codeberg.org/api/v1/repos/mikf/gallery-dl/releases/latest";

/// Sites whose pages (and their subdomains') gallery-dl takes.
const SITES: &[&str] = &[
    "imgur.com", "pixiv.net", "deviantart.com", "artstation.com", "flickr.com", "tumblr.com",
    "danbooru.donmai.us", "gelbooru.com", "e621.net", "bsky.app",
];

/// Files that are pictures or videos themselves (`i.imgur.com/x.jpg`), which the engine fetches.
const MEDIA_FILES: &[&str] = &["jpg", "jpeg", "png", "gif", "gifv", "webp", "avif", "bmp", "mp4", "webm", "mov"];

/// The lowercase host of `url` without a trailing dot, and whether it is `site` or under it.
fn on(url: &Url, site: &str) -> bool {
    let host = url.host_str().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
    host == site || host.strip_suffix(site).is_some_and(|sub| sub.ends_with('.'))
}

/// Whether `url` is an image site's link gallery-dl takes: a page of one of [`SITES`], Pinterest
/// (every country's domain) or a Reddit gallery; not a picture's own file.
pub fn handles(url: &Url) -> bool {
    let file = url.path_segments().and_then(|mut s| s.next_back()).unwrap_or_default();
    let is_file = file.rsplit_once('.').is_some_and(|(_, ext)| MEDIA_FILES.contains(&ext.to_ascii_lowercase().as_str()));
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let pinterest = host.split('.').any(|label| label == "pinterest") || host == "pin.it";
    let reddit_gallery = on(url, "reddit.com") && url.path().starts_with("/gallery/");
    !is_file && (SITES.iter().any(|site| on(url, site)) || pinterest || reddit_gallery)
}

/// Whether `url` is an X/Twitter or Reddit post, which goes to gallery-dl when yt-dlp finds no
/// video in it.
pub(crate) fn takes_posts(url: &Url) -> bool {
    ["x.com", "twitter.com", "reddit.com", "redd.it"].iter().any(|site| on(url, site))
}

/// Downloads the gallery at `url` with gallery-dl into a folder named after it in `out_dir`, and
/// returns that folder and whether any file of it arrived now. Files a run before left there are
/// not downloaded again. None (logged) when there is no gallery-dl to run: none is built for this
/// system, or it would not install; the engine then takes the link as it would without it.
pub async fn run(
    url: &Url,
    options: &DownloadOptions,
    out_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Option<Result<(PathBuf, bool), String>> {
    let bin = tokio::select! {
        bin = gallery_dl(options.proxy.as_deref()) => bin,
        () = cancel.cancelled() => return Some(Err(CANCELLED.to_string())),
    };
    match bin {
        Ok(bin) => Some(run_with(&bin, url, options, out_dir, snapshots, cancel).await),
        Err(e) => {
            tracing::warn!("No gallery-dl for {url}, so it is downloaded without: {e}");
            None
        }
    }
}

/// The folder a gallery goes in: its site and path (`imgur.com a AbC`).
fn folder_name(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    let path = url.path_segments().into_iter().flatten().filter(|s| !s.is_empty());
    let parts: Vec<String> = std::iter::once(host.trim_start_matches("www.").to_string())
        .chain(path.map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()))
        .collect();
    Some(crate::engine::sanitize_filename(&parts.join(" "))).filter(|n| !n.is_empty()).unwrap_or_else(|| "gallery".into())
}

/// gallery-dl's arguments for `url`: every file straight into `folder`, with the download's
/// cookies and speed limit. The proxy goes in the environment (see `media::ytdlp_command`).
fn args(url: &Url, options: &DownloadOptions, folder: &Path) -> Vec<String> {
    let cookies = match (&options.browser_cookies, &options.cookies_path) {
        (Some(browser), _) => browser.to_args(),
        (None, Some(file)) => crate::media::BrowserCookieSource::File(file.clone()).to_args(),
        (None, None) => Vec::new(),
    };
    let mut args = vec!["--directory".to_string(), folder.to_string_lossy().into_owned()];
    args.extend(cookies);
    if let Some(rate) = options.max_speed {
        args.extend(["--limit-rate".to_string(), rate.to_string()]);
    }
    args.extend(["--".to_string(), url.to_string()]);
    args
}

/// [`run`] with the gallery-dl at `bin`. gallery-dl prints the path of each file it has (`# `
/// before one it already had); the snapshots count their bytes, and name the folder so the front
/// ends never take them for a live recording. Each file is marked as from the internet as it
/// comes, so one of a gallery stopped halfway is too. Cancelling kills its process tree; a
/// gallery-dl that fails after some files is an error that says so (they stay for the next run).
async fn run_with(
    bin: &Path,
    url: &Url,
    options: &DownloadOptions,
    out_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let folder = std::path::absolute(out_dir.join(folder_name(url))).map_err(|e| format!("Invalid folder {}: {e}", out_dir.display()))?;
    tokio::fs::create_dir_all(&folder).await.map_err(|e| format!("Failed to create {}: {e}", folder.display()))?;
    let mut cmd = crate::media::ytdlp_command(bin, &args(url, options, &folder), options.proxy.as_deref(), &folder);
    let mut child = cmd.spawn().map_err(|e| format!("Failed to run gallery-dl ({}): {e}", bin.display()))?;
    let mut tree = ProcessTree::attach(&child);
    let (Some(out), Some(mut err)) = (child.stdout.take(), child.stderr.take()) else {
        tree.kill_and_reap(&mut child).await;
        return Err("Failed to capture the output of gallery-dl".into());
    };
    // Drained alongside, so gallery-dl never blocks on a full pipe.
    let stderr = tokio::spawn(async move {
        let mut text = Vec::new();
        let _ = err.read_to_end(&mut text).await;
        String::from_utf8_lossy(&text).into_owned()
    });

    let (mut out, mut line, started) = (BufReader::new(out), Vec::new(), Instant::now());
    let (mut files, mut bytes, mut fresh) = (0usize, 0u64, false);
    let status = loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tree.kill_and_reap(&mut child).await;
                return Err(CANCELLED.to_string());
            }
            read = out.read_until(b'\n', &mut line) => {
                if !matches!(read, Ok(n) if n > 0) {
                    break tokio::select! {
                        status = child.wait() => status.map_err(|e| format!("Failed to wait on gallery-dl: {e}"))?,
                        () = cancel.cancelled() => {
                            tree.kill_and_reap(&mut child).await;
                            return Err(CANCELLED.to_string());
                        }
                    };
                }
                let text = String::from_utf8_lossy(&line).into_owned();
                line.clear();
                let path = folder.join(text.trim_end().trim_start_matches("# "));
                let Ok(meta) = tokio::fs::metadata(&path).await else { continue };
                files += 1;
                bytes += meta.len();
                fresh |= !text.starts_with("# ");
                if options.post.mark_of_the_web && path.starts_with(&folder) {
                    crate::postprocess::mark_from_internet(&path, url);
                }
                tracing::debug!("gallery-dl has {} ({files} files)", path.display());
                let speed = bytes as f64 / started.elapsed().as_secs_f64().max(0.001);
                emit(&snapshots, || EngineSnapshot { downloaded_bytes: bytes, speed_bytes_per_sec: speed, active_workers: 1, ..done(&folder, 0) });
            }
        }
    };
    tree.disarm();
    let stderr = stderr.await.unwrap_or_default();
    let errors: Vec<&str> = stderr.lines().filter(|l| l.contains("[error]")).collect();
    let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let failure = match (&errors[..], &tail[..]) {
        ([], []) => format!("gallery-dl exited with {status}"),
        ([], lines) => lines[lines.len().saturating_sub(5)..].join("\n"),
        (errors, _) => errors.join("\n"),
    };
    if files == 0 {
        // Only an empty folder goes.
        let _ = tokio::fs::remove_dir(&folder).await;
        return Err(if status.success() { format!("gallery-dl found nothing to download at {url}") } else { failure });
    }
    if !status.success() {
        return Err(format!("gallery-dl stopped after {files} files (kept in {}; run it again for the rest): {failure}", folder.display()));
    }
    emit(&snapshots, || EngineSnapshot { downloaded_bytes: bytes, progress_ratio: 1.0, ..done(&folder, bytes) });
    Ok((folder, fresh))
}

/// A snapshot of `size` bytes in `folder`.
fn done(folder: &Path, size: u64) -> EngineSnapshot {
    EngineSnapshot { total_bytes: size, target_path: Some(folder.to_path_buf()), ..Default::default() }
}

fn emit(tx: &Option<broadcast::Sender<EngineSnapshot>>, make: impl FnOnce() -> EngineSnapshot) {
    if let Some(tx) = tx {
        let _ = tx.send(make());
    }
}

/// gallery-dl's release file for this system; None where it has none (macOS, ARM Linux), where it
/// is found on PATH (see `media::prepare_macos`) or not at all.
fn asset() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86")) {
        Some("gallery-dl_x86.exe")
    } else if cfg!(windows) {
        Some("gallery-dl.exe")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("gallery-dl.bin")
    } else {
        None
    }
}

/// The gallery-dl to run: on PATH, else the managed one, installed now if need be. Never one
/// next to the application, which may be the save folder (a portable copy in Downloads), where
/// any download could put a `gallery-dl.exe`.
async fn gallery_dl(proxy: Option<&str>) -> Result<PathBuf, String> {
    // Serializes installs across concurrent galleries.
    static INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let found = || {
        let name = exe_name("gallery-dl");
        let managed = crate::media::managed_bin_dir().map(|dir| dir.join(&name));
        crate::media::find_in_path(&name).or_else(|| managed.filter(|p| p.is_file()))
    };
    if let Ok(Some(bin)) = tokio::task::spawn_blocking(found).await {
        return Ok(bin);
    }
    let _guard = INSTALL_LOCK.lock().await;
    if let Ok(Some(bin)) = tokio::task::spawn_blocking(found).await {
        return Ok(bin);
    }
    let bin_dir = crate::media::managed_bin_dir().ok_or("Cannot determine a per-user directory to install gallery-dl into")?;
    install(&crate::media::http_client(proxy)?, LATEST, &bin_dir).await
}

/// Installs the release file of [`asset`] that `api` (Codeberg's description of a release) lists
/// into `bin_dir`, once its SHA-256 matches the one the release's `SHA256SUMS` gives: written next
/// to its place and renamed into it, so no half-written gallery-dl is ever run. A file the sums
/// leave out is refused.
async fn install(client: &reqwest::Client, api: &str, bin_dir: &Path) -> Result<PathBuf, String> {
    let how = if cfg!(target_os = "macos") { "brew install gallery-dl, or pip install gallery-dl" } else { "pip install gallery-dl" };
    let asset = asset().ok_or_else(|| format!("There is no gallery-dl build for this system: install gallery-dl ({how}) and try again"))?;
    let body = http_get(client, api, Duration::from_secs(30)).await?.bytes().await.map_err(|e| format!("Failed to read {api}: {e}"))?;
    let release: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("Unreadable gallery-dl release: {e}"))?;
    // Absolute in Codeberg's; a relative one is on the server of `api`.
    let link = |name: &str| {
        release["assets"]
            .as_array()
            .and_then(|files| files.iter().find(|f| f["name"] == name))
            .and_then(|f| Url::parse(api).ok()?.join(f["browser_download_url"].as_str()?).ok())
            .ok_or_else(|| format!("The latest gallery-dl release has no {name}"))
    };
    let (url, sums_url) = (link(asset)?, link("SHA256SUMS")?);
    let sums = http_get(client, sums_url.as_str(), Duration::from_secs(30)).await?.text().await.map_err(|e| format!("Failed to read {sums_url}: {e}"))?;
    let expected = crate::media::expected_sha256(&sums, asset)
        .ok_or_else(|| format!("The gallery-dl SHA256SUMS has no entry for {asset}"))?
        .to_ascii_lowercase();
    let bytes = http_get(client, url.as_str(), Duration::from_secs(600))
        .await?
        .bytes()
        .await
        .map_err(|e| format!("Failed to download {url}: {e}"))?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected {
        return Err(format!("Downloaded gallery-dl failed checksum verification (expected {expected}, got {actual})"));
    }
    let target = bin_dir.join(exe_name("gallery-dl"));
    let tmp = bin_dir.join(format!(".gallery-dl.{}.tmp", crate::media::unique_suffix()));
    let (to, from) = (target.clone(), tmp.clone());
    let installed = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(from.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(&from, &bytes)?;
        #[cfg(not(windows))]
        crate::media::make_executable(&from)?;
        crate::media::rename_patiently(&from, &to)
    })
    .await
    .map_err(|e| format!("gallery-dl install task failed: {e}"))?;
    if let Err(e) = installed {
        let _ = std::fs::remove_file(&tmp);
        // Another process installed it meanwhile.
        if !target.is_file() {
            return Err(format!("Failed to install gallery-dl to {}: {e}", target.display()));
        }
    }
    tracing::info!("Installed gallery-dl {} to {}", release["tag_name"].as_str().unwrap_or("(unknown version)"), target.display());
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn handles_galleries_not_their_files() {
        for yes in [
            "https://imgur.com/a/AbC",
            "https://www.pixiv.net/en/artworks/123",
            "https://someone.deviantart.com/gallery",
            "https://www.artstation.com/artwork/xyz",
            "https://www.flickr.com/photos/me/albums/1",
            "https://blog.tumblr.com/post/1",
            "https://www.pinterest.co.uk/me/board/",
            "https://danbooru.donmai.us/posts/1",
            "https://gelbooru.com/index.php?page=post&s=view&id=1",
            "https://e621.net/posts/1",
            "https://bsky.app/profile/me.bsky.social/post/1",
            "https://www.reddit.com/gallery/abc",
        ] {
            assert!(handles(&url(yes)), "{yes}");
        }
        for no in [
            "https://i.imgur.com/AbC.jpg",
            "https://64.media.tumblr.com/x/s1280x1920/y.png",
            "https://www.reddit.com/r/pics/comments/abc/title/",
            "https://notimgur.com/a/AbC",
            "https://example.com/gallery/abc",
        ] {
            assert!(!handles(&url(no)), "{no}");
        }
        assert!(takes_posts(&url("https://x.com/me/status/1")) && takes_posts(&url("https://old.reddit.com/r/a/comments/b/")));
        assert!(!takes_posts(&url("https://notx.com/me/status/1")));
    }

    #[test]
    fn no_video_errors_send_posts_to_gallery_dl() {
        use crate::media::found_no_video;
        assert!(found_no_video("ERROR: [twitter] 123: No video could be found in this tweet"));
        assert!(found_no_video("ERROR: Unsupported URL: https://www.reddit.com/gallery/abc"));
        assert!(!found_no_video("ERROR: [twitter] 123: NSFW tweet requires authentication"));
        assert!(!found_no_video(CANCELLED));
    }

    #[test]
    fn args_put_files_in_the_folder_with_the_downloads_cookies() {
        let folder = Path::new("/saves/imgur.com a AbC");
        let options = DownloadOptions {
            browser_cookies: Some(crate::media::BrowserCookieSource::Firefox),
            cookies_path: Some("/ignored.txt".into()),
            max_speed: Some(500_000),
            ..Default::default()
        };
        let link = url("https://imgur.com/a/AbC");
        assert_eq!(folder_name(&link), "imgur.com a AbC");
        assert_eq!(
            args(&link, &options, folder),
            ["--directory", "/saves/imgur.com a AbC", "--cookies-from-browser", "firefox", "--limit-rate", "500000", "--", "https://imgur.com/a/AbC"]
        );
        let plain = args(&link, &DownloadOptions { cookies_path: Some("/c.txt".into()), ..Default::default() }, folder);
        assert_eq!(plain[2..4], ["--cookies", "/c.txt"]);
        assert_eq!(folder_name(&url("https://www.pixiv.net/en/artworks/12%3F3")), "pixiv.net en artworks 12_3");
    }

    /// Codeberg's description of a release `/<name>` whose sums are at `/<name>/SHA256SUMS`; the
    /// links are relative, so they lead to the server the description comes from.
    fn release(name: &str) -> (String, Vec<u8>) {
        let asset = asset().unwrap();
        let json = serde_json::json!({ "tag_name": "v1.0.0", "assets": [
            { "name": "gallery-dl.tar.gz", "browser_download_url": "/other" },
            { "name": asset, "browser_download_url": format!("/{asset}") },
            { "name": "SHA256SUMS", "browser_download_url": format!("/{name}/SHA256SUMS") },
        ]});
        (format!("/{name}"), json.to_string().into_bytes())
    }

    #[tokio::test]
    async fn install_takes_only_the_file_its_sums_name() {
        let Some(asset) = asset() else { return };
        let program = b"gallery-dl build".to_vec();
        let sums = |hash: String| format!("00  gallery-dl.tar.gz\n{hash}  {asset}\n").into_bytes();
        let files = vec![
            release("good"),
            ("/good/SHA256SUMS".into(), sums(format!("{:X}", Sha256::digest(&program)))),
            release("bad"),
            ("/bad/SHA256SUMS".into(), sums(format!("{:x}", Sha256::digest(b"something else")))),
            release("none"),
            ("/none/SHA256SUMS".into(), b"00  gallery-dl.tar.gz\n".to_vec()),
            (format!("/{asset}"), program.clone()),
        ];
        let base = crate::media::tests::serve_files(files, None).await;
        let base = base.trim_end_matches("/release");
        let client = crate::media::http_client(None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(exe_name("gallery-dl"));

        for (api, error) in [("/bad", "checksum"), ("/none", "no entry")] {
            let e = install(&client, &format!("{base}{api}"), dir.path()).await.unwrap_err();
            assert!(e.contains(error), "{e}");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing is left behind");
        }
        assert_eq!(install(&client, &format!("{base}/good"), dir.path()).await.unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), program);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temporary file is left");
    }

    /// Installs the real latest gallery-dl into a temporary folder and runs it.
    #[tokio::test]
    #[ignore = "network"]
    async fn live_install_runs() {
        let dir = tempfile::tempdir().unwrap();
        let bin = install(&crate::media::http_client(None).unwrap(), LATEST, dir.path()).await.unwrap();
        let out = std::process::Command::new(&bin).arg("--version").output().unwrap();
        assert!(out.status.success() && !out.stdout.is_empty(), "{out:?}");
    }

    /// A stand-in for gallery-dl that writes `one.jpg` into its working directory (the gallery's
    /// folder), prints its path and that of a file it "already had", then waits `wait` seconds.
    fn fake_gallery_dl(dir: &Path, wait: u32) -> PathBuf {
        #[cfg(windows)]
        let (bin, script) = (
            dir.join("fake.cmd"),
            format!("@echo off\r\necho picture>one.jpg\r\necho %CD%\\one.jpg\r\necho # %CD%\\one.jpg\r\nping -n {} 127.0.0.1 >nul\r\n", wait + 1),
        );
        #[cfg(not(windows))]
        let (bin, script) = (
            dir.join("fake.sh"),
            format!("#!/bin/sh\necho picture > one.jpg\necho \"$PWD/one.jpg\"\necho \"# $PWD/one.jpg\"\nsleep {wait}\n"),
        );
        std::fs::write(&bin, script).unwrap();
        #[cfg(not(windows))]
        crate::media::make_executable(&bin).unwrap();
        bin
    }

    #[tokio::test]
    async fn run_reports_each_file_and_returns_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gallery_dl(dir.path(), 0);
        let (tx, mut rx) = broadcast::channel(16);
        let link = url("https://imgur.com/a/AbC");
        let saves = dir.path().join("saves");
        let (folder, fresh) = run_with(&bin, &link, &DownloadOptions::default(), &saves, Some(tx), CancellationToken::new()).await.unwrap();
        assert_eq!((folder.clone(), fresh), (std::path::absolute(saves.join("imgur.com a AbC")).unwrap(), true));
        if cfg!(windows) {
            assert!(std::fs::metadata(format!("{}:Zone.Identifier", folder.join("one.jpg").display())).is_ok(), "marked as it came");
        }
        let size = std::fs::metadata(folder.join("one.jpg")).unwrap().len();
        let mut seen = Vec::new();
        while let Ok(s) = rx.try_recv() {
            assert_eq!(s.target_path.as_deref(), Some(folder.as_path()));
            assert!(!s.is_recording());
            seen.push((s.downloaded_bytes, s.total_bytes));
        }
        assert_eq!(seen, [(size, 0), (2 * size, 0), (2 * size, 2 * size)]);
    }

    #[tokio::test]
    async fn cancelling_kills_gallery_dl_and_an_empty_run_fails() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gallery_dl(dir.path(), 30);
        let (tx, mut rx) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let link = url("https://imgur.com/a/AbC");
        let options = DownloadOptions::default();
        let run = run_with(&bin, &link, &options, dir.path(), Some(tx), cancel.clone());
        let stop = async {
            rx.recv().await.unwrap();
            cancel.cancel();
        };
        let started = Instant::now();
        let (result, ()) = tokio::join!(run, stop);
        assert_eq!(result.unwrap_err(), CANCELLED);
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());

        // One that prints nothing, and leaves no folder behind.
        #[cfg(windows)]
        let silent = dir.path().join("silent.cmd");
        #[cfg(not(windows))]
        let silent = dir.path().join("silent.sh");
        std::fs::write(&silent, if cfg!(windows) { "@echo off\r\n" } else { "#!/bin/sh\n" }).unwrap();
        #[cfg(not(windows))]
        crate::media::make_executable(&silent).unwrap();
        let other = url("https://imgur.com/a/Empty");
        let e = run_with(&silent, &other, &options, dir.path(), None, CancellationToken::new()).await.unwrap_err();
        assert!(e.contains("found nothing"), "{e}");
        assert!(!dir.path().join("imgur.com a Empty").exists());

        // One that fails after a file: an error that says so, the file kept.
        #[cfg(windows)]
        let failing = (dir.path().join("failing.cmd"), "@echo off\r\necho x>one.jpg\r\necho %CD%\\one.jpg\r\necho [error] 404>&2\r\nexit /b 4\r\n");
        #[cfg(not(windows))]
        let failing = (dir.path().join("failing.sh"), "#!/bin/sh\necho x > one.jpg\necho \"$PWD/one.jpg\"\necho '[error] 404' >&2\nexit 4\n");
        std::fs::write(&failing.0, failing.1).unwrap();
        #[cfg(not(windows))]
        crate::media::make_executable(&failing.0).unwrap();
        let e = run_with(&failing.0, &url("https://imgur.com/a/Part"), &options, dir.path(), None, CancellationToken::new()).await.unwrap_err();
        assert!(e.starts_with("gallery-dl stopped after 1 files") && e.ends_with("[error] 404"), "{e}");
        assert!(dir.path().join("imgur.com a Part").join("one.jpg").is_file());
    }
}
