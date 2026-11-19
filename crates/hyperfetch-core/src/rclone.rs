//! Cloud drives (Google Drive, OneDrive, Dropbox, Box, pCloud and the other backends rclone
//! knows) through rclone, with a config file of the app's own (see `helpers::account_dir`): the
//! user's other rclone setups are never read or changed. A drive is set up by rclone's own
//! sign-in (it opens the browser for the drive's OAuth page); its folders list as downloads with
//! `lsjson` (`remote:path` or `rclone://remote/path` links), each with its size and the hash the
//! drive keeps; and its files are downloaded by the engine itself, with its connections, ranges,
//! resume and checksums, from rclone's remote control server on 127.0.0.1, which serves every
//! drive of the config (`rcd --rc-serve`) under a user and password made for that run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, BufReader};
use url::Url;

use crate::helpers::{self, Release, Tool};
use crate::ingest::Task;
use crate::media::ProcessTree;

/// rclone (MIT), v1.76.0: the files and SHA-256 sums of its signed SHA256SUMS.
pub static RCLONE: Tool = Tool {
    program: "rclone",
    licence: "MIT",
    home: "https://rclone.org",
    version_args: &["version"],
    releases: &[
        Release {
            target: "windows-x86_64",
            version: "v1.76.0",
            url: "https://github.com/rclone/rclone/releases/download/v1.76.0/rclone-v1.76.0-windows-amd64.zip",
            sha256: "8b4fd2737ade4e70c057ef4a49deb7cffe94b0fb9d7587c189d149823f21a1dc",
            size: 32_193_987,
        },
        Release {
            target: "windows-aarch64",
            version: "v1.76.0",
            url: "https://github.com/rclone/rclone/releases/download/v1.76.0/rclone-v1.76.0-windows-arm64.zip",
            sha256: "c406c8a69a5a408e50624aa990fbb0a6266b3a4e83782a94de3cb96f50f86f10",
            size: 29_049_527,
        },
        Release {
            target: "linux-x86_64",
            version: "v1.76.0",
            url: "https://github.com/rclone/rclone/releases/download/v1.76.0/rclone-v1.76.0-linux-amd64.zip",
            sha256: "b00f00b3266c3764413afe599d3ec316f6f43b3e0ea8a0bc1d186d2133e46df7",
            size: 32_194_302,
        },
        Release {
            target: "linux-aarch64",
            version: "v1.76.0",
            url: "https://github.com/rclone/rclone/releases/download/v1.76.0/rclone-v1.76.0-linux-arm64.zip",
            sha256: "ee169016675e23a69d77240565133ff7dbc9b33d16fa8f7053d4af39f1eaeb78",
            size: 29_231_141,
        },
        Release {
            target: "macos-x86_64",
            version: "v1.76.0",
            url: "https://github.com/rclone/rclone/releases/download/v1.76.0/rclone-v1.76.0-osx-amd64.zip",
            sha256: "4f5835733d52b818340f97b1fc0348a70461534d123363a183ca2b660219595a",
            size: 35_749_026,
        },
        Release {
            target: "macos-aarch64",
            version: "v1.76.0",
            url: "https://github.com/rclone/rclone/releases/download/v1.76.0/rclone-v1.76.0-osx-arm64.zip",
            sha256: "e72dc90b70f4947a7b72b61bde4a4fbba5777b2874fb448951ffdde67ebd0ab7",
            size: 33_189_281,
        },
    ],
};

/// The app's own rclone config file.
pub fn config_file() -> PathBuf {
    helpers::account_dir("rclone").join("rclone.conf")
}

/// The names of the drives (`[name]` sections) of the rclone config `text`.
fn remotes_in(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix('[')?.strip_suffix(']').map(str::to_string))
        .filter(|name| !name.is_empty())
        .collect()
}

/// The drives set up in the app's config (see [`config_file`]). Blocking.
pub fn remotes() -> Vec<String> {
    std::fs::read_to_string(config_file()).map(|text| remotes_in(&text)).unwrap_or_default()
}

/// What a drive's name may be made of (rclone's own rule).
fn remote_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || " _.+@-".contains(c)) && !name.starts_with([' ', '-']) && !name.ends_with(' ')
}

/// What of a drive's name or path is written as `%XX` in an `rclone://` link.
const LINK_PART: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// The `rclone://` link of `path` on the drive `remote`.
pub fn link(remote: &str, path: &str) -> Option<Url> {
    let parts: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(|s| percent_encoding::utf8_percent_encode(s, LINK_PART).to_string()).collect();
    Url::parse(&format!("rclone://{}/{}", percent_encoding::utf8_percent_encode(remote, LINK_PART), parts.join("/"))).ok()
}

/// The drive and path an `rclone://` link names.
pub fn parts(url: &Url) -> Option<(String, String)> {
    let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
    let remote = decode(url.host_str()?);
    let path: Vec<String> = url.path().split('/').filter(|s| !s.is_empty()).map(decode).collect();
    (url.scheme() == "rclone" && remote_name(&remote)).then(|| (remote, path.join("/")))
}

/// Whether `token` has the shape of a drive's `remote:path` (a name of one letter is a Windows
/// drive's; `scheme://` is a link's).
pub(crate) fn may_be_remote(token: &str) -> bool {
    token.split_once(':').is_some_and(|(remote, path)| remote.chars().count() >= 2 && remote_name(remote) && !path.starts_with("//"))
}

/// `token` as an `rclone://` link: one already, or `remote:path` of a drive the app's config
/// has. Blocking (it reads the config).
pub fn link_of(token: &str) -> Option<Url> {
    if let Some(url) = Url::parse(token).ok().filter(|u| u.scheme() == "rclone") {
        return parts(&url).and_then(|(remote, path)| link(&remote, &path));
    }
    let (remote, path) = token.split_once(':').filter(|_| may_be_remote(token))?;
    remotes().iter().any(|r| r == remote).then(|| link(remote, path)).flatten()
}

/// One item of `rclone lsjson`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Entry {
    path: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    is_dir: bool,
    #[serde(default)]
    hashes: HashMap<String, String>,
}

/// The checksum of the strongest of `hashes` the engine checks a file with.
fn checksum(hashes: &HashMap<String, String>) -> Option<String> {
    ["sha256", "sha1", "md5"].iter().find_map(|algo| hashes.get(*algo).filter(|h| !h.is_empty()).map(|h| format!("{algo}:{h}")))
}

/// The downloads `rclone lsjson` gave as `listed` for the folder `path` of `remote`: each file
/// under the folder's name, with its size and checksum.
fn tasks(remote: &str, path: &str, listed: &[Entry]) -> Vec<Task> {
    let folder = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(remote);
    listed
        .iter()
        .filter(|e| !e.is_dir)
        .filter_map(|e| {
            let full = format!("{}/{}", path.trim_end_matches('/'), e.path);
            let name = crate::ingest::clean_path(std::iter::once(folder).chain(e.path.split('/').filter(|s| !s.is_empty()))).ok()?;
            Some(Task { urls: vec![link(remote, &full)?], name: Some(name), size: u64::try_from(e.size).ok(), checksum: checksum(&e.hashes), ..Task::default() })
        })
        .collect()
}

/// Longest an `lsjson` of a large drive may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Runs rclone (installed now if need be) with `args` and the app's config, and returns what it
/// printed, or its last error line when it fails.
async fn rclone(args: &[&str], proxy: Option<&str>, limit: Duration) -> Result<Vec<u8>, String> {
    let bin = helpers::ensure(&RCLONE, proxy).await?;
    run_with(&bin, args, proxy, limit).await
}

async fn run_with(bin: &Path, args: &[&str], proxy: Option<&str>, limit: Duration) -> Result<Vec<u8>, String> {
    let dir = helpers::own_dir("rclone")?;
    let mut all: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    all.extend(["--config".to_string(), dir.join("rclone.conf").to_string_lossy().into_owned()]);
    let mut cmd = helpers::command(bin, &all, proxy, &dir);
    let output = match tokio::time::timeout(limit, cmd.output()).await {
        Ok(output) => output.map_err(|e| format!("Failed to run rclone: {e}"))?,
        Err(_) => return Err(format!("rclone gave no answer within {} s", limit.as_secs())),
    };
    if !output.status.success() {
        let text = String::from_utf8_lossy(&output.stderr);
        let line = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no message");
        return Err(format!("rclone failed: {}", line.trim()));
    }
    Ok(output.stdout)
}

/// The downloads of the `rclone://` link `url`: its file, or every file under its folder.
pub async fn list(url: &Url, proxy: Option<&str>) -> Result<Vec<Task>, String> {
    let (remote, path) = parts(url).ok_or_else(|| format!("{url} names no drive"))?;
    let target = format!("{remote}:{path}");
    let stat = rclone(&["lsjson", "--stat", "--hash", target.as_str()], proxy, LIST_TIMEOUT).await?;
    let item: Entry = serde_json::from_slice(&stat).map_err(|e| format!("Unreadable rclone listing of {target}: {e}"))?;
    if !item.is_dir {
        let name = crate::ingest::clean_path([item.path.as_str()]).ok();
        return Ok(vec![Task { urls: vec![url.clone()], name, size: u64::try_from(item.size).ok(), checksum: checksum(&item.hashes), ..Task::default() }]);
    }
    let listed = rclone(&["lsjson", "-R", "--files-only", "--hash", target.as_str()], proxy, LIST_TIMEOUT).await?;
    let entries: Vec<Entry> = serde_json::from_slice(&listed).map_err(|e| format!("Unreadable rclone listing of {target}: {e}"))?;
    Ok(tasks(&remote, &path, &entries))
}

/// rclone's remote control server of this process, while a download uses it: dropped with the
/// last, which stops it.
pub(crate) struct Served {
    /// Where it serves the drives (`http://127.0.0.1:port/`).
    pub(crate) base: Url,
    /// The Authorization header it takes, made for this run.
    pub(crate) auth: String,
    _process: parking_lot::Mutex<(tokio::process::Child, ProcessTree)>,
}

impl Served {
    /// Where it serves `path` of `remote` (`/[remote:]/path`).
    pub(crate) fn url(&self, remote: &str, path: &str) -> Option<Url> {
        served_url(&self.base, remote, path)
    }
}

fn served_url(base: &Url, remote: &str, path: &str) -> Option<Url> {
    let mut url = base.clone();
    {
        let mut segments = url.path_segments_mut().ok()?;
        segments.clear().push(&format!("[{remote}:]"));
        for part in path.split('/').filter(|s| !s.is_empty()) {
            segments.push(part);
        }
    }
    Some(url)
}

static SERVER: tokio::sync::Mutex<Weak<Served>> = tokio::sync::Mutex::const_new(Weak::new());

/// The running server (see [`Served`]), started now if none is.
pub(crate) async fn served(proxy: Option<&str>) -> Result<Arc<Served>, String> {
    let mut held = SERVER.lock().await;
    if let Some(served) = held.upgrade() {
        return Ok(served);
    }
    let bin = helpers::ensure(&RCLONE, proxy).await?;
    let served = Arc::new(start(&bin, proxy).await?);
    *held = Arc::downgrade(&served);
    Ok(served)
}

/// Starts the server on the port it had last time, so a download stopped then resumes (its
/// links name the port); on another one when that is taken.
async fn start(bin: &Path, proxy: Option<&str>) -> Result<Served, String> {
    let dir = helpers::own_dir("rclone")?;
    let saved = std::fs::read_to_string(dir.join("port")).ok().and_then(|p| p.trim().parse::<u16>().ok());
    match launch(bin, &dir, proxy, saved.unwrap_or(0)).await {
        Err(e) if saved.is_some() => {
            tracing::info!("rclone could not serve on port {} again ({e}); another port", saved.unwrap_or(0));
            launch(bin, &dir, proxy, 0).await
        }
        other => other,
    }
}

/// The address in rclone's line that says where its remote control server listens.
fn serving_url(line: &str) -> Option<Url> {
    let (_, rest) = line.split_once("Serving remote control on ")?;
    Url::parse(rest.trim()).ok().filter(|u| u.host_str() == Some("127.0.0.1"))
}

/// How long rclone may take to say where it serves.
const START_TIMEOUT: Duration = Duration::from_secs(30);

async fn launch(bin: &Path, dir: &Path, proxy: Option<&str>, port: u16) -> Result<Served, String> {
    let (user, pass) = (helpers::random_hex(8), helpers::random_hex(16));
    let config = dir.join("rclone.conf").to_string_lossy().into_owned();
    let args = vec!["rcd".to_string(), "--rc-serve".into(), "--rc-addr".into(), format!("127.0.0.1:{port}"), "--config".into(), config];
    let mut cmd = helpers::command(bin, &args, proxy, dir);
    // In its environment, where other users cannot read them, not on its command line.
    cmd.env("RCLONE_RC_USER", &user).env("RCLONE_RC_PASS", &pass).stdout(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("Failed to run rclone ({}): {e}", bin.display()))?;
    helpers::lower_priority(&child);
    let tree = ProcessTree::attach(&child);
    let stderr = child.stderr.take().ok_or("Failed to capture the output of rclone")?;
    let mut lines = BufReader::new(stderr).lines();
    let found = tokio::time::timeout(START_TIMEOUT, async {
        let mut last = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(url) = serving_url(&line) {
                return Ok(url);
            }
            last = line;
        }
        Err(format!("rclone stopped: {}", last.trim()))
    })
    .await
    .map_err(|_| format!("rclone did not start serving within {} s", START_TIMEOUT.as_secs()))??;
    // Read on, so rclone never waits on a full pipe.
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!("rclone: {line}");
        }
    });
    if let Some(port) = found.port() {
        let _ = std::fs::write(dir.join("port"), port.to_string());
    }
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")));
    Ok(Served { base: found, auth, _process: parking_lot::Mutex::new((child, tree)) })
}

/// What a drive's sign-in (`config create`) printed: the page it opens, which the user opens by
/// hand when the browser did not come up.
fn auth_page(line: &str) -> Option<String> {
    let at = line.find("http://127.0.0.1:")?;
    let url = line[at..].split_whitespace().next()?;
    url.contains("/auth").then(|| url.to_string())
}

/// Sets up the drive `name` of the backend `kind` (`drive`, `onedrive`, `dropbox`, ...) with
/// rclone's own sign-in, which opens the browser at the drive's page and waits for the user (5
/// minutes at most); `show` is told the page's address. Returns the drives set up.
pub async fn sign_in(name: &str, kind: &str, proxy: Option<&str>, mut show: impl FnMut(String)) -> Result<String, String> {
    if !remote_name(name) || name.chars().count() < 2 {
        return Err(format!("{name:?} cannot name a drive: letters, digits, spaces and _ . + @ - only, two characters at least"));
    }
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!("{kind:?} is no rclone backend (drive, onedrive, dropbox, box, pcloud, ...)"));
    }
    let bin = helpers::ensure(&RCLONE, proxy).await?;
    let dir = helpers::own_dir("rclone")?;
    let config = dir.join("rclone.conf").to_string_lossy().into_owned();
    let args = vec!["config".to_string(), "create".into(), name.into(), kind.into(), "config_is_local=true".into(), "--config".into(), config];
    let mut cmd = helpers::command(&bin, &args, proxy, &dir);
    let mut child = cmd.spawn().map_err(|e| format!("Failed to run rclone: {e}"))?;
    let mut tree = ProcessTree::attach(&child);
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        tree.kill_and_reap(&mut child).await;
        return Err("Failed to capture the output of rclone".into());
    };
    let (mut out, mut err) = (BufReader::new(out).lines(), BufReader::new(err).lines());
    let mut last = String::new();
    let waited = tokio::time::timeout(Duration::from_secs(300), async {
        let (mut out_open, mut err_open) = (true, true);
        while out_open || err_open {
            let line = tokio::select! {
                line = out.next_line(), if out_open => line.ok().flatten().or_else(|| { out_open = false; None }),
                line = err.next_line(), if err_open => line.ok().flatten().or_else(|| { err_open = false; None }),
            };
            let Some(line) = line else { continue };
            if let Some(page) = auth_page(&line) {
                show(page);
            }
            if !line.trim().is_empty() {
                last = line;
            }
        }
        child.wait().await
    })
    .await;
    let status = match waited {
        Ok(status) => status.map_err(|e| format!("Failed to wait on rclone: {e}"))?,
        Err(_) => {
            tree.kill_and_reap(&mut child).await;
            return Err("The drive's sign-in was not finished within 5 minutes".to_string());
        }
    };
    tree.disarm();
    if !status.success() {
        return Err(format!("rclone could not set up {name}: {}", last.trim()));
    }
    Ok(remotes().join(", "))
}

/// Removes the drive `name` from the app's config (rclone forgets its sign-in with it).
pub async fn sign_out(name: &str, proxy: Option<&str>) -> Result<(), String> {
    rclone(&["config", "delete", name], proxy, Duration::from_secs(60)).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_name_a_drive_and_a_path() {
        assert_eq!(remotes_in("[gdrive]\ntype = drive\ntoken = {}\n\n[my box]\ntype = box\n"), ["gdrive", "my box"]);
        let url = link("my box", "Films/Old #1.mkv").unwrap();
        assert_eq!(url.as_str(), "rclone://my%20box/Films/Old%20%231.mkv");
        assert_eq!(parts(&url).unwrap(), ("my box".to_string(), "Films/Old #1.mkv".to_string()));
        assert_eq!(link_of("rclone://gdrive/a/b.txt").unwrap().as_str(), "rclone://gdrive/a/b.txt");
        assert!(link_of("C:\\Users\\x.txt").is_none() && link_of("https://example.com/x").is_none());
        assert!(link_of("nosuchdrive:x").is_none(), "only a drive the config has");
        assert!(!remote_name("-x") && !remote_name("a/b") && remote_name("work_2.0"));
    }

    #[test]
    fn a_listing_becomes_downloads_with_sizes_and_hashes() {
        let json = r#"[
            {"Path":"a.txt","Name":"a.txt","Size":3,"MimeType":"text/plain","ModTime":"2026-01-01T00:00:00Z","IsDir":false,"Hashes":{"md5":"900150983cd24fb0d6963f7d28e17f72","sha1":"a9993e364706816aba3e25717850c26c9cd0d89d"}},
            {"Path":"sub","Name":"sub","Size":-1,"IsDir":true},
            {"Path":"sub/b c.bin","Name":"b c.bin","Size":-1,"IsDir":false}
        ]"#;
        let entries: Vec<Entry> = serde_json::from_str(json).unwrap();
        let listed = tasks("gdrive", "Docs", &entries);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].urls[0].as_str(), "rclone://gdrive/Docs/a.txt");
        assert_eq!(listed[0].name.as_deref(), Some(Path::new("Docs/a.txt")));
        assert_eq!((listed[0].size, listed[0].checksum.as_deref()), (Some(3), Some("sha1:a9993e364706816aba3e25717850c26c9cd0d89d")));
        assert_eq!(listed[1].urls[0].as_str(), "rclone://gdrive/Docs/sub/b%20c.bin");
        assert_eq!((listed[1].size, listed[1].checksum.as_deref()), (None, None));
        assert_eq!(tasks("gdrive", "", &entries)[0].name.as_deref(), Some(Path::new("gdrive/a.txt")), "a drive's top under its name");
    }

    #[test]
    fn the_server_says_where_it_serves_and_serves_each_drive_under_its_name() {
        let line = "2026/11/19 10:00:00 NOTICE: Serving remote control on http://127.0.0.1:53682/";
        let base = serving_url(line).unwrap();
        assert_eq!(base.as_str(), "http://127.0.0.1:53682/");
        assert!(serving_url("NOTICE: Serving remote control on http://0.0.0.0:5572/").is_none());
        let url = served_url(&base, "my box", "Films/a b.mkv").unwrap();
        assert!(url.as_str().starts_with("http://127.0.0.1:53682/") && url.as_str().ends_with("/Films/a%20b.mkv"), "{url}");
        let decoded = percent_encoding::percent_decode_str(url.path()).decode_utf8_lossy().into_owned();
        assert_eq!(decoded, "/[my box:]/Films/a b.mkv");
        let page = "If your browser doesn't open automatically go to the following link: http://127.0.0.1:53682/auth?state=abc";
        assert_eq!(auth_page(page).as_deref(), Some("http://127.0.0.1:53682/auth?state=abc"));
        assert!(auth_page("Waiting for code...").is_none());
    }

    /// The server is a stand-in that says it serves at a test server's address: the engine then
    /// downloads a drive's file from there, with the session's sign-in, and a resumed download
    /// finds the port of the time before.
    #[tokio::test]
    async fn a_drives_file_downloads_through_the_server() {
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let served = body.clone();
        let (addr, hits) = crate::hls::tests::serve(move |path, range| {
            let decoded = percent_encoding::percent_decode_str(path).decode_utf8_lossy().into_owned();
            if decoded != "/[gdrive:]/Films/a.bin" {
                return (404, String::new(), Vec::new());
            }
            match range {
                Some(r) => (206, format!("Content-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\n", r.start, r.end, served.len()), served[r.start as usize..=r.end as usize].to_vec()),
                None => (200, "Accept-Ranges: bytes\r\n".to_string(), served.clone()),
            }
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let fake = helpers::tests::fake_tool(
            dir.path(),
            "rclone",
            &format!("echo NOTICE: Serving remote control on http://127.0.0.1:{}/ 1>&2\nping -n 30 127.0.0.1 >nul", addr.port()),
            &format!("echo \"NOTICE: Serving remote control on http://127.0.0.1:{}/\" >&2\nsleep 30", addr.port()),
        );
        let server = Arc::new(launch(&fake, dir.path(), None, 0).await.unwrap());
        assert_eq!(server.base.port(), Some(addr.port()));
        assert!(server.auth.starts_with("Basic "));
        assert_eq!(std::fs::read_to_string(dir.path().join("port")).unwrap(), addr.port().to_string());
        let url = server.url("gdrive", "Films/a.bin").unwrap();
        let options = crate::engine::DownloadOptions {
            output_path: Some(dir.path().join("out").join("")),
            auth_header: Some(server.auth.clone()),
            no_proxy: true,
            remote_checksum: false,
            ..Default::default()
        };
        let path = crate::engine::DownloadEngine::new(vec![url], options).run(None).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert!(hits.lock().keys().any(|k| k.ends_with("/Films/a.bin")));
        drop(server);
    }
}
