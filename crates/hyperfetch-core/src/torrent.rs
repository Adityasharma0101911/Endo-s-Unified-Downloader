use url::Url;

#[derive(Debug, Clone)]
pub struct MagnetInfo {
    pub info_hash: String,
    pub display_name: Option<String>,
    pub trackers: Vec<String>,
    pub web_seeds: Vec<Url>,
}

#[derive(Debug, Clone)]
pub struct TorrentInfo {
    pub name: String,
    pub total_length: u64,
    pub piece_length: u64,
    pub num_pieces: usize,
    pub web_seeds: Vec<Url>,
    pub trackers: Vec<String>,
}

pub fn is_magnet_uri(s: &str) -> bool {
    s.trim().starts_with("magnet:?")
}

/// Parses a Magnet URI (RFC draft / BEP 9)
pub fn parse_magnet_uri(uri: &str) -> Result<MagnetInfo, String> {
    let trimmed = uri.trim();
    if !trimmed.starts_with("magnet:?") {
        return Err("Not a valid magnet URI (must start with magnet:?)".to_string());
    }

    let query = &trimmed["magnet:?".len()..];
    let mut info_hash = String::new();
    let mut display_name = None;
    let mut trackers = Vec::new();
    let mut web_seeds = Vec::new();

    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let val = parts.next().unwrap_or("");
        let decoded_val = percent_decode(val);

        match key {
            "xt" => {
                // E.g. urn:btih:<hash>
                if let Some(stripped) = decoded_val.strip_prefix("urn:btih:") {
                    info_hash = stripped.to_ascii_lowercase();
                }
            }
            "dn" => {
                display_name = Some(decoded_val);
            }
            "tr" => {
                if !trackers.contains(&decoded_val) {
                    trackers.push(decoded_val);
                }
            }
            "ws" | "as" => {
                if let Ok(u) = Url::parse(&decoded_val) {
                    if !web_seeds.contains(&u) {
                        web_seeds.push(u);
                    }
                }
            }
            _ => {}
        }
    }

    if info_hash.is_empty() {
        return Err("Magnet URI does not contain a valid info_hash (xt=urn:btih:...)".to_string());
    }

    Ok(MagnetInfo {
        info_hash,
        display_name,
        trackers,
        web_seeds,
    })
}

/// Parses a .torrent file bytes using zero-dependency bencode parsing.
pub fn parse_torrent_bytes(data: &[u8]) -> Result<TorrentInfo, String> {
    let (bval, _) = decode_bencode(data, 0)?;
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

    // Web seeds: url-list (BEP 19)
    if let Some(b_url) = dict_get(&root, b"url-list") {
        match b_url {
            BValue::Bytes(b) => {
                if let Ok(s) = std::str::from_utf8(b) {
                    if let Ok(u) = Url::parse(s) {
                        web_seeds.push(u);
                    }
                }
            }
            BValue::List(list) => {
                for item in list {
                    if let BValue::Bytes(b) = item {
                        if let Ok(s) = std::str::from_utf8(b) {
                            if let Ok(u) = Url::parse(s) {
                                if !web_seeds.contains(&u) {
                                    web_seeds.push(u);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
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

    let piece_length = match dict_get(info_dict, b"piece length") {
        Some(BValue::Int(i)) => *i as u64,
        _ => 0,
    };

    let num_pieces = match dict_get(info_dict, b"pieces") {
        Some(BValue::Bytes(b)) => b.len() / 20,
        _ => 0,
    };

    // Calculate total length (single file vs multi-file)
    let total_length = if let Some(BValue::Int(len)) = dict_get(info_dict, b"length") {
        *len as u64
    } else if let Some(BValue::List(files)) = dict_get(info_dict, b"files") {
        let mut sum = 0u64;
        for file in files {
            if let BValue::Dict(fd) = file {
                if let Some(BValue::Int(flen)) = dict_get(fd, b"length") {
                    sum += *flen as u64;
                }
            }
        }
        sum
    } else {
        0
    };

    Ok(TorrentInfo {
        name,
        total_length,
        piece_length,
        num_pieces,
        web_seeds,
        trackers,
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

fn decode_bencode(data: &[u8], mut pos: usize) -> Result<(BValue, usize), String> {
    if pos >= data.len() {
        return Err("Unexpected end of bencode data".to_string());
    }

    match data[pos] {
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
                let (val, next_pos) = decode_bencode(data, pos)?;
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
                let (key_val, next_pos) = decode_bencode(data, pos)?;
                let key = match key_val {
                    BValue::Bytes(k) => k,
                    _ => return Err("Dictionary key must be a byte string".to_string()),
                };
                let (val, val_next_pos) = decode_bencode(data, next_pos)?;
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
            let end = start + len;
            if end > data.len() {
                return Err("String length exceeds data".to_string());
            }
            let bytes = data[start..end].to_vec();
            Ok((BValue::Bytes(bytes), end))
        }
        _ => Err(format!("Unexpected byte in bencode at offset {}: {}", pos, data[pos])),
    }
}

fn percent_decode(input: &str) -> String {
    let replaced = input.replace('+', " ");
    let mut bytes = Vec::with_capacity(replaced.len());
    let mut chars = replaced.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(c1), Some(c2)) = (h1, h2) {
                let hex_str = [c1, c2];
                if let Ok(s) = std::str::from_utf8(&hex_str) {
                    if let Ok(val) = u8::from_str_radix(s, 16) {
                        bytes.push(val);
                        continue;
                    }
                }
                bytes.push(b'%');
                bytes.push(c1);
                bytes.push(c2);
            } else {
                bytes.push(b'%');
                if let Some(c1) = h1 { bytes.push(c1); }
            }
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
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
    }
}
