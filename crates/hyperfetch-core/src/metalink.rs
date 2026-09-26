use quick_xml::events::Event;
use quick_xml::reader::Reader;
use url::Url;

#[derive(Debug, Clone)]
pub struct MetalinkFile {
    /// Plain file name, valid on every OS; directory parts of the metalink name are dropped.
    pub name: String,
    pub size: Option<u64>,
    /// HTTP(S) URLs, best priority first.
    pub urls: Vec<Url>,
    /// Whole-file hashes as (type, lowercase hex). Types are normalized without dashes
    /// ("sha-256" -> "sha256"). Piece hashes are not included.
    pub hashes: Vec<(String, String)>,
}

/// Returns the last path component of a metalink file name, made valid on every OS. Rejects
/// names that are absolute (including `C:\...`) or climb out of the download directory
/// (RFC 5854 section 4.1.2.1).
fn safe_file_name(name: &str) -> Result<String, String> {
    let unsafe_name = || format!("Unsafe file name in Metalink: {:?}", name);
    let drive = matches!(name.as_bytes(), [letter, b':', b'/' | b'\\', ..] if letter.is_ascii_alphabetic());
    if name.starts_with(['/', '\\']) || drive {
        return Err(unsafe_name());
    }
    let parts: Vec<&str> = name.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").collect();
    if parts.contains(&"..") {
        return Err(unsafe_name());
    }
    parts
        .last()
        .map(|p| crate::engine::sanitize_component(p))
        .filter(|p| !p.is_empty())
        .ok_or_else(unsafe_name)
}

/// Parses RFC 5854 (.meta4) and Metalink 3.0 (.metalink) XML documents.
pub fn parse_metalink(xml_content: &str) -> Result<Vec<MetalinkFile>, String> {
    let mut reader = Reader::from_str(xml_content);
    reader.config_mut().trim_text(true);

    let mut files = Vec::new();
    let mut current_file: Option<MetalinkFile> = None;
    // (rank, url) for the current file; lower rank = preferred.
    let mut ranked_urls: Vec<(u32, Url)> = Vec::new();
    let mut current_tag = String::new();
    let mut current_hash_type = String::new();
    let mut current_url_rank = u32::MAX;
    let mut current_url_usable = true;
    let mut in_pieces = false;

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                // Match on the local name so namespace prefixes (`<ml:file>`) do not matter.
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase();
                let attr = |wanted: &[u8]| {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref().eq_ignore_ascii_case(wanted))
                        .map(|a| match a.unescape_value() {
                            Ok(value) => value.into_owned(),
                            Err(_) => String::from_utf8_lossy(&a.value).into_owned(),
                        })
                };

                match name.as_str() {
                    "file" => {
                        let file_name = attr(b"name").unwrap_or_default();
                        ranked_urls.clear();
                        current_file = Some(MetalinkFile {
                            name: safe_file_name(&file_name)?,
                            size: None,
                            urls: Vec::new(),
                            hashes: Vec::new(),
                        });
                    }
                    "pieces" => in_pieces = true,
                    "hash" => current_hash_type = attr(b"type").unwrap_or_default().to_ascii_lowercase().replace('-', ""),
                    "url" => {
                        // Metalink 3 also lists e.g. `type="bittorrent"` links to .torrent files.
                        current_url_usable = attr(b"type")
                            .is_none_or(|t| t.eq_ignore_ascii_case("http") || t.eq_ignore_ascii_case("https"));
                        // RFC 5854: priority 1 is best. Metalink 3: preference 100 is best.
                        current_url_rank = match (attr(b"priority"), attr(b"preference")) {
                            (Some(p), _) => p.trim().parse().unwrap_or(u32::MAX),
                            (None, Some(p)) => p.trim().parse::<u32>().map_or(u32::MAX, |p| 101u32.saturating_sub(p)),
                            (None, None) => u32::MAX,
                        };
                    }
                    _ => {}
                }
                current_tag = name;
            }
            Ok(Event::Text(ref e)) => {
                let text = e.unescape().map_err(|err| err.to_string())?.into_owned();
                if let Some(ref mut file) = current_file {
                    match current_tag.as_str() {
                        "size" => {
                            if let Ok(s) = text.parse::<u64>() {
                                file.size = Some(s);
                            }
                        }
                        "url" if current_url_usable => {
                            if let Ok(u) = Url::parse(&text) {
                                if matches!(u.scheme(), "http" | "https") && !ranked_urls.iter().any(|(_, x)| *x == u) {
                                    ranked_urls.push((current_url_rank, u));
                                }
                            }
                        }
                        "hash" if !in_pieces => {
                            let clean_hash = text.trim().to_ascii_lowercase();
                            if !clean_hash.is_empty() {
                                let htype = if current_hash_type.is_empty() {
                                    match clean_hash.len() {
                                        64 => "sha256".to_string(),
                                        32 => "md5".to_string(),
                                        _ => "unknown".to_string(),
                                    }
                                } else {
                                    current_hash_type.clone()
                                };
                                file.hashes.push((htype, clean_hash));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref().to_ascii_lowercase().as_slice() {
                    b"file" => {
                        if let Some(mut f) = current_file.take() {
                            ranked_urls.sort_by_key(|(rank, _)| *rank);
                            f.urls = ranked_urls.drain(..).map(|(_, u)| u).collect();
                            files.push(f);
                        }
                    }
                    b"pieces" => in_pieces = false,
                    _ => {}
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML error at position {}: {:?}", reader.buffer_position(), e)),
            _ => {}
        }
        buf.clear();
    }

    if current_file.is_some() {
        return Err("Truncated Metalink XML: unterminated <file> element".to_string());
    }

    if files.is_empty() {
        return Err("No file entries found in Metalink XML".to_string());
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rfc5854_metalink() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="example.tar.gz">
    <size>10485760</size>
    <hash type="sha-256">2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824</hash>
    <url priority="2">https://mirror2.example.com/example.tar.gz</url>
    <url priority="1">https://mirror1.example.com/example.tar.gz</url>
    <url>ftp://mirror3.example.com/example.tar.gz</url>
  </file>
</metalink>"#;

        let files = parse_metalink(xml).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.name, "example.tar.gz");
        assert_eq!(f.size, Some(10485760));
        assert_eq!(f.urls.len(), 2, "only HTTP(S) URLs are usable");
        assert_eq!(f.urls[0].host_str(), Some("mirror1.example.com"), "sorted by priority");
        assert_eq!(f.hashes.len(), 1);
        assert_eq!(f.hashes[0].0, "sha256");
        assert_eq!(f.hashes[0].1, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }

    #[test]
    fn test_piece_hashes_are_not_file_hashes() {
        let xml = r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="a.bin">
            <hash type="sha-256">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</hash>
            <pieces length="262144" type="sha-256">
              <hash>bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</hash>
              <hash>cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc</hash>
            </pieces>
            <url>https://m/a.bin</url></file></metalink>"#;
        let files = parse_metalink(xml).unwrap();
        assert_eq!(files[0].hashes, vec![("sha256".to_string(), "a".repeat(64))]);
    }

    #[test]
    fn test_namespace_prefixed_metalink() {
        let xml = r#"<ml:metalink xmlns:ml="urn:ietf:params:xml:ns:metalink">
            <ml:file ml:name="p.iso"><ml:size>42</ml:size><ml:url>https://m/p.iso</ml:url></ml:file>
            </ml:metalink>"#;
        let files = parse_metalink(xml).unwrap();
        assert_eq!(files[0].name, "p.iso");
        assert_eq!(files[0].size, Some(42));
        assert_eq!(files[0].urls.len(), 1);
    }

    #[test]
    fn test_metalink3_preference_and_safe_names() {
        let xml = r#"<metalink version="3.0"><files><file name="sub/dir/x.iso"><resources>
            <url type="http" preference="10">http://slow/x.iso</url>
            <url type="http" preference="100">http://fast/x.iso</url>
            <url type="bittorrent" preference="100">http://fast/x.iso.torrent</url>
            </resources></file></files></metalink>"#;
        let files = parse_metalink(xml).unwrap();
        assert_eq!(files[0].name, "x.iso");
        assert_eq!(files[0].urls[0].host_str(), Some("fast"));
        assert_eq!(files[0].urls.len(), 2, "torrent links are not mirrors");

        assert!(parse_metalink(r#"<metalink><file name="t.iso"><url>http://m/t.iso</url>"#).is_err());

        for bad in ["../../.bashrc", "/etc/passwd", "C:\\Windows\\x.dll", "c:/x.dll", "a\\..\\..\\b", "", "sub/..."] {
            let xml = format!(r#"<metalink><file name="{}"><url>http://m/x</url></file></metalink>"#, bad);
            assert!(parse_metalink(&xml).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn test_names_invalid_on_windows_are_cleaned_not_rejected() {
        for (name, expected) in [
            ("Ep 1: Pilot.mkv", "Ep 1_ Pilot.mkv"),
            ("A: Tale.mkv", "A_ Tale.mkv"),
            ("show/S01: &quot;Pilot&quot;?.mkv", "S01_ _Pilot__.mkv"),
            ("x&#x85;y&#9;.bin", "x_y_.bin"),
            ("Tom &amp; Jerry.mkv", "Tom & Jerry.mkv"),
            ("con.txt", "_con.txt"),
            // Dotfiles keep their names; only the trailing dots and spaces Windows drops go.
            (".htaccess", ".htaccess"),
            (".config/settings.json", "settings.json"),
            ("site/.gitignore. ", ".gitignore"),
        ] {
            let xml = format!(r#"<metalink><file name="{}"><url>http://m/x</url></file></metalink>"#, name);
            assert_eq!(parse_metalink(&xml).unwrap()[0].name, expected, "{name:?}");
        }
    }
}
