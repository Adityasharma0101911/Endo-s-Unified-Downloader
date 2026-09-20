use quick_xml::events::Event;
use quick_xml::reader::Reader;
use url::Url;

#[derive(Debug, Clone)]
pub struct MetalinkFile {
    pub name: String,
    pub size: Option<u64>,
    pub urls: Vec<Url>,
    pub hashes: Vec<(String, String)>, // (type, hex)
}

/// Parses RFC 5854 (.meta4) and Metalink 3.0 (.metalink) XML documents.
pub fn parse_metalink(xml_content: &str) -> Result<Vec<MetalinkFile>, String> {
    let mut reader = Reader::from_str(xml_content);
    reader.config_mut().trim_text(true);

    let mut files = Vec::new();
    let mut current_file: Option<MetalinkFile> = None;
    let mut current_tag = String::new();
    let mut current_hash_type = String::new();

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_tag = name.clone();

                if name.eq_ignore_ascii_case("file") {
                    let mut file_name = String::new();
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref().eq_ignore_ascii_case(b"name") {
                            file_name = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                    current_file = Some(MetalinkFile {
                        name: file_name,
                        size: None,
                        urls: Vec::new(),
                        hashes: Vec::new(),
                    });
                } else if name.eq_ignore_ascii_case("hash") {
                    current_hash_type = String::new();
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref().eq_ignore_ascii_case(b"type") {
                            current_hash_type = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                }
            }
            Ok(Event::Text(ref e)) => {
                let text = e.unescape().map_err(|err| err.to_string())?.into_owned();
                if let Some(ref mut file) = current_file {
                    match current_tag.to_ascii_lowercase().as_str() {
                        "size" => {
                            if let Ok(s) = text.parse::<u64>() {
                                file.size = Some(s);
                            }
                        }
                        "url" => {
                            if let Ok(u) = Url::parse(&text) {
                                if !file.urls.contains(&u) {
                                    file.urls.push(u);
                                }
                            }
                        }
                        "hash" => {
                            let clean_hash = text.trim().to_ascii_lowercase();
                            if !clean_hash.is_empty() {
                                let htype = if current_hash_type.is_empty() {
                                    if clean_hash.len() == 64 {
                                        "sha256".to_string()
                                    } else if clean_hash.len() == 32 {
                                        "md5".to_string()
                                    } else {
                                        "unknown".to_string()
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
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name.eq_ignore_ascii_case("file") {
                    if let Some(f) = current_file.take() {
                        files.push(f);
                    }
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML error at position {}: {:?}", reader.buffer_position(), e)),
            _ => {}
        }
        buf.clear();
    }

    if let Some(f) = current_file {
        files.push(f);
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
    <url priority="1">https://mirror1.example.com/example.tar.gz</url>
    <url priority="2">https://mirror2.example.com/example.tar.gz</url>
  </file>
</metalink>"#;

        let files = parse_metalink(xml).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.name, "example.tar.gz");
        assert_eq!(f.size, Some(10485760));
        assert_eq!(f.urls.len(), 2);
        assert_eq!(f.hashes.len(), 1);
        assert_eq!(f.hashes[0].0, "sha-256");
        assert_eq!(f.hashes[0].1, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }
}
