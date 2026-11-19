//! WARC 1.1 output (`--warc FILE.warc.gz`) for `--mirror`, `--save-page` and Wayback downloads,
//! which replay tools open: a `warcinfo` record a run, then each answer as a `response` record
//! (the HTTP status line and headers as received, and the payload's bytes as received) with the
//! `request` that asked for it, and a capture the Wayback Machine served as a `resource` record
//! dated at its capture time. Every record is a gzip member of its own, so a reader may start at
//! any record's offset, and carries its `WARC-Block-Digest` and `WARC-Payload-Digest` (SHA-1, in
//! base32). A CDXJ index of the responses and resources (`FILE.cdxj`: SURT, time, then url, mime,
//! status, digest, length, offset and filename) is kept sorted beside it.
//!
//! What the HTTP library hands over is not quite the wire: header names come in lower case, and a
//! chunked body without its chunk framing, so its `Transfer-Encoding` header is left out; a body
//! sent compressed stays compressed, as received. Redirects the library followed are not recorded,
//! only the answer they led to.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha1::{Digest, Sha1};
use url::Url;

/// A WARC file being written (see the module). Its index is written by [`Writer::finish`], or,
/// when the run stops before that, as it is dropped.
pub struct Writer {
    file: std::fs::File,
    path: PathBuf,
    /// The file's name, for the index.
    name: String,
    info: String,
    /// The index's lines: those of earlier runs, then this one's.
    index: Vec<String>,
    finished: bool,
}

/// What a `response` record keeps of an answer besides its body.
pub(crate) struct Head {
    status: u16,
    /// As `HTTP/1.1`.
    version: String,
    headers: Vec<(String, String)>,
}

/// The status, HTTP version and headers of `answer`, as received.
pub(crate) fn head_of(answer: &reqwest::Response) -> Head {
    let headers = answer.headers().iter().map(|(name, value)| (name.as_str().to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned())).collect();
    Head { status: answer.status().as_u16(), version: format!("{:?}", answer.version()), headers }
}

/// The CDXJ index of the WARC file `path`: `site.warc.gz` → `site.cdxj`.
pub fn index_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = name.strip_suffix(".gz").unwrap_or(&name);
    let stem = stem.strip_suffix(".warc").unwrap_or(stem);
    path.with_file_name(format!("{stem}.cdxj"))
}

/// Base32 (RFC 4648, no padding), as WARC digests are written.
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut out, mut buffer, mut bits) = (String::new(), 0u32, 0);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// `time` as WARC dates are written (`2026-11-19T10:00:00Z`) and as the index's 14 digits.
pub(crate) fn dates(time: SystemTime) -> (String, String) {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (year, month, day) = crate::engine::civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    (format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z"), format!("{year:04}{month:02}{day:02}{h:02}{m:02}{s:02}"))
}

/// The time of a Wayback Machine timestamp (`20100603123456`, its start at least: `2010`).
pub fn capture_time(timestamp: &str) -> Option<SystemTime> {
    let digits = timestamp.get(..timestamp.len().min(14))?;
    if digits.len() < 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let padded = format!("{digits:0<14}");
    let part = |a: usize, b: usize| padded[a..b].parse::<i64>().ok();
    let (month, day) = (part(4, 6)?.max(1), part(6, 8)?.max(1));
    let days = crate::engine::days_from_civil(part(0, 4)?, month, day);
    let secs = days * 86_400 + part(8, 10)? * 3600 + part(10, 12)? * 60 + part(12, 14)?;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).ok()?))
}

/// A record's id: a random (version 4) UUID.
fn record_id() -> String {
    let mut b = [0u8; 16];
    let hex = crate::helpers::random_hex(16);
    for (i, byte) in b.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0);
    }
    (b[6], b[8]) = ((b[6] & 0x0f) | 0x40, (b[8] & 0x3f) | 0x80);
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("<urn:uuid:{}-{}-{}-{}-{}>", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// The SURT form of `url` the index is sorted by: host labels reversed (`com,example`), `www.`
/// left out, the port when not the scheme's, `)`, then the path and the sorted query, in lower case.
pub(crate) fn surt(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let mut key = match url.host() {
        Some(url::Host::Domain(_)) => {
            let host = host.strip_prefix("www.").unwrap_or(&host);
            host.split('.').rev().collect::<Vec<_>>().join(",")
        }
        _ => host,
    };
    if let Some(port) = url.port() {
        key.push_str(&format!(":{port}"));
    }
    key.push(')');
    key.push_str(&url.path().to_ascii_lowercase());
    if let Some(query) = url.query().filter(|q| !q.is_empty()) {
        let mut args: Vec<String> = query.split('&').map(str::to_ascii_lowercase).collect();
        args.sort();
        key.push('?');
        key.push_str(&args.join("&"));
    }
    key
}

/// A record as WARC frames it: `WARC/1.1`, its named fields, `Content-Length`, a blank line, the
/// block, and two line ends (`head` the block's start, `payload` the rest).
fn frame_head(fields: &[(&str, String)], block_len: u64) -> String {
    let mut text = String::from("WARC/1.1\r\n");
    for (name, value) in fields {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("Content-Length: {block_len}\r\n\r\n"));
    text
}

/// A rough media type from a file's extension, for a capture saved without its headers.
fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "txt" => "text/plain",
        "xml" => "application/xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

impl Writer {
    /// Opens the WARC file `path` (an earlier run's goes on, its index kept) and writes this run's
    /// `warcinfo` record.
    pub fn open(path: &Path) -> Result<Self, String> {
        let error = |e: std::io::Error| format!("Cannot write {}: {e}", path.display());
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(error)?;
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(error)?;
        let index = std::fs::read_to_string(index_path(path)).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut writer = Self { file, path: path.to_path_buf(), name: name.clone(), info: String::new(), index, finished: false };
        let fields = format!(
            "software: Endo's Unified Downloader {}\r\nformat: WARC File Format 1.1\r\nconformsTo: http://iipc.github.io/warc-specifications/specifications/warc-format/warc-1.1/\r\n",
            env!("CARGO_PKG_VERSION")
        );
        let (id, _, _, _) = writer.record("warcinfo", None, SystemTime::now(), "application/warc-fields", &[("WARC-Filename", name)], b"", &mut std::io::Cursor::new(fields.as_bytes()))?;
        writer.info = id;
        Ok(writer)
    }

    /// Writes one record: its `head` (the HTTP status line and headers, or nothing) and `payload`
    /// as its block. Returns its id, offset and length in the file, and its payload's digest.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        kind: &str,
        target: Option<&Url>,
        date: SystemTime,
        content_type: &str,
        extra: &[(&str, String)],
        head: &[u8],
        payload: &mut (impl Read + Seek),
    ) -> Result<(String, u64, u64, String), String> {
        let error = |e: std::io::Error| format!("Cannot write {}: {e}", self.path.display());
        let (mut payload_hash, mut block_hash) = (Sha1::new(), Sha1::new());
        block_hash.update(head);
        payload.seek(SeekFrom::Start(0)).map_err(error)?;
        let (mut buf, mut payload_len) = (vec![0u8; 64 << 10], 0u64);
        loop {
            let n = payload.read(&mut buf).map_err(error)?;
            if n == 0 {
                break;
            }
            payload_hash.update(&buf[..n]);
            block_hash.update(&buf[..n]);
            payload_len += n as u64;
        }
        let (id, (date, _)) = (record_id(), dates(date));
        let digest = base32(&payload_hash.finalize());
        let mut fields = vec![("WARC-Type", kind.to_string()), ("WARC-Record-ID", id.clone()), ("WARC-Date", date)];
        if let Some(target) = target {
            fields.push(("WARC-Target-URI", target.to_string()));
        }
        if kind != "warcinfo" {
            fields.push(("WARC-Warcinfo-ID", self.info.clone()));
            fields.push(("WARC-Payload-Digest", format!("sha1:{digest}")));
        }
        fields.extend(extra.iter().cloned());
        fields.push(("WARC-Block-Digest", format!("sha1:{}", base32(&block_hash.finalize()))));
        fields.push(("Content-Type", content_type.to_string()));
        let offset = self.file.metadata().map_err(error)?.len();
        let mut gz = flate2::write::GzEncoder::new(&mut self.file, flate2::Compression::default());
        gz.write_all(frame_head(&fields, head.len() as u64 + payload_len).as_bytes()).map_err(error)?;
        gz.write_all(head).map_err(error)?;
        payload.seek(SeekFrom::Start(0)).map_err(error)?;
        std::io::copy(&mut payload.take(payload_len), &mut gz).map_err(error)?;
        gz.write_all(b"\r\n\r\n").map_err(error)?;
        gz.finish().map_err(error)?;
        let length = self.file.metadata().map_err(error)?.len() - offset;
        Ok((id, offset, length, digest))
    }

    /// Adds the index line of a record (see the module).
    fn index_line(&mut self, url: &Url, date: SystemTime, mime: &str, status: Option<u16>, digest: &str, offset: u64, length: u64) {
        let mut fields = serde_json::Map::new();
        fields.insert("url".into(), url.as_str().into());
        fields.insert("mime".into(), mime.split(';').next().unwrap_or_default().trim().into());
        if let Some(status) = status {
            fields.insert("status".into(), status.to_string().into());
        }
        fields.insert("digest".into(), digest.into());
        fields.insert("length".into(), length.to_string().into());
        fields.insert("offset".into(), offset.to_string().into());
        fields.insert("filename".into(), self.name.clone().into());
        self.index.push(format!("{} {} {}", surt(url), dates(date).1, serde_json::Value::Object(fields)));
    }

    /// Writes the answer to a GET of `url` (its `head`, and `payload` its body as received) as a
    /// `response` record, and the request as one beside it.
    pub(crate) fn response(&mut self, url: &Url, date: SystemTime, head: &Head, payload: &mut (impl Read + Seek)) -> Result<(), String> {
        let Head { status, version, headers } = head;
        let reason = reqwest::StatusCode::from_u16(*status).ok().and_then(|s| s.canonical_reason()).unwrap_or("");
        let mut text = format!("{version} {status} {reason}\r\n");
        for (name, value) in headers.iter().filter(|(name, _)| !name.eq_ignore_ascii_case("transfer-encoding")) {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        text.push_str("\r\n");
        let mime = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-type")).map_or("unk", |(_, v)| v.as_str()).to_string();
        let (id, offset, length, digest) = self.record("response", Some(url), date, "application/http;msgtype=response", &[], text.as_bytes(), payload)?;
        self.index_line(url, date, &mime, Some(*status), &digest, offset, length);
        let target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_string(),
        };
        let request = format!("GET {target} HTTP/1.1\r\nhost: {host}\r\nuser-agent: {}\r\n\r\n", crate::resolver::APP_USER_AGENT);
        let concurrent = [("WARC-Concurrent-To", id)];
        self.record("request", Some(url), date, "application/http;msgtype=request", &concurrent, request.as_bytes(), &mut std::io::Cursor::new(&b""[..]))?;
        Ok(())
    }

    /// Writes `payload`, what `url` held at `date` (a capture the Wayback Machine kept), as a
    /// `resource` record, unless the index has it already.
    pub fn resource(&mut self, url: &Url, date: SystemTime, mime: &str, payload: &mut (impl Read + Seek)) -> Result<(), String> {
        let key = format!("{} {} ", surt(url), dates(date).1);
        if self.index.iter().any(|line| line.starts_with(&key) && line.contains(&format!("\"url\":{}", serde_json::Value::from(url.as_str())))) {
            return Ok(());
        }
        let (_, offset, length, digest) = self.record("resource", Some(url), date, mime, &[], b"", payload)?;
        self.index_line(url, date, mime, None, &digest, offset, length);
        Ok(())
    }

    /// Writes the index, sorted, in one rename.
    fn write_index(&mut self) -> Result<(), String> {
        self.index.sort();
        self.index.dedup();
        let file = index_path(&self.path);
        let part = file.with_extension("cdxj.part");
        let text: String = self.index.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&part, text).and_then(|()| std::fs::rename(&part, &file)).map_err(|e| format!("Cannot write {}: {e}", file.display()))
    }

    /// Ends the run: writes the index.
    pub fn finish(mut self) -> Result<(), String> {
        self.finished = true;
        self.write_index()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.write_index();
        }
    }
}

/// Writes each capture of `lines` (the Wayback manifest's, see `wayback::manifest_line`) whose
/// file is in `dir` into the WARC file `warc` as a `resource` record dated at its capture time.
/// Blocking.
pub fn wayback_resources(dir: &Path, lines: &[String], warc: &Path) -> Result<usize, String> {
    let mut writer = Writer::open(warc)?;
    let mut written = 0;
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        let [timestamp, original, _, file] = fields[..] else { continue };
        let (Some(date), Ok(url)) = (capture_time(timestamp), Url::parse(original)) else { continue };
        let Ok(mut payload) = std::fs::File::open(dir.join(file)) else { continue };
        writer.resource(&url, date, mime_of(file), &mut payload)?;
        written += 1;
    }
    writer.finish()?;
    Ok(written)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One record read back: its fields and block.
    pub(crate) struct Record {
        pub fields: Vec<(String, String)>,
        pub block: Vec<u8>,
    }

    impl Record {
        pub fn field(&self, name: &str) -> Option<&str> {
            self.fields.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }
    }

    /// A minimal WARC reader: the records of the uncompressed `bytes`, each checked to be framed as
    /// WARC 1.1 says (version line, fields, blank line, Content-Length bytes of block, CRLF CRLF).
    pub(crate) fn records(mut bytes: &[u8]) -> Vec<Record> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            let end = bytes.windows(4).position(|w| w == b"\r\n\r\n").expect("a record's fields end in a blank line");
            let head = std::str::from_utf8(&bytes[..end]).unwrap();
            let mut lines = head.split("\r\n");
            assert_eq!(lines.next(), Some("WARC/1.1"));
            let fields: Vec<(String, String)> = lines.map(|l| l.split_once(": ").map(|(n, v)| (n.to_string(), v.to_string())).expect("name: value")).collect();
            let len: usize = fields.iter().find(|(n, _)| n == "Content-Length").unwrap().1.parse().unwrap();
            let block = bytes[end + 4..end + 4 + len].to_vec();
            assert_eq!(&bytes[end + 4 + len..end + 8 + len], b"\r\n\r\n", "a block ends in two line ends");
            bytes = &bytes[end + 8 + len..];
            out.push(Record { fields, block });
        }
        out
    }

    #[test]
    fn digests_dates_and_keys_are_written_as_warc_and_cdxj_want() {
        assert_eq!(base32(&Sha1::digest(b"")), "3I42H3S6NNFQ2MSVX7XZKYAYSCX5QBYJ");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        let time = UNIX_EPOCH + Duration::from_secs(1_158_686_414);
        assert_eq!(dates(time), ("2006-09-19T17:20:14Z".to_string(), "20060919172014".to_string()));
        assert_eq!(capture_time("20060919172014"), Some(time));
        assert_eq!(capture_time("2006"), capture_time("20060101000000"));
        assert!(capture_time("20x6").is_none());
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(surt(&url("http://www.Example.com/A/b.html?z=1&a=2")), "com,example)/a/b.html?a=2&z=1");
        assert_eq!(surt(&url("https://example.com:8443/")), "com,example:8443)/");
        assert_eq!(surt(&url("http://10.0.0.1/x")), "10.0.0.1)/x");
        assert_eq!(index_path(Path::new("out/site.warc.gz")), Path::new("out/site.cdxj"));
        let id = record_id();
        assert!(id.starts_with("<urn:uuid:") && id.len() == 47 && id.as_bytes()[24] == b'4', "{id}");
    }

    /// The warcinfo record of the WARC 1.1 specification's example, framed byte for byte as there.
    #[test]
    fn records_are_framed_as_the_specifications_example() {
        let block = "software: Heritrix 1.12.0 http://crawler.archive.org\r\nhostname: crawling017.archive.org\r\nip: 207.241.227.234\r\n\
                     isPartOf: testcrawl-20050708\r\ndescription: testcrawl with WARC output\r\noperator: IA_Admin\r\n\
                     http-header-user-agent: Mozilla/5.0 (compatible; heritrix/1.4.0 +http://crawler.archive.org)\r\n\
                     format: WARC file version 1.1\r\nconformsTo: http://www.archive.org/documents/WarcFileFormat-1.1.html\r\n";
        let fields = [
            ("WARC-Type", "warcinfo".to_string()),
            ("WARC-Date", "2006-09-19T17:20:14Z".to_string()),
            ("WARC-Record-ID", "<urn:uuid:d7ae5c10-e6b3-4d27-967d-34780c58ba39>".to_string()),
            ("Content-Type", "application/warc-fields".to_string()),
        ];
        let framed = format!("{}{block}\r\n\r\n", frame_head(&fields, block.len() as u64));
        let expected = format!(
            "WARC/1.1\r\nWARC-Type: warcinfo\r\nWARC-Date: 2006-09-19T17:20:14Z\r\nWARC-Record-ID: <urn:uuid:d7ae5c10-e6b3-4d27-967d-34780c58ba39>\r\n\
             Content-Type: application/warc-fields\r\nContent-Length: {}\r\n\r\n{block}\r\n\r\n",
            block.len()
        );
        assert_eq!(framed, expected);
        assert_eq!(records(framed.as_bytes())[0].block, block.as_bytes());
    }

    /// A run's records read back whole, each its own gzip member at the offset its index line gives,
    /// with digests that match; a second run goes on in the same file and keeps the first's index.
    #[test]
    fn a_warc_reads_back_record_by_record_from_its_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("site.warc.gz");
        let page = b"<html>hello</html>".to_vec();
        let url = Url::parse("http://example.com/a.html").unwrap();
        let headers = vec![("content-type".to_string(), "text/html".to_string()), ("transfer-encoding".to_string(), "chunked".to_string())];
        let head = Head { status: 200, version: "HTTP/1.1".to_string(), headers };
        let mut warc = Writer::open(&path).unwrap();
        warc.response(&url, UNIX_EPOCH + Duration::from_secs(1_700_000_000), &head, &mut std::io::Cursor::new(&page[..])).unwrap();
        warc.finish().unwrap();
        let old = Url::parse("http://example.com/old.css").unwrap();
        let mut warc = Writer::open(&path).unwrap();
        warc.resource(&old, capture_time("2010").unwrap(), "text/css", &mut std::io::Cursor::new(&b"p{}"[..])).unwrap();
        warc.resource(&old, capture_time("2010").unwrap(), "text/css", &mut std::io::Cursor::new(&b"p{}"[..])).unwrap();
        drop(warc);

        let bytes = std::fs::read(&path).unwrap();
        let mut all = Vec::new();
        flate2::read::MultiGzDecoder::new(&bytes[..]).read_to_end(&mut all).unwrap();
        let kinds: Vec<String> = records(&all).iter().map(|r| r.field("WARC-Type").unwrap().to_string()).collect();
        assert_eq!(kinds, ["warcinfo", "response", "request", "warcinfo", "resource"], "the same capture once");

        let index = std::fs::read_to_string(index_path(&path)).unwrap();
        let lines: Vec<&str> = index.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("com,example)/a.html 20231114221320 {") && lines[1].starts_with("com,example)/old.css 20100101000000 {"), "{index}");
        for line in &lines {
            let json: serde_json::Value = serde_json::from_str(line.splitn(3, ' ').nth(2).unwrap()).unwrap();
            let (offset, length): (usize, usize) = (json["offset"].as_str().unwrap().parse().unwrap(), json["length"].as_str().unwrap().parse().unwrap());
            let mut one = Vec::new();
            flate2::read::GzDecoder::new(&bytes[offset..offset + length]).read_to_end(&mut one).unwrap();
            let record = records(&one).remove(0);
            assert_eq!(record.field("WARC-Target-URI"), json["url"].as_str());
            assert_eq!(record.field("WARC-Block-Digest").unwrap(), format!("sha1:{}", base32(&Sha1::digest(&record.block))));
            assert_eq!(json["filename"], "site.warc.gz");
            if record.field("WARC-Type") == Some("response") {
                let text = String::from_utf8(record.block.clone()).unwrap();
                assert!(text.starts_with("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n\r\n") && text.ends_with("<html>hello</html>"), "{text}");
                assert_eq!(record.field("WARC-Payload-Digest").unwrap(), format!("sha1:{}", base32(&Sha1::digest(&page))));
                assert_eq!((json["status"].as_str(), json["mime"].as_str()), (Some("200"), Some("text/html")));
                assert_eq!(json["digest"].as_str().unwrap(), base32(&Sha1::digest(&page)));
            } else {
                assert_eq!(record.block, b"p{}");
            }
        }
    }
}
