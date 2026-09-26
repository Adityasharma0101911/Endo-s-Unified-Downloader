//! Turns user input (command-line URLs, batch-file lines, interactive input) into download tasks.

use std::path::{Path, PathBuf};

use hyperfetch_core::{metalink, torrent};
use url::Url;

/// Largest .metalink/.torrent document fetched from the network.
const MAX_DESCRIPTOR_BYTES: usize = 16 * 1024 * 1024;

/// One file to download.
#[derive(Debug, Default, PartialEq)]
pub struct Task {
    /// Mirrors that all serve the same bytes.
    pub urls: Vec<Url>,
    /// File name (a relative path for multi-file torrents) chosen by the input; None lets the
    /// server decide.
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

pub fn http_url(token: &str) -> Option<Url> {
    Url::parse(token).ok().filter(|u| matches!(u.scheme(), "http" | "https"))
}

/// Where a token points to a .metalink/.meta4/.torrent document, if it does.
enum Source {
    Remote(Url),
    Local(PathBuf),
}

fn descriptor_source(token: &str) -> Option<(Source, Descriptor)> {
    match http_url(token) {
        Some(url) => descriptor_kind(url.path()).map(|kind| (Source::Remote(url), kind)),
        None => descriptor_kind(token).map(|kind| (Source::Local(PathBuf::from(token)), kind)),
    }
}

/// A magnet display name usable as a file name (no directories, no traversal).
fn safe_file_name(name: &str) -> Option<&str> {
    let name = name.trim();
    (!name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0'])).then_some(name)
}

/// Parses one input (a batch line split on whitespace, or the command-line URLs) into tasks.
///
/// The tokens are mirrors of one file: http(s) URLs and magnet links (their web
/// seeds). A local or remote .metalink/.meta4/.torrent must stand alone and may yield several
/// tasks, one per file.
pub async fn ingest(tokens: &[&str], http: &reqwest::Client) -> Result<Vec<Task>, String> {
    if let [token] = tokens {
        if let Some((source, kind)) = descriptor_source(token) {
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

    let mut task = Task::default();
    for &token in tokens {
        if descriptor_source(token).is_some() {
            return Err(format!("{}: a .metalink, .meta4 or .torrent input must be on its own", token));
        }
        let urls = if token.starts_with("blob:") {
            return Err(format!(
                "{}: blob: URLs only exist inside the browser tab; copy the page URL from the address bar instead",
                token
            ));
        } else if torrent::is_magnet_uri(token) {
            let magnet = torrent::parse_magnet_uri(token)?;
            if magnet.web_seeds.is_empty() {
                return Err(format!(
                    "magnet {} has no HTTP web seeds (ws=); BitTorrent swarm downloads are not supported",
                    magnet.info_hash
                ));
            }
            if task.name.is_none() {
                task.name = magnet.display_name.as_deref().and_then(safe_file_name).map(PathBuf::from);
            }
            magnet.web_seeds
        } else {
            vec![http_url(token).ok_or_else(|| format!("'{}' is not an http(s) URL, magnet link, metalink or torrent", token))?]
        };
        for url in urls {
            if !task.urls.contains(&url) {
                task.urls.push(url);
            }
        }
    }
    if task.urls.is_empty() {
        return Err("no URL given".to_string());
    }
    Ok(vec![task])
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

fn metalink_tasks(bytes: &[u8]) -> Result<Vec<Task>, String> {
    let text = decode_text(bytes)?;
    let files = metalink::parse_metalink(&text)?;
    if files.is_empty() {
        return Err("the metalink lists no files".to_string());
    }
    files
        .into_iter()
        .map(|file| {
            if file.urls.is_empty() {
                return Err(format!("metalink file '{}' has no http(s) URLs", file.name));
            }
            let checksum = ["sha256", "md5"]
                .iter()
                .find_map(|algo| file.hashes.iter().find(|(t, _)| t == algo).map(|(t, h)| format!("{}:{}", t, h)));
            Ok(Task { urls: file.urls, name: Some(PathBuf::from(file.name)), checksum })
        })
        .collect()
}

fn torrent_tasks(bytes: &[u8]) -> Result<Vec<Task>, String> {
    let info = torrent::parse_torrent_bytes(bytes)?;
    let single_file = matches!(&info.files[..], [f] if f.path == [info.name.clone()]);
    info.files
        .iter()
        .map(|file| {
            let relative: PathBuf = file.path.iter().collect();
            if file.urls.is_empty() {
                return Err(format!(
                    "torrent file '{}' has no HTTP web seeds (url-list); BitTorrent swarm downloads are not supported",
                    relative.display()
                ));
            }
            let name = if single_file { relative } else { Path::new(&info.name).join(relative) };
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

/// The meaningful lines of a batch file: trimmed, without blanks and # comments.
pub fn batch_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(line: &str) -> Result<Vec<Task>, String> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let tokens: Vec<&str> = line.split_whitespace().collect();
        rt.block_on(ingest(&tokens, &reqwest::Client::new()))
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
        assert!(run("ftp://x.example/f").unwrap_err().contains("not an http(s) URL"));
        assert!(run("blob:https://www.youtube.com/abc").unwrap_err().contains("blob:"));
        assert!(run("C:\\file.bin").is_err());
        assert!(run("https://a.example/x.torrent https://b.example/y").unwrap_err().contains("on its own"));
    }

    #[test]
    fn magnet_web_seeds_and_name() {
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let tasks = run(&format!("magnet:?xt=urn:btih:{}&dn=my+file.iso&ws=https%3A%2F%2Fseed.example%2Fdl%2F", hash)).unwrap();
        assert_eq!(tasks[0].name, Some(PathBuf::from("my file.iso")));
        assert_eq!(tasks[0].urls[0].as_str(), "https://seed.example/dl/my%20file.iso");

        let err = run(&format!("magnet:?xt=urn:btih:{}&dn=x", hash)).unwrap_err();
        assert!(err.contains("no HTTP web seeds"), "{}", err);
    }

    #[test]
    fn local_metalink_yields_named_tasks_with_checksums() {
        let dir = std::env::temp_dir().join(format!("hf-cli-ingest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("list.meta4");
        let xml = r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink">
            <file name="a.bin"><hash type="sha-256">ABCDEF</hash><url>https://m1.example/a.bin</url></file>
            <file name="b.bin"><hash type="md5">0123</hash><url>https://m1.example/b.bin</url></file>
            </metalink>"#;
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(xml.as_bytes());
        std::fs::write(&path, bytes).unwrap();

        let tasks = run(path.to_str().unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, Some(PathBuf::from("a.bin")));
        assert_eq!(tasks[0].checksum.as_deref(), Some("sha256:abcdef"));
        assert_eq!(tasks[1].checksum.as_deref(), Some("md5:0123"));
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
    fn batch_lines_skip_comments_and_blanks() {
        let lines: Vec<&str> = batch_lines("# queue\r\n\r\n  https://a  https://b \r\n#x\nhttps://c").collect();
        assert_eq!(lines, ["https://a  https://b", "https://c"]);
    }
}
