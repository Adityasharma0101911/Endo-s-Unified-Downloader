//! Requests to S3 and S3-compatible servers signed with an access key (AWS Signature Version 4):
//! each one as it is sent (see `worker::authorize`), with an unsigned payload, so a range request
//! or a retry gets a signature of its own. Only hosts that serve S3 are signed for (see
//! [`S3Key::applies`]): Amazon S3, Wasabi, Backblaze B2, DigitalOcean Spaces, Cloudflare R2, and
//! the endpoint the user names for another server (MinIO, say). A clock off by minutes is set
//! right by the server's `Date` (see [`learn_skew`]).

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, DATE};
use reqwest::RequestBuilder;
use ring::hmac;
use sha2::{Digest, Sha256};
use url::Url;

/// The payload hash of a request whose body is not signed (S3 takes it for any request).
const UNSIGNED: &str = "UNSIGNED-PAYLOAD";
/// A server clock further off than this from the one signing goes by is taken as the right one.
const SKEW: i64 = 5 * 60;

/// Seconds the servers' clock is ahead of this computer's, as a signed request's refusal showed.
static AHEAD: AtomicI64 = AtomicI64::new(0);

/// An access key for S3 and S3-compatible servers, with the region and the endpoint of a server
/// on another host. Never saved with a download, and its Debug shows no value.
#[derive(Clone, Default, PartialEq)]
pub struct S3Key {
    pub access_key: String,
    pub secret: String,
    /// The session token of temporary credentials.
    pub session_token: Option<String>,
    /// The region signed for; else the one the host names (`s3.eu-west-1.amazonaws.com`), else
    /// `us-east-1` (`auto` on R2).
    pub region: Option<String>,
    /// An S3-compatible server on a host of its own (`http://minio.lan:9000`): its links are
    /// signed for too, and a folder link on it (`/{bucket}/{folder}/`) lists the bucket.
    pub endpoint: Option<Url>,
}

impl fmt::Debug for S3Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Key").field("region", &self.region).field("endpoint", &self.endpoint.as_ref().map(Url::as_str)).finish_non_exhaustive()
    }
}

impl S3Key {
    /// Whether requests to `url` are signed with this key: a host that serves S3 (its own
    /// address for a bucket), or the endpoint's host and port.
    pub fn applies(&self, url: &Url) -> bool {
        let Some(host) = url.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase()) else { return false };
        if let Some(endpoint) = &self.endpoint {
            if endpoint.host_str().is_some_and(|h| h.eq_ignore_ascii_case(&host)) && endpoint.port_or_known_default() == url.port_or_known_default() {
                return true;
            }
        }
        let s3_label = |l: &&str| *l == "s3" || (l.starts_with("s3-") && !l.starts_with("s3-website"));
        let labels: Vec<&str> = host.split('.').collect();
        ["amazonaws.com", "wasabisys.com", "backblazeb2.com"].iter().any(|d| host.ends_with(&format!(".{d}")) && labels.iter().any(s3_label))
            || host.ends_with(".digitaloceanspaces.com")
            || host.ends_with(".r2.cloudflarestorage.com")
    }

    /// The region requests to `url` are signed for (see [`S3Key::region`]).
    fn region_for(&self, url: &Url) -> String {
        if let Some(region) = self.region.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
            return region.to_string();
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        if host.ends_with(".r2.cloudflarestorage.com") {
            return "auto".into();
        }
        let labels: Vec<&str> = host.split('.').collect();
        if let Some(spaces) = host.strip_suffix(".digitaloceanspaces.com") {
            return spaces.rsplit('.').next().unwrap_or("us-east-1").to_string();
        }
        // s3.{region}.host, s3-{region}.host, {bucket}.s3.dualstack.{region}.host.
        let at = labels.iter().position(|l| *l == "s3" || l.starts_with("s3-"));
        let region = at.and_then(|at| match labels[at].strip_prefix("s3-") {
            Some(region) => Some(region),
            None => labels[at + 1..].iter().copied().find(|l| *l != "dualstack"),
        });
        match region {
            Some(region) if !["amazonaws", "wasabisys", "backblazeb2", "external-1"].contains(&region) => region.to_string(),
            _ => "us-east-1".into(),
        }
    }
}

/// `request` signed with `key` (an `Authorization` header and the `x-amz-` ones it signs) for
/// the time the servers keep; as it was when it cannot be read (a body that streams).
pub(crate) fn signed(request: RequestBuilder, key: &S3Key) -> RequestBuilder {
    let Some(Ok(built)) = request.try_clone().map(RequestBuilder::build) else { return request };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64) + AHEAD.load(Ordering::Relaxed);
    let date = amz_date(now.max(0) as u64);
    let mut headers = vec![("x-amz-content-sha256", UNSIGNED), ("x-amz-date", date.as_str())];
    if let Some(token) = key.session_token.as_deref().filter(|t| !t.is_empty()) {
        headers.push(("x-amz-security-token", token));
    }
    let region = key.region_for(built.url());
    let value = authorization(&key.access_key, &key.secret, &region, "s3", built.method().as_str(), built.url(), &headers, UNSIGNED);
    let mut sent = HeaderMap::new();
    for (name, value) in &headers {
        if let Ok(mut value) = HeaderValue::from_str(value) {
            value.set_sensitive(name.ends_with("token"));
            sent.insert(*name, value);
        }
    }
    if let Ok(mut value) = HeaderValue::from_str(&value) {
        value.set_sensitive(true);
        sent.insert(AUTHORIZATION, value);
    }
    request.headers(sent)
}

/// Takes the servers' clock from `headers` (a signed request's refusal's `Date`) when it is
/// minutes off the one signing goes by; true when it was, for the request to be signed again.
pub(crate) fn learn_skew(headers: &HeaderMap) -> bool {
    let Some(server) = headers.get(DATE).and_then(|d| d.to_str().ok()).and_then(crate::engine::parse_http_date) else { return false };
    let local = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let ahead = server as i64 - local;
    if (ahead - AHEAD.load(Ordering::Relaxed)).abs() <= SKEW {
        return false;
    }
    tracing::warn!("This computer's clock is {} s off the server's: signing by the server's", ahead);
    AHEAD.store(ahead, Ordering::Relaxed);
    true
}

/// `secs` as SigV4 writes a time: `20150830T123600Z`.
fn amz_date(secs: u64) -> String {
    let (year, month, day) = crate::engine::civil_from_days((secs / 86_400) as i64);
    let time = secs % 86_400;
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z", time / 3600, time / 60 % 60, time % 60)
}

/// `bytes` with every one but A-Z, a-z, 0-9 and `-._~` as `%XX`, as SigV4 encodes a URI's parts.
fn encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The `Authorization` value for a `method` request to `url` (its host signed with `headers`,
/// lowercase names whose values are sent as they are) whose payload hashes to `payload`: an
/// S3 one's path is not normalized, each segment encoded once.
#[allow(clippy::too_many_arguments)]
fn authorization(access_key: &str, secret: &str, region: &str, service: &str, method: &str, url: &Url, headers: &[(&str, &str)], payload: &str) -> String {
    let decode = |s: &str| percent_encoding::percent_decode_str(s).collect::<Vec<u8>>();
    let path: Vec<String> = url.path().split('/').map(|segment| encode(&decode(segment))).collect();
    let mut query: Vec<(String, String)> = url
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            // A query's `+` is a space, which SigV4 encodes as %20.
            let plain = |s: &str| encode(&decode(&s.replace('+', "%20")));
            (plain(k), plain(v))
        })
        .collect();
    query.sort();
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    let mut signed: Vec<(&str, &str)> = vec![("host", &host)];
    signed.extend(headers.iter().copied());
    signed.sort();
    let names = signed.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(";");
    let date = headers.iter().find(|(name, _)| *name == "x-amz-date").map_or("", |(_, value)| value);
    let canonical = format!(
        "{method}\n{}\n{}\n{}\n{names}\n{payload}",
        path.join("/"),
        query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"),
        signed.iter().map(|(name, value)| format!("{name}:{}\n", value.trim())).collect::<String>(),
    );
    let scope = format!("{}/{region}/{service}/aws4_request", &date[..date.len().min(8)]);
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let to_sign = format!("AWS4-HMAC-SHA256\n{date}\n{scope}\n{}", hex(&Sha256::digest(canonical.as_bytes())));
    let mac = |key: &[u8], data: &str| hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes()).as_ref().to_vec();
    let mut signing = format!("AWS4{secret}").into_bytes();
    for part in [&date[..date.len().min(8)], region, service, "aws4_request"] {
        signing = mac(&signing, part);
    }
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={names}, Signature={}", hex(&mac(&signing, &to_sign)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// AWS's own SigV4 test suite (aws-c-auth, `tests/aws-signing-test-suite/v4`): requests with
    /// their published signatures.
    #[test]
    fn signatures_match_the_published_test_suite() {
        let (key, secret, empty) = ("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        let date = ("x-amz-date", "20150830T123600Z");
        let token = "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267";
        for (path, headers, signature) in [
            ("/", vec![date], "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"),
            ("/?Param2=value2&Param1=value1", vec![date], "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"),
            ("/ሴ", vec![date], "8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85"),
            ("/-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", vec![date], "07ef7494c76fa4850883e2b006601f940f8a34d404d0cfa977f52a65bbf5f24f"),
            ("/", vec![date, ("x-amz-security-token", token)], "07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8"),
        ] {
            let url = Url::parse(&format!("https://example.amazonaws.com{path}")).unwrap();
            let value = authorization(key, secret, "us-east-1", "service", "GET", &url, &headers, empty);
            assert!(value.ends_with(&format!("Signature={signature}")), "{path}: {value}");
            assert!(value.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date"));
        }
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
    }

    /// HMAC-SHA256 written out from SHA-256 (RFC 2104), apart from the signer's.
    fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut block = if key.len() > 64 { Sha256::digest(key).to_vec() } else { key.to_vec() };
        block.resize(64, 0);
        let inner: Vec<u8> = block.iter().map(|b| b ^ 0x36).chain(data.iter().copied()).collect();
        let outer: Vec<u8> = block.iter().map(|b| b ^ 0x5c).chain(Sha256::digest(&inner)).collect();
        Sha256::digest(&outer).to_vec()
    }

    /// The signature a server expects of the request it got, worked out from what it got: its
    /// line, the headers its Authorization names, `secret`, and the date's region "us-east-1".
    fn expected(secret: &str, head: &str) -> Option<String> {
        let mut lines = head.split("\r\n");
        let mut line = lines.next()?.split(' ');
        let (method, target) = (line.next()?, line.next()?);
        let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
        let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        let auth = header("authorization")?;
        let names = auth.split("SignedHeaders=").nth(1)?.split(',').next()?;
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut pairs: Vec<&str> = query.split('&').filter(|p| !p.is_empty()).collect();
        pairs.sort();
        let query: Vec<String> = pairs.iter().map(|p| if p.contains('=') { p.to_string() } else { format!("{p}=") }).collect();
        let canonical_headers: String = names.split(';').map(|n| format!("{n}:{}\n", header(n).unwrap_or_default())).collect();
        let date = header("x-amz-date")?;
        let canonical = format!("{method}\n{path}\n{}\n{canonical_headers}\n{names}\n{}", query.join("&"), header("x-amz-content-sha256")?);
        let hex = |b: &[u8]| b.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let scope = format!("{}/us-east-1/s3/aws4_request", &date[..8]);
        let to_sign = format!("AWS4-HMAC-SHA256\n{date}\n{scope}\n{}", hex(&Sha256::digest(canonical.as_bytes())));
        let mut key = format!("AWS4{secret}").into_bytes();
        for part in [&date[..8], "us-east-1", "s3", "aws4_request"] {
            key = hmac_sha256(&key, part.as_bytes());
        }
        Some(hex(&hmac_sha256(&key, to_sign.as_bytes())))
    }

    /// An S3-compatible server on loopback (a MinIO, say) whose clock is an hour ahead: it lists
    /// bucket `bk` and serves `bk/a b.txt` (ranges too) to a request signed with `secret` for its
    /// time, with the session token, and turns every other away (403, with its `Date`).
    async fn minio(secret: &'static str, token: &'static str, content: Vec<u8>) -> Url {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let content = std::sync::Arc::new(content);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let content = content.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 3600;
                    let sent = head.split("\r\n").find_map(|l| l.to_ascii_lowercase().strip_prefix("x-amz-date:").map(|d| d.trim().to_ascii_uppercase()));
                    let on_time = sent.is_some_and(|d| d == amz_date(now) || d == amz_date(now - 1) || d == amz_date(now + 1));
                    let signed = expected(secret, &head).is_some_and(|sig| head.contains(&format!("Signature={sig}")));
                    let tokened = head.to_ascii_lowercase().contains(&format!("x-amz-security-token: {}", token.to_ascii_lowercase()));
                    let target = head.split(' ').nth(1).unwrap_or_default().to_string();
                    let reply = |status: &str, extra: String, body: &[u8]| {
                        let date = crate::engine::http_date(now);
                        [format!("HTTP/1.1 {status}\r\nDate: {date}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n", body.len()).as_bytes(), body].concat()
                    };
                    let answer = if !(on_time && signed && tokened) {
                        reply("403 Forbidden", String::new(), b"<Error><Code>AccessDenied</Code></Error>")
                    } else if target == "/bk/?list-type=2&prefix=" {
                        let xml = format!("<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>a b.txt</Key><Size>{}</Size></Contents></ListBucketResult>", content.len());
                        reply("200 OK", String::new(), xml.as_bytes())
                    } else if target == "/bk/a%20b.txt" {
                        let range = head.split("\r\n").find_map(|l| l.to_ascii_lowercase().strip_prefix("range: bytes=").map(str::to_string));
                        let (start, end) = range.and_then(|r| {
                            let (a, b) = r.split_once('-')?;
                            Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?.min(content.len() - 1)))
                        }).unwrap_or((0, content.len() - 1));
                        let ranged = format!("Accept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{}\r\nETag: \"v1\"\r\n", content.len());
                        reply("206 Partial Content", ranged, &content[start..=end])
                    } else {
                        reply("404 Not Found", String::new(), b"<Error><Code>NoSuchKey</Code></Error>")
                    };
                    let _ = socket.write_all(&answer).await;
                });
            }
        });
        url
    }

    /// With the key, its token and its endpoint, a bucket on an S3-compatible server is listed
    /// and its object downloaded, every request signed as the server works it out (an hour ahead
    /// of this computer's clock, learned from its first refusal); without the key it is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_private_bucket_is_listed_and_downloaded_with_signed_requests() {
        let content = crate::netsim::file(200_000);
        let endpoint = minio("SECRETKEY", "TOKEN", content.clone()).await;
        let key = S3Key {
            access_key: "AKID".into(),
            secret: "SECRETKEY".into(),
            session_token: Some("TOKEN".into()),
            region: None,
            endpoint: Some(endpoint.clone()),
        };
        let folder = endpoint.join("/bk/").unwrap();
        let options = crate::ingest::ListOptions { s3: Some(key.clone()), ..Default::default() };
        assert!(crate::cloud::endpoint_lists(&folder, &options));
        let tasks = crate::cloud::list(&reqwest::Client::new(), &folder, &options).await.unwrap().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!((tasks[0].urls[0].path(), tasks[0].size), ("/bk/a%20b.txt", Some(200_000)));

        let dir = tempfile::tempdir().unwrap();
        let download = |key: Option<S3Key>, name: &str| {
            let options = crate::engine::DownloadOptions {
                s3: key,
                output_path: Some(dir.path().join(name)),
                remote_checksum: false,
                ..Default::default()
            };
            crate::engine::DownloadEngine::new(tasks[0].urls.clone(), options)
        };
        let path = download(Some(key.clone()), "signed.txt").run(None).await.unwrap();
        assert!(std::fs::read(path).unwrap() == content);
        let refused = download(None, "unsigned.txt").run(None).await.unwrap_err();
        assert!(refused.contains("403"), "{refused}");
        let wrong = S3Key { secret: "WRONG".into(), ..key };
        let refused = crate::cloud::list(&reqwest::Client::new(), &folder, &crate::ingest::ListOptions { s3: Some(wrong), ..Default::default() }).await.unwrap();
        assert!(refused.unwrap_err().contains("refused the access key"));
    }

    /// Only hosts that serve S3 are signed for, each in the region its address names.
    #[test]
    fn keys_go_to_s3_hosts_alone_in_their_region() {
        let key = S3Key { access_key: "AK".into(), secret: "S".into(), endpoint: Url::parse("http://minio.lan:9000").ok(), ..Default::default() };
        let region = |link: &str| key.applies(&Url::parse(link).unwrap()).then(|| key.region_for(&Url::parse(link).unwrap()));
        assert_eq!(region("https://bk.s3.amazonaws.com/a").as_deref(), Some("us-east-1"));
        assert_eq!(region("https://bk.s3.eu-west-1.amazonaws.com/a").as_deref(), Some("eu-west-1"));
        assert_eq!(region("https://s3-ap-south-1.amazonaws.com/bk/a").as_deref(), Some("ap-south-1"));
        assert_eq!(region("https://bk.s3.dualstack.us-east-2.amazonaws.com/a").as_deref(), Some("us-east-2"));
        assert_eq!(region("https://s3.eu-central-2.wasabisys.com/bk/a").as_deref(), Some("eu-central-2"));
        assert_eq!(region("https://s3.us-west-004.backblazeb2.com/bk/a").as_deref(), Some("us-west-004"));
        assert_eq!(region("https://bk.nyc3.digitaloceanspaces.com/a").as_deref(), Some("nyc3"));
        assert_eq!(region("https://acct.r2.cloudflarestorage.com/bk/a").as_deref(), Some("auto"));
        assert_eq!(region("http://minio.lan:9000/bk/a").as_deref(), Some("us-east-1"));
        for elsewhere in ["https://example.com/a", "http://minio.lan:9001/bk/a", "https://bk.s3-website-us-east-1.amazonaws.com/a", "https://ec2.amazonaws.com/"] {
            assert_eq!(region(elsewhere), None, "{elsewhere}");
        }
        let fixed = S3Key { region: Some("eu-north-1".into()), ..key.clone() };
        assert_eq!(fixed.region_for(&Url::parse("https://bk.s3.amazonaws.com/a").unwrap()), "eu-north-1");
        assert!(!format!("{key:?}").contains("AK") && !format!("{key:?}").contains("\"S\""));
    }
}
