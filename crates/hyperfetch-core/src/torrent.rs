use percent_encoding::percent_decode_str;
use url::Url;

/// Nesting deeper than this is rejected instead of recursing (real torrents use < 10 levels).
const MAX_BENCODE_DEPTH: usize = 64;

#[derive(Debug, Clone)]
pub struct MagnetInfo {
    pub info_hash: String,
    pub display_name: Option<String>,
    pub trackers: Vec<String>,
    /// Direct HTTP(S) URLs of the file. Directory web seeds (ending in '/') already have
    /// the display name appended (BEP 19); ones without a usable name are dropped.
    pub web_seeds: Vec<Url>,
}

#[derive(Debug, Clone)]
pub struct TorrentFile {
    /// Path components below the torrent's root directory (just the name for single-file torrents).
    pub path: Vec<String>,
    pub length: u64,
    /// URLs this file can be downloaded from, built from the web seeds per BEP 19.
    pub urls: Vec<Url>,
}

#[derive(Debug, Clone)]
pub struct TorrentInfo {
    pub name: String,
    pub total_length: u64,
    pub piece_length: u64,
    pub num_pieces: usize,
    /// Raw `url-list` entries as listed in the torrent; use `files[..].urls` to download.
    pub web_seeds: Vec<Url>,
    pub trackers: Vec<String>,
    pub files: Vec<TorrentFile>,
}

pub fn is_magnet_uri(s: &str) -> bool {
    s.trim().starts_with("magnet:?")
}

/// A single path component that cannot escape the download directory.
fn is_safe_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', ':', '\0'])
}

/// BEP 19: a web seed ending in '/' is a directory; the torrent name (and, for
/// multi-file torrents, the file's path) is appended to it. `None` for URLs that
/// cannot carry a path.
fn web_seed_file_url(seed: &Url, components: &[&str]) -> Option<Url> {
    let mut url = seed.clone();
    url.path_segments_mut().ok()?.pop_if_empty().extend(components);
    Some(url)
}

/// Parses a Magnet URI (RFC draft / BEP 9)
pub fn parse_magnet_uri(uri: &str) -> Result<MagnetInfo, String> {
    let trimmed = uri.trim();
    let query = trimmed
        .strip_prefix("magnet:?")
        .ok_or_else(|| "Not a valid magnet URI (must start with magnet:?)".to_string())?;

    let mut info_hash = String::new();
    let mut display_name = None;
    let mut trackers = Vec::new();
    let mut seeds: Vec<(Url, bool)> = Vec::new(); // (url, is a BEP 19 web seed)

    for pair in query.split('&') {
        let (key, val) = pair.split_once('=').unwrap_or((pair, ""));
        // Indexed keys such as `tr.1` or `ws.2` mean the same as the plain key.
        let key = key.split('.').next().unwrap_or(key);
        let decoded = percent_decode_str(val).decode_utf8_lossy().into_owned();

        match key {
            "xt" => {
                let hash = decoded.strip_prefix("urn:btih:").or_else(|| decoded.strip_prefix("urn:btmh:"));
                // Prefer the v1 (btih) hash when both are present.
                if let Some(hash) = hash {
                    if info_hash.is_empty() || decoded.starts_with("urn:btih:") {
                        info_hash = hash.to_ascii_lowercase();
                    }
                }
            }
            // '+' means a space only in the display name; in URLs it is a literal '+'.
            "dn" => display_name = Some(percent_decode_str(&val.replace('+', " ")).decode_utf8_lossy().into_owned()),
            "tr" => {
                if !trackers.contains(&decoded) {
                    trackers.push(decoded);
                }
            }
            "ws" | "as" => {
                if let Ok(u) = Url::parse(&decoded) {
                    if matches!(u.scheme(), "http" | "https") && !seeds.iter().any(|(s, _)| *s == u) {
                        seeds.push((u, key == "ws"));
                    }
                }
            }
            _ => {}
        }
    }

    if info_hash.is_empty() {
        return Err("Magnet URI does not contain a valid info_hash (xt=urn:btih:...)".to_string());
    }

    let name = display_name.as_deref().filter(|n| is_safe_component(n));
    let mut web_seeds = Vec::new();
    for (seed, is_web_seed) in seeds {
        let url = if is_web_seed && seed.path().ends_with('/') {
            match name.and_then(|n| web_seed_file_url(&seed, &[n])) {
                Some(u) => u,
                None => {
                    tracing::warn!("Ignoring directory web seed {} (no usable dn= file name)", seed);
                    continue;
                }
            }
        } else {
            seed
        };
        if !web_seeds.contains(&url) {
            web_seeds.push(url);
        }
    }

    Ok(MagnetInfo {
        info_hash,
        display_name,
        trackers,
        web_seeds,
    })
}

fn non_negative(value: i64, what: &str) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("Invalid torrent: negative {}", what))
}

/// Parses a .torrent file bytes using zero-dependency bencode parsing.
pub fn parse_torrent_bytes(data: &[u8]) -> Result<TorrentInfo, String> {
    let (bval, _) = decode_bencode(data, 0, 0)?;
    let root = match bval {
        BValue::Dict(d) => d,
        _ => return Err("Invalid torrent: root must be a bencode dictionary".to_string()),
    };

    let mut trackers = Vec::new();
    let mut web_seeds = Vec::new();

    // Announce
    if let Some(BValue::Bytes(ann)) = dict_get(&root, b"announce") {
        if let Ok(s) = std::str::from_utf8(ann) {
            trackers.push(s.to_string());
        }
    }

    // Announce-list
    if let Some(BValue::List(tiers)) = dict_get(&root, b"announce-list") {
        for tier in tiers {
            if let BValue::List(tr_list) = tier {
                for tr in tr_list {
                    if let BValue::Bytes(b) = tr {
                        if let Ok(s) = std::str::from_utf8(b) {
                            if !trackers.contains(&s.to_string()) {
                                trackers.push(s.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Web seeds: url-list (BEP 19), a single string or a list of strings
    let seed_values: Vec<&BValue> = match dict_get(&root, b"url-list") {
        Some(BValue::List(list)) => list.iter().collect(),
        Some(single) => vec![single],
        None => Vec::new(),
    };
    for value in seed_values {
        if let BValue::Bytes(b) = value {
            if let Some(u) = std::str::from_utf8(b).ok().and_then(|s| Url::parse(s).ok()) {
                if matches!(u.scheme(), "http" | "https") && !web_seeds.contains(&u) {
                    web_seeds.push(u);
                }
            }
        }
    }

    // Info dict
    let info_dict = match dict_get(&root, b"info") {
        Some(BValue::Dict(d)) => d,
        _ => return Err("Invalid torrent: missing 'info' dictionary".to_string()),
    };

    let name = match dict_get(info_dict, b"name") {
        Some(BValue::Bytes(b)) => String::from_utf8_lossy(b).to_string(),
        _ => "torrent_download".to_string(),
    };
    if !is_safe_component(&name) {
        return Err(format!("Invalid torrent: unsafe name '{}'", name));
    }

    let piece_length = match dict_get(info_dict, b"piece length") {
        Some(BValue::Int(i)) => non_negative(*i, "piece length")?,
        _ => 0,
    };

    let num_pieces = match dict_get(info_dict, b"pieces") {
        Some(BValue::Bytes(b)) => b.len() / 20,
        _ => 0,
    };

    // Single-file torrents have `length`; multi-file ones a `files` list under the `name` directory.
    let mut files = Vec::new();
    if let Some(BValue::Int(len)) = dict_get(info_dict, b"length") {
        let urls = web_seeds
            .iter()
            .filter_map(|seed| {
                if seed.path().ends_with('/') { web_seed_file_url(seed, &[&name]) } else { Some(seed.clone()) }
            })
            .collect();
        files.push(TorrentFile { path: vec![name.clone()], length: non_negative(*len, "length")?, urls });
    } else if let Some(BValue::List(entries)) = dict_get(info_dict, b"files") {
        for entry in entries {
            let BValue::Dict(fd) = entry else {
                return Err("Invalid torrent: file entry is not a dictionary".to_string());
            };
            let length = match dict_get(fd, b"length") {
                Some(BValue::Int(l)) => non_negative(*l, "file length")?,
                _ => return Err("Invalid torrent: file entry without length".to_string()),
            };
            let path = match dict_get(fd, b"path") {
                Some(BValue::List(parts)) => parts
                    .iter()
                    .map(|p| match p {
                        BValue::Bytes(b) => Ok(String::from_utf8_lossy(b).to_string()),
                        _ => Err("Invalid torrent: path component is not a string".to_string()),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err("Invalid torrent: file entry without path".to_string()),
            };
            if path.is_empty() || !path.iter().all(|c| is_safe_component(c)) {
                return Err(format!("Invalid torrent: unsafe file path {:?}", path));
            }
            let mut components = vec![name.as_str()];
            components.extend(path.iter().map(String::as_str));
            // A multi-file web seed is always a directory, even if the trailing '/' is missing.
            let urls = web_seeds.iter().filter_map(|seed| web_seed_file_url(seed, &components)).collect();
            files.push(TorrentFile { path, length, urls });
        }
    }

    let total_length = files
        .iter()
        .try_fold(0u64, |sum, f| sum.checked_add(f.length))
        .ok_or_else(|| "Invalid torrent: total length overflows".to_string())?;

    Ok(TorrentInfo {
        name,
        total_length,
        piece_length,
        num_pieces,
        web_seeds,
        trackers,
        files,
    })
}

#[derive(Debug, Clone)]
enum BValue {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BValue>),
    Dict(Vec<(Vec<u8>, BValue)>),
}

fn dict_get<'a>(dict: &'a [(Vec<u8>, BValue)], key: &[u8]) -> Option<&'a BValue> {
    dict.iter().find(|(k, _)| k.as_slice() == key).map(|(_, v)| v)
}

fn decode_bencode(data: &[u8], mut pos: usize, depth: usize) -> Result<(BValue, usize), String> {
    if depth > MAX_BENCODE_DEPTH {
        return Err(format!("Bencode nested deeper than {} levels", MAX_BENCODE_DEPTH));
    }
    let Some(&tag) = data.get(pos) else {
        return Err("Unexpected end of bencode data".to_string());
    };

    match tag {
        b'i' => {
            pos += 1;
            let end = data[pos..]
                .iter()
                .position(|&b| b == b'e')
                .ok_or_else(|| "Unterminated integer".to_string())?
                + pos;
            let num_str = std::str::from_utf8(&data[pos..end]).map_err(|e| e.to_string())?;
            let num = num_str.parse::<i64>().map_err(|e| e.to_string())?;
            Ok((BValue::Int(num), end + 1))
        }
        b'l' => {
            pos += 1;
            let mut list = Vec::new();
            while pos < data.len() && data[pos] != b'e' {
                let (val, next_pos) = decode_bencode(data, pos, depth + 1)?;
                list.push(val);
                pos = next_pos;
            }
            if pos >= data.len() {
                return Err("Unterminated list".to_string());
            }
            Ok((BValue::List(list), pos + 1))
        }
        b'd' => {
            pos += 1;
            let mut dict = Vec::new();
            while pos < data.len() && data[pos] != b'e' {
                let (key_val, next_pos) = decode_bencode(data, pos, depth + 1)?;
                let key = match key_val {
                    BValue::Bytes(k) => k,
                    _ => return Err("Dictionary key must be a byte string".to_string()),
                };
                let (val, val_next_pos) = decode_bencode(data, next_pos, depth + 1)?;
                dict.push((key, val));
                pos = val_next_pos;
            }
            if pos >= data.len() {
                return Err("Unterminated dictionary".to_string());
            }
            Ok((BValue::Dict(dict), pos + 1))
        }
        b'0'..=b'9' => {
            let colon = data[pos..]
                .iter()
                .position(|&b| b == b':')
                .ok_or_else(|| "Missing colon in string".to_string())?
                + pos;
            let len_str = std::str::from_utf8(&data[pos..colon]).map_err(|e| e.to_string())?;
            let len = len_str.parse::<usize>().map_err(|e| e.to_string())?;
            let start = colon + 1;
            let end = start
                .checked_add(len)
                .filter(|&end| end <= data.len())
                .ok_or_else(|| "String length exceeds data".to_string())?;
            Ok((BValue::Bytes(data[start..end].to_vec()), end))
        }
        other => Err(format!("Unexpected byte in bencode at offset {}: {}", pos, other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_magnet_uri() {
        let uri = "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335380dc1f74c26&dn=Ubuntu+24.04&tr=https%3A%2F%2Ftorrent.ubuntu.com%2Fannounce&ws=https%3A%2F%2Freleases.ubuntu.com%2F24.04%2Fubuntu-24.04-desktop-amd64.iso";
        let info = parse_magnet_uri(uri).unwrap();

        assert_eq!(info.info_hash, "c12fe1c06bba254a9dc9f519b335380dc1f74c26");
        assert_eq!(info.display_name, Some("Ubuntu 24.04".to_string()));
        assert_eq!(info.trackers.len(), 1);
        assert_eq!(info.trackers[0], "https://torrent.ubuntu.com/announce");
        assert_eq!(info.web_seeds.len(), 1);
        assert_eq!(info.web_seeds[0].as_str(), "https://releases.ubuntu.com/24.04/ubuntu-24.04-desktop-amd64.iso");
    }

    #[test]
    fn test_magnet_web_seeds_follow_bep19_and_keep_plus() {
        let uri = "magnet:?xt.1=urn:btih:ABCDEF&dn=my+file.iso&tr.1=udp%3A%2F%2Ft1&tr.2=udp%3A%2F%2Ft2\
                   &ws=http%3A%2F%2Fmirror%2Fpub%2F&ws.2=http://h/a+b.iso&as=http://h/dir/";
        let info = parse_magnet_uri(uri).unwrap();
        assert_eq!(info.info_hash, "abcdef");
        assert_eq!(info.trackers, vec!["udp://t1", "udp://t2"]);
        let seeds: Vec<&str> = info.web_seeds.iter().map(Url::as_str).collect();
        // The directory seed gets the name appended; '+' in a URL stays a '+'; `as=` is used as given.
        assert_eq!(seeds, vec!["http://mirror/pub/my%20file.iso", "http://h/a+b.iso", "http://h/dir/"]);

        // A directory seed without a safe name cannot be resolved and is dropped.
        let info = parse_magnet_uri("magnet:?xt=urn:btih:ab&dn=..&ws=http://mirror/pub/").unwrap();
        assert!(info.web_seeds.is_empty());
    }

    #[test]
    fn test_parse_torrent_bytes() {
        // Construct a small valid torrent bencode dictionary:
        // d8:announce28:https://example.com/announce8:url-list27:https://mirror.com/test.iso4:infod6:lengthi1048576e4:name8:test.iso12:piece lengthi262144e6:pieces20:12345678901234567890ee
        let torrent_data = b"d8:announce28:https://example.com/announce8:url-list27:https://mirror.com/test.iso4:infod6:lengthi1048576e4:name8:test.iso12:piece lengthi262144e6:pieces20:12345678901234567890ee";
        let info = parse_torrent_bytes(torrent_data).unwrap();

        assert_eq!(info.name, "test.iso");
        assert_eq!(info.total_length, 1048576);
        assert_eq!(info.piece_length, 262144);
        assert_eq!(info.num_pieces, 1);
        assert_eq!(info.web_seeds.len(), 1);
        assert_eq!(info.web_seeds[0].as_str(), "https://mirror.com/test.iso");
        assert_eq!(info.trackers.len(), 1);
        assert_eq!(info.files.len(), 1);
        assert_eq!(info.files[0].urls[0].as_str(), "https://mirror.com/test.iso");
    }

    #[test]
    fn test_torrent_directory_web_seeds() {
        let single = b"d8:url-listl19:https://mirror/pub/e4:infod6:lengthi5e4:name5:a.isoee";
        let info = parse_torrent_bytes(single).unwrap();
        assert_eq!(info.files[0].urls[0].as_str(), "https://mirror/pub/a.iso");

        let multi = b"d8:url-list18:https://mirror/pub4:infod5:filesld6:lengthi3e4:pathl3:sub5:x.bineed6:lengthi4e4:pathl5:y.bineee4:name3:diree";
        let info = parse_torrent_bytes(multi).unwrap();
        assert_eq!(info.total_length, 7);
        assert_eq!(info.files[0].path, vec!["sub", "x.bin"]);
        assert_eq!(info.files[0].urls[0].as_str(), "https://mirror/pub/dir/sub/x.bin");
        assert_eq!(info.files[1].urls[0].as_str(), "https://mirror/pub/dir/y.bin");
    }

    #[test]
    fn test_crafted_torrents_are_rejected_without_panicking() {
        // String length that overflows `start + len`.
        assert!(decode_bencode(b"18446744073709551615:x", 0, 0).is_err());
        // Deep nesting would otherwise overflow the stack.
        let deep = vec![b'l'; 1_000_000];
        assert!(decode_bencode(&deep, 0, 0).unwrap_err().contains("nested"));
        // Negative lengths.
        assert!(parse_torrent_bytes(b"d4:infod6:lengthi-1e4:name1:aee").unwrap_err().contains("negative"));
        assert!(parse_torrent_bytes(b"d4:infod6:lengthi1e4:name1:a12:piece lengthi-5eee").is_err());
        // Total length overflow across files.
        let overflow = b"d4:infod5:filesld6:lengthi9223372036854775807e4:pathl1:aeed6:lengthi9223372036854775807e4:pathl1:beed6:lengthi9223372036854775807e4:pathl1:ceee4:name1:dee";
        assert!(parse_torrent_bytes(overflow).unwrap_err().contains("overflows"));
        // Path traversal in names and file paths.
        assert!(parse_torrent_bytes(b"d4:infod6:lengthi1e4:name2:..ee").is_err());
        assert!(parse_torrent_bytes(b"d4:infod5:filesld6:lengthi1e4:pathl2:..6:.bashrceee4:name1:dee").is_err());
    }
}
