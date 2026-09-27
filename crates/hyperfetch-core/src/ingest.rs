//! Turns what the user enters (links, mirrors, magnet links, .metalink/.meta4/.torrent files and
//! URLs) into download tasks, the same way for every front end.

use std::path::PathBuf;
use std::time::Duration;

use url::Url;

use crate::{metalink, resolver, torrent};

/// Largest .metalink/.torrent document fetched from the network.
const MAX_DESCRIPTOR_BYTES: usize = 16 * 1024 * 1024;

pub const BLOB_MESSAGE: &str = "Browser-internal blob: URLs exist only in the browser's memory and cannot be downloaded by external tools. Copy the page URL from the address bar instead (e.g. https://www.youtube.com/watch?v=...).";

/// One file to download.
#[derive(Debug, Default, PartialEq)]
pub struct Task {
    /// Mirrors that all serve the same bytes.
    pub urls: Vec<Url>,
    /// File name (a relative path for metalinks and multi-file torrents) chosen by the input;
    /// None lets the server decide.
    pub name: Option<PathBuf>,
    /// Checksum published by a metalink.
    pub checksum: Option<String>,
}

impl Task {
    /// Short name for progress output.
    pub fn label(&self) -> String {
        if let Some(name) = &self.name {
            return name.to_string_lossy().into_owned();
        }
        self.urls
            .first()
            .map(|u| {
                u.path_segments()
                    .and_then(|mut s| s.next_back())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| u.host_str().unwrap_or("download").to_string())
            })
            .unwrap_or_else(|| "download".to_string())
    }
}

#[derive(Clone, Copy)]
enum Descriptor {
    Metalink,
    Torrent,
}

fn descriptor_kind(path: &str) -> Option<Descriptor> {
    let path = path.to_ascii_lowercase();
    if path.ends_with(".metalink") || path.ends_with(".meta4") {
        Some(Descriptor::Metalink)
    } else if path.ends_with(".torrent") {
        Some(Descriptor::Torrent)
    } else {
        None
    }
}

/// `token` as an http(s) URL.
pub fn http_url(token: &str) -> Option<Url> {
    Url::parse(token).ok().filter(|u| matches!(u.scheme(), "http" | "https"))
}

/// A `blob:` URL, or a YouTube URL ending in a blob UUID pasted without its prefix.
pub fn is_blob_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("blob:")
        || (s.contains("youtube.com")
            && s.rsplit('/').next().is_some_and(|last| last.len() == 36 && last.matches('-').count() == 4))
}

/// Where a token points to a .metalink/.meta4/.torrent document, if it does.
enum Source {
    Remote(Url),
    Local(PathBuf),
}

fn descriptor_source(token: &str) -> Option<(Source, Descriptor)> {
    if torrent::is_magnet_uri(token) {
        return None;
    }
    match http_url(token) {
        Some(url) => {
            let url = resolver::unwrap_redirect(&url).unwrap_or(url);
            descriptor_kind(url.path()).map(|kind| (Source::Remote(url), kind))
        }
        None => descriptor_kind(token).map(|kind| (Source::Local(PathBuf::from(token)), kind)),
    }
}

/// Whether `text` names a .metalink, .meta4 or .torrent (a URL or a local path, alone or among
/// other tokens). [`ingest`] reads it, which takes I/O a UI thread leaves to the runtime.
pub fn names_document(text: &str) -> bool {
    descriptor_source(&unquote(text)).is_some() || split_tokens(text).iter().any(|t| descriptor_source(t).is_some())
}

/// `name` (from a torrent, metalink or magnet, so untrusted) made safe as one path component on
/// every OS (see `engine::sanitize_component`). None when nothing is left, as for "." and "..".
fn clean_component(name: &str) -> Option<String> {
    Some(crate::engine::sanitize_component(name)).filter(|c| !c.is_empty())
}

/// A relative path built from untrusted name components, each cleaned by [`clean_component`].
fn clean_path<'a>(components: impl IntoIterator<Item = &'a str>) -> Result<PathBuf, String> {
    components
        .into_iter()
        .map(|c| clean_component(c).ok_or_else(|| format!("unusable file name {:?}", c)))
        .collect()
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches(['"', '\'']).to_string()
}

/// The whitespace-separated tokens of `line`, without quotes around them.
pub fn split_tokens(line: &str) -> Vec<String> {
    line.split_whitespace().map(unquote).filter(|t| !t.is_empty()).collect()
}

/// Splits one line of input into mirror tokens. A line naming a single existing local file (a
/// .torrent path with spaces, possibly quoted by drag and drop) stays one token; quotes around
/// tokens are removed.
pub async fn input_tokens(line: &str) -> Vec<String> {
    let whole = unquote(line);
    if tokio::fs::metadata(&whole).await.is_ok_and(|m| m.is_file()) {
        return vec![whole];
    }
    split_tokens(line)
}

/// Parses one input (a batch line split on whitespace, or the command-line URLs) into tasks.
///
/// The tokens are mirrors of one file (see [`link_task`]). A local or remote
/// .metalink/.meta4/.torrent must stand alone and may yield several tasks, one per file.
pub async fn ingest(tokens: &[impl AsRef<str>], http: &reqwest::Client) -> Result<Vec<Task>, String> {
    if let [token] = tokens {
        if let Some((source, kind)) = descriptor_source(token.as_ref()) {
            let bytes = match source {
                Source::Remote(url) => fetch(http, &url).await?,
                Source::Local(path) => tokio::fs::read(&path)
                    .await
                    .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?,
            };
            return match kind {
                Descriptor::Metalink => metalink_tasks(&bytes),
                Descriptor::Torrent => torrent_tasks(&bytes),
            };
        }
    }
    if let Some(token) = tokens.iter().map(|t| t.as_ref()).find(|t| descriptor_source(t).is_some()) {
        return Err(format!("{}: a .metalink, .meta4 or .torrent input must be on its own", token));
    }
    link_task(tokens).map(|task| vec![task])
}

/// The mirrors of one file named by links: http(s) URLs, each "leaving this site" link replaced
/// by its target, and magnet links with HTTP web seeds (the first one also names the file).
pub fn link_task(tokens: &[impl AsRef<str>]) -> Result<Task, String> {
    let mut task = Task::default();
    for token in tokens {
        let token = token.as_ref();
        if is_blob_url(token) {
            return Err(BLOB_MESSAGE.to_string());
        }
        let urls = if torrent::is_magnet_uri(token) {
            let magnet = torrent::parse_magnet_uri(token).map_err(|e| format!("Invalid magnet link: {}", e))?;
            if magnet.web_seeds.is_empty() {
                let name = magnet.display_name.map(|n| format!(" for \"{}\"", n)).unwrap_or_default();
                return Err(format!(
                    "The magnet link{} has no HTTP web seeds (ws=). Peer-to-peer BitTorrent transfers are not supported, so it cannot be downloaded.",
                    name
                ));
            }
            if task.name.is_none() {
                task.name = magnet.display_name.as_deref().and_then(clean_component).map(PathBuf::from);
            }
            magnet.web_seeds
        } else {
            let url = Url::parse(token).map_err(|e| format!("Invalid URL '{}': {}", truncate_chars(token, 80), e))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(format!("Unsupported URL scheme '{}': only http and https can be downloaded", url.scheme()));
            }
            vec![resolver::unwrap_redirect(&url).unwrap_or(url)]
        };
        for url in urls {
            if !task.urls.contains(&url) {
                task.urls.push(url);
            }
        }
    }
    if task.urls.is_empty() {
        return Err("Enter a download URL".to_string());
    }
    Ok(task)
}

/// The client [`ingest`] fetches remote .metalink/.torrent documents with, through `proxy` if set.
pub fn descriptor_client(proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("Endos-Unified-Downloader/", env!("CARGO_PKG_VERSION")));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| format!("invalid proxy: {}", e))?);
    }
    builder.build().map_err(|e| format!("cannot create HTTP client: {}", e))
}

async fn fetch(http: &reqwest::Client, url: &Url) -> Result<Vec<u8>, String> {
    let fail = |e: reqwest::Error| format!("Cannot fetch {}: {}", url, e);
    let mut resp = http.get(url.clone()).send().await.and_then(|r| r.error_for_status()).map_err(fail)?;
    let too_large = || format!("{} is larger than {} bytes", url, MAX_DESCRIPTOR_BYTES);
    if resp.content_length().is_some_and(|len| len > MAX_DESCRIPTOR_BYTES as u64) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(fail)? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_DESCRIPTOR_BYTES {
            return Err(too_large());
        }
    }
    Ok(body)
}

/// One task per metalink file, named by its relative path ("dir/file.iso" is saved in dir/).
fn metalink_tasks(bytes: &[u8]) -> Result<Vec<Task>, String> {
    let text = decode_text(bytes)?;
    metalink::parse_metalink(&text)?
        .into_iter()
        .map(|file| {
            if file.urls.is_empty() {
                return Err(format!("metalink file '{}' has no http(s) URLs", file.name));
            }
            let checksum = ["sha256", "md5"]
                .iter()
                .find_map(|algo| file.hashes.iter().find(|(t, _)| t == algo).map(|(t, h)| format!("{}:{}", t, h)));
            Ok(Task { name: Some(clean_path(file.name.split('/'))?), urls: file.urls, checksum })
        })
        .collect()
}

fn torrent_tasks(bytes: &[u8]) -> Result<Vec<Task>, String> {
    let info = torrent::parse_torrent_bytes(bytes)?;
    let single_file = matches!(&info.files[..], [f] if f.path == [info.name.clone()]);
    info.files
        .iter()
        .map(|file| {
            let relative = clean_path(file.path.iter().map(String::as_str))?;
            if file.urls.is_empty() {
                return Err(format!(
                    "torrent file '{}' has no HTTP web seeds (url-list); BitTorrent swarm downloads are not supported",
                    relative.display()
                ));
            }
            let name = if single_file { relative } else { clean_path([info.name.as_str()])?.join(relative) };
            Ok(Task { urls: file.urls.clone(), name: Some(name), checksum: None })
        })
        .collect()
}

/// Decodes a text file saved as UTF-8 (with or without BOM) or UTF-16 with a BOM.
pub fn decode_text(bytes: &[u8]) -> Result<String, String> {
    let utf16 = |data: &[u8], from: fn([u8; 2]) -> u16| {
        if !data.len().is_multiple_of(2) {
            return Err("invalid UTF-16 text (odd length)".to_string());
        }
        let units: Vec<u16> = data.chunks_exact(2).map(|c| from([c[0], c[1]])).collect();
        String::from_utf16(&units).map_err(|_| "invalid UTF-16 text".to_string())
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8(rest.to_vec()).map_err(|_| "invalid UTF-8 text".to_string()),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8(bytes.to_vec()).map_err(|_| "text is not UTF-8 or UTF-16 with a BOM".to_string()),
    }
}

/// At most `max` characters, ending in "..." when shortened. Never splits a character.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn run(line: &str) -> Result<Vec<Task>, String> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let tokens: Vec<&str> = line.split_whitespace().collect();
        rt.block_on(ingest(&tokens, &reqwest::Client::new()))
    }

    fn link(text: &str) -> Result<Task, String> {
        link_task(&split_tokens(text))
    }

    #[test]
    fn mirrors_form_one_task() {
        let tasks = run("https://a.example/f.iso  http://b.example/f.iso https://a.example/f.iso").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].urls.len(), 2);
        assert_eq!(tasks[0].name, None);
        assert_eq!(tasks[0].label(), "f.iso");
    }

    #[test]
    fn rejects_non_http_inputs() {
        assert!(run("ftp://x.example/f").unwrap_err().contains("scheme 'ftp'"));
        assert!(run("blob:https://www.youtube.com/abc").unwrap_err().contains("blob:"));
        assert!(run("C:\\file.bin").is_err());
        assert!(run("https://a.example/x.torrent https://b.example/y").unwrap_err().contains("on its own"));
    }

    #[test]
    fn links_are_mirrors_of_one_file_and_bad_input_is_refused() {
        let task = link("  https://a.com/f.iso\thttp://b.com/f.iso https://a.com/f.iso ").unwrap();
        assert_eq!(task.urls.len(), 2, "duplicates are dropped");
        assert_eq!(link("   ").unwrap_err(), "Enter a download URL");
        assert!(link("ftp://a.com/f").unwrap_err().contains("scheme"));
        assert!(link("not a url").unwrap_err().contains("Invalid URL"));
        assert_eq!(link("blob:https://www.youtube.com/x").unwrap_err(), BLOB_MESSAGE);
        assert_eq!(link("https://www.youtube.com/0b9f5e2c-1d3a-4c5e-9f7a-123456789abc").unwrap_err(), BLOB_MESSAGE);
        let err = link(&"x".repeat(100)).unwrap_err();
        assert!(err.contains(&format!("'{}...'", "x".repeat(77))), "long input is shortened: {}", err);
    }

    /// The queue and history hold the target of a "leaving this site" link, not the link itself.
    #[test]
    fn leaving_links_are_replaced_by_their_target() {
        let wrapped = "https://www.google.com/url?q=https%3A%2F%2Fwww.mediafire.com%2Ffile%2Fabc%2Fmod.zip";
        let url = Url::parse(wrapped).unwrap();
        let target = resolver::unwrap_redirect(&url).unwrap_or(url);
        assert_eq!(run(wrapped).unwrap()[0].urls, link(wrapped).unwrap().urls);
        assert_eq!(link(wrapped).unwrap().urls, [target]);
    }

    #[test]
    fn magnet_web_seeds_and_name() {
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let tasks = run(&format!("magnet:?xt=urn:btih:{}&dn=my+file.iso&ws=https%3A%2F%2Fseed.example%2Fdl%2F", hash)).unwrap();
        assert_eq!(tasks[0].name, Some(PathBuf::from("my file.iso")));
        assert_eq!(tasks[0].urls[0].as_str(), "https://seed.example/dl/my%20file.iso");

        let err = run(&format!("magnet:?xt=urn:btih:{}&dn=Ubuntu", hash)).unwrap_err();
        assert!(err.contains("\"Ubuntu\"") && err.contains("no HTTP web seeds"), "{}", err);
        assert!(run("magnet:?dn=x").unwrap_err().starts_with("Invalid magnet link"));
        // A magnet whose name ends in .torrent is still a magnet, not a file to read.
        let named = format!("magnet:?xt=urn:btih:{}&ws=https%3A%2F%2Fs.example%2Fa&dn=a.torrent", hash);
        assert!(!names_document(&named));
        assert_eq!(run(&named).unwrap()[0].name, Some(PathBuf::from("a.torrent")));
    }

    #[test]
    fn documents_are_told_from_links() {
        assert!(names_document("https://a.example/x.torrent"));
        assert!(names_document("https://a.example/list.META4?x=1"));
        assert!(names_document("\"C:\\My Files\\list.metalink\""));
        assert!(names_document("https://a.example/x.torrent https://b.example/y"), "refused later, off the UI thread");
        assert!(!names_document("https://a.example/x.iso https://b.example/x.iso"));
        assert!(!names_document("https://a.example/get?file=x.torrent"));
    }

    fn write_temp(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn local_metalink_yields_named_tasks_with_checksums() {
        let xml = r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink">
            <file name="a.bin"><hash type="sha-256">ABCDEF</hash><url>https://m1.example/a.bin</url></file>
            <file name="b.bin"><hash type="md5">0123</hash><url>https://m1.example/b.bin</url></file>
            </metalink>"#;
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(xml.as_bytes());
        let (_dir, path) = write_temp("list.meta4", &bytes);

        let tasks = run(path.to_str().unwrap()).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, Some(PathBuf::from("a.bin")));
        assert_eq!(tasks[0].checksum.as_deref(), Some("sha256:abcdef"));
        assert_eq!(tasks[1].checksum.as_deref(), Some("md5:0123"));
    }

    #[test]
    fn metalink_files_keep_their_folders() {
        let xml = r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink">
            <file name="dir/sub/file.iso"><url>https://m.example/file.iso</url></file>
            <file name="./top: level?.txt"><url>https://m.example/top.txt</url></file>
            <file name="con/aux.txt"><url>https://m.example/aux.txt</url></file>
            </metalink>"#;
        let names: Vec<_> = metalink_tasks(xml.as_bytes()).unwrap().into_iter().map(|t| t.name.unwrap()).collect();
        assert_eq!(
            names,
            [Path::new("dir").join("sub").join("file.iso"), PathBuf::from("top_ level_.txt"), Path::new("_con").join("_aux.txt")]
        );
        for bad in ["../escape.iso", "dir/../../escape.iso", "/etc/passwd", "C:\\Windows\\x.dll", "dir/.../x"] {
            let xml = format!(r#"<metalink><file name="{}"><url>https://m.example/x</url></file></metalink>"#, bad);
            assert!(metalink_tasks(xml.as_bytes()).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn multi_file_torrent_becomes_one_task_per_file() {
        let torrent = b"d8:url-list20:https://s.example/d/4:infod5:filesld6:lengthi3e4:pathl1:a5:x.binee\
d6:lengthi4e4:pathl5:y.bineee4:name4:root12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        let tasks = torrent_tasks(torrent).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, Some(Path::new("root").join("a").join("x.bin")));
        assert_eq!(tasks[1].urls[0].as_str(), "https://s.example/d/root/y.bin");

        let no_seeds = b"d4:infod6:lengthi3e4:name5:x.bin12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        assert!(torrent_tasks(no_seeds).unwrap_err().contains("no HTTP web seeds"));
    }

    #[test]
    fn untrusted_names_are_cleaned() {
        let name = "\u{1b}[31mRED\u{1b}[0m?.bin";
        let torrent = format!(
            "d8:url-list20:https://s.example/d/4:infod6:lengthi3e4:name{}:{}12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee",
            name.len(),
            name
        );
        let tasks = torrent_tasks(torrent.as_bytes()).unwrap();
        assert_eq!(tasks[0].name, Some(PathBuf::from("_[31mRED_[0m_.bin")));

        let hash = "0123456789abcdef0123456789abcdef01234567";
        let tasks = run(&format!("magnet:?xt=urn:btih:{}&dn=Ep+1%3A+Pilot%1B.mkv..&ws=https%3A%2F%2Fs.example%2Fx", hash)).unwrap();
        assert_eq!(tasks[0].name, Some(PathBuf::from("Ep 1_ Pilot_.mkv")));

        assert_eq!(clean_component(".."), None);
        assert_eq!(clean_component(" . "), None);
        // Names Windows keeps for devices never reach the disk as they are.
        assert_eq!(clean_component("con.txt").as_deref(), Some("_con.txt"));
        assert!(clean_path(["ok", ".."]).unwrap_err().contains("unusable"));
    }

    #[test]
    fn a_local_path_with_spaces_stays_one_token() {
        let (_dir, path) = write_temp("my file.torrent", b"x");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let plain = rt.block_on(input_tokens(path.to_str().unwrap()));
        let quoted = rt.block_on(input_tokens(&format!("\"{}\"", path.display())));
        let mirrors = rt.block_on(input_tokens("\"https://a.example/f\"  https://b.example/f"));
        assert_eq!(plain, [path.to_str().unwrap()]);
        assert_eq!(quoted, plain);
        assert_eq!(mirrors, ["https://a.example/f", "https://b.example/f"]);
        assert!(names_document(path.to_str().unwrap()));
    }

    #[test]
    fn text_decoding_handles_boms() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhttps://x\n").unwrap(), "https://x\n");
        let utf16le: Vec<u8> = [0xFF, 0xFE].into_iter().chain("a\nb".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(decode_text(&utf16le).unwrap(), "a\nb");
        let utf16be: Vec<u8> = [0xFE, 0xFF].into_iter().chain("é".encode_utf16().flat_map(u16::to_be_bytes)).collect();
        assert_eq!(decode_text(&utf16be).unwrap(), "é");
        assert!(decode_text(&[0xFF, 0xFE, 0x41]).is_err());
        assert!(decode_text(&[0xC3]).is_err());
    }

    #[test]
    fn documents_are_fetched_through_the_proxy_setting() {
        assert!(descriptor_client(None).is_ok());
        assert!(descriptor_client(Some("socks5://127.0.0.1:1080")).is_ok());
        assert!(descriptor_client(Some("::not a proxy::")).unwrap_err().starts_with("invalid proxy"));
    }

    #[test]
    fn truncate_chars_never_splits_characters() {
        let url = format!("https://ja.wikipedia.org/wiki/{}", "東京都の区市町村".repeat(5));
        let short = truncate_chars(&url, 55);
        assert_eq!(short.chars().count(), 55);
        assert!(short.ends_with("..."));
        assert_eq!(truncate_chars("héllo", 5), "héllo");
        assert_eq!(truncate_chars("héllo!", 5), "hé...");
    }
}
