//! Debrid services (Real-Debrid, AllDebrid, TorBox, Premiumize): hoster links unrestricted into
//! direct ones, and magnet links turned into direct downloads.
//!
//! The key goes in an Authorization header (Premiumize only takes it as a parameter, so it goes in
//! the form body or the query there), and never into an error: errors carry no request URL and
//! have the key blanked out.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use reqwest::{Client, Method};
use serde_json::Value;
use url::Url;

use crate::ingest::{clean_path, Task};
use crate::resolver::ResolverError;

/// AllDebrid wants every call to name the app.
const AGENT: &str = "hyperfetch";

/// How long a torrent (or a TorBox hoster download) the provider does not have yet may take to
/// get there before it is reported as not cached.
const WAIT: Duration = Duration::from_secs(60);

/// Hosters debrid services take, used when the provider's own list can't be fetched.
const BUILT_IN_HOSTS: &[&str] = &[
    "rapidgator.net", "rg.to", "1fichier.com", "nitroflare.com", "turbobit.net",
    "mega.nz", "mega.io", "mediafire.com", "filefactory.com", "uploaded.net",
    "ddownload.com", "katfile.com", "send.cm", "keep2share.cc", "k2s.cc",
    "doodstream.com", "dood.to", "dood.so", "dood.pm", "dood.watch", "ds2play.com",
    "streamtape.com", "mixdrop.co", "upstore.net", "filestore.to",
];

/// Each provider's supported-hosts list, fetched once per run (keyed by API base).
static HOSTS: LazyLock<Mutex<HashMap<String, Arc<Vec<String>>>>> = LazyLock::new(Default::default);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Provider {
    RealDebrid,
    AllDebrid,
    TorBox,
    Premiumize,
}

impl Provider {
    /// The provider a setting names; an empty or unknown name is None.
    fn named(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().replace(['-', '_', ' ', '.'], "").as_str() {
            "realdebrid" | "rd" => Some(Self::RealDebrid),
            "alldebrid" | "ad" => Some(Self::AllDebrid),
            "torbox" | "tb" => Some(Self::TorBox),
            "premiumize" | "premiumizeme" | "pm" => Some(Self::Premiumize),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::RealDebrid => "Real-Debrid",
            Self::AllDebrid => "AllDebrid",
            Self::TorBox => "TorBox",
            Self::Premiumize => "Premiumize",
        }
    }

    fn base(self) -> &'static str {
        match self {
            Self::RealDebrid => "https://api.real-debrid.com/rest/1.0",
            Self::AllDebrid => "https://api.alldebrid.com",
            Self::TorBox => "https://api.torbox.app/v1/api",
            Self::Premiumize => "https://www.premiumize.me/api",
        }
    }
}

/// The providers to try for `provider`: the one it names, else Real-Debrid then AllDebrid (an
/// unnamed key was always tried that way).
fn providers(provider: Option<&str>) -> Vec<Provider> {
    match provider.and_then(Provider::named) {
        Some(p) => vec![p],
        None => vec![Provider::RealDebrid, Provider::AllDebrid],
    }
}

/// One file a provider serves.
#[derive(Debug, PartialEq)]
struct File {
    /// Relative path from the provider ('/'-separated, untrusted).
    path: String,
    size: Option<u64>,
    url: Url,
}

/// One provider's API, reached with one key.
struct Api<'a> {
    http: &'a Client,
    provider: Provider,
    base: String,
    key: &'a str,
    wait: Duration,
}

impl<'a> Api<'a> {
    fn new(http: &'a Client, provider: Provider, key: &'a str) -> Self {
        Api { http, provider, base: provider.base().to_string(), key: key.trim(), wait: WAIT }
    }

    /// `message` with the provider's name in front and the key blanked out.
    fn error(&self, message: impl std::fmt::Display) -> String {
        let message = format!("{}: {}", self.provider.name(), message);
        if self.key.is_empty() { message } else { message.replace(self.key, "<key>") }
    }

    async fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, String> {
        self.call(Method::GET, path, params).await
    }

    async fn post(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, String> {
        self.call(Method::POST, path, params).await
    }

    /// The JSON answer to one call, or the provider's error message.
    async fn call(&self, method: Method, path: &str, params: &[(&str, &str)]) -> Result<Value, String> {
        let mut params = params.to_vec();
        let mut request = self.http.request(method.clone(), format!("{}{}", self.base, path)).timeout(Duration::from_secs(30));
        match self.provider {
            Provider::Premiumize => params.push(("apikey", self.key)),
            Provider::AllDebrid => request = request.query(&[("agent", AGENT)]).bearer_auth(self.key),
            _ => request = request.bearer_auth(self.key),
        }
        request = if method == Method::GET { request.query(&params) } else { request.form(&params) };
        // `without_url`: Premiumize's key, and TorBox's on its download links, ride in the URL.
        let answer = request.send().await.map_err(|e| self.error(e.without_url()))?;
        let status = answer.status();
        let text = answer.text().await.map_err(|e| self.error(e.without_url()))?;
        let body: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) };
        let failed = !status.is_success() || body["status"] == "error" || body["success"] == false;
        if !failed {
            return Ok(body);
        }
        let message = [&body["error"]["message"], &body["detail"], &body["message"], &body["error"]]
            .into_iter()
            .find_map(Value::as_str)
            .map(|m| m.replace('_', " "))
            .unwrap_or_else(|| format!("HTTP {}", status));
        Err(self.error(message))
    }

    /// A file from the provider's fields; its link must be http(s).
    fn file(&self, url: &Value, path: &Value, size: &Value) -> Result<File, String> {
        let url = url
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or_else(|| self.error("no download link in the answer"))?;
        Ok(File { path: path.as_str().unwrap_or_default().to_string(), size: size.as_u64(), url })
    }

    /// The id the provider gave a new torrent or download (a number or a string).
    fn id(&self, value: &Value) -> Result<String, String> {
        value.as_str().map(str::to_string).or_else(|| value.as_u64().map(|n| n.to_string())).ok_or_else(|| self.error("no id in the answer"))
    }

    /// Waits a little before asking again, or gives up once the wait is over.
    async fn pause(&self, started: Instant, percent: f64) -> Result<(), String> {
        if started.elapsed() >= self.wait {
            return Err(self.error(format!(
                "this isn't cached yet ({:.0}% fetched so far); it keeps going in your account, so try again later",
                percent
            )));
        }
        tokio::time::sleep(self.wait / 30).await;
        Ok(())
    }

    /// The direct file behind the hoster link `link`.
    async fn link(&self, link: &str) -> Result<File, String> {
        match self.provider {
            Provider::RealDebrid => {
                let v = self.post("/unrestrict/link", &[("link", link)]).await?;
                self.file(&v["download"], &v["filename"], &v["filesize"])
            }
            Provider::AllDebrid => {
                let v = self.get("/v4/link/unlock", &[("link", link)]).await?;
                self.file(&v["data"]["link"], &v["data"]["filename"], &v["data"]["filesize"])
            }
            Provider::TorBox => self.torbox(true, link).await?.1.into_iter().next().ok_or_else(|| self.error("no file")),
            Provider::Premiumize => self.premiumize_direct(link).await?.into_iter().next().ok_or_else(|| self.error("no file")),
        }
    }

    /// The torrent's name and files, once the provider has them all.
    async fn magnet(&self, magnet: &str) -> Result<(String, Vec<File>), String> {
        match self.provider {
            Provider::RealDebrid => self.real_debrid(magnet).await,
            Provider::AllDebrid => self.alldebrid(magnet).await,
            Provider::TorBox => self.torbox(false, magnet).await,
            Provider::Premiumize => self.premiumize(magnet).await,
        }
    }

    async fn real_debrid(&self, magnet: &str) -> Result<(String, Vec<File>), String> {
        let id = self.id(&self.post("/torrents/addMagnet", &[("magnet", magnet)]).await?["id"])?;
        let (started, mut selected) = (Instant::now(), false);
        let info = loop {
            let info = self.get(&format!("/torrents/info/{}", id), &[]).await?;
            match info["status"].as_str().unwrap_or_default() {
                "downloaded" => break info,
                "waiting_files_selection" if !selected => {
                    self.post(&format!("/torrents/selectFiles/{}", id), &[("files", "all")]).await?;
                    selected = true;
                    continue;
                }
                s @ ("magnet_error" | "error" | "virus" | "dead") => return Err(self.error(format!("torrent {}", s.replace('_', " ")))),
                _ => self.pause(started, info["progress"].as_f64().unwrap_or_default()).await?,
            }
        };
        let mut files = Vec::new();
        for link in info["links"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            files.push(self.link(link).await?);
        }
        Ok((info["filename"].as_str().unwrap_or_default().to_string(), files))
    }

    async fn alldebrid(&self, magnet: &str) -> Result<(String, Vec<File>), String> {
        let upload = self.post("/v4/magnet/upload", &[("magnets[]", magnet)]).await?;
        let added = &upload["data"]["magnets"][0];
        if let Some(message) = added["error"]["message"].as_str() {
            return Err(self.error(message));
        }
        let id = self.id(&added["id"])?;
        let started = Instant::now();
        loop {
            let status = one(&self.get("/v4.1/magnet/status", &[("id", id.as_str())]).await?["data"]["magnets"]).clone();
            match status["statusCode"].as_u64() {
                Some(4) => break,
                Some(code) if code >= 5 => return Err(self.error(status["status"].as_str().unwrap_or("the torrent failed"))),
                _ => {
                    let (done, size) = (status["downloaded"].as_f64().unwrap_or_default(), status["size"].as_f64().unwrap_or_default());
                    self.pause(started, if size > 0.0 { done * 100.0 / size } else { 0.0 }).await?
                }
            }
        };
        let tree = self.get("/v4/magnet/files", &[("id[]", id.as_str())]).await?;
        let mut links = Vec::new();
        walk(&one(&tree["data"]["magnets"])["files"], "", &mut links);
        let mut files = Vec::new();
        for (path, link) in links {
            files.push(File { path, ..self.link(&link).await? });
        }
        // The file tree already holds the torrent's folder.
        Ok((String::new(), files))
    }

    /// A TorBox torrent (`web` false, `source` a magnet) or hoster download (`web` true).
    async fn torbox(&self, web: bool, source: &str) -> Result<(String, Vec<File>), String> {
        let (kind, create, field, id_key, id_param) = if web {
            ("webdl", "createwebdownload", "link", "webdownload_id", "web_id")
        } else {
            ("torrents", "createtorrent", "magnet", "torrent_id", "torrent_id")
        };
        let made = self.post(&format!("/{}/{}", kind, create), &[(field, source)]).await?;
        let id = self.id(&made["data"][id_key])?;
        let started = Instant::now();
        let item = loop {
            let item = self.get(&format!("/{}/mylist", kind), &[("id", id.as_str()), ("bypass_cache", "true")]).await?["data"].clone();
            if item["download_present"] == true {
                break item;
            }
            if item["download_state"].as_str().is_some_and(|s| s.contains("error") || s == "failed") {
                return Err(self.error(format!("the download failed ({})", item["download_state"].as_str().unwrap_or_default())));
            }
            self.pause(started, item["progress"].as_f64().unwrap_or_default() * 100.0).await?;
        };
        let mut files = Vec::new();
        for file in item["files"].as_array().into_iter().flatten() {
            let file_id = self.id(&file["id"])?;
            // TorBox's download-link call takes the key only as `token`.
            let params = [("token", self.key), (id_param, id.as_str()), ("file_id", file_id.as_str()), ("zip_link", "false")];
            let link = self.get(&format!("/{}/requestdl", kind), &params).await?;
            let path = if file["short_name"].is_string() { &file["short_name"] } else { &file["name"] };
            files.push(self.file(&link["data"], path, &file["size"])?);
        }
        Ok((item["name"].as_str().unwrap_or_default().to_string(), files))
    }

    /// The files Premiumize serves straight away for `source` (a hoster link or a cached torrent).
    async fn premiumize_direct(&self, source: &str) -> Result<Vec<File>, String> {
        let v = self.post("/transfer/directdl", &[("src", source)]).await?;
        v["content"].as_array().into_iter().flatten().map(|c| self.file(&c["link"], &c["path"], &c["size"])).collect()
    }

    async fn premiumize(&self, magnet: &str) -> Result<(String, Vec<File>), String> {
        // Paths from Premiumize already hold the torrent's folder.
        if let Some(files) = self.premiumize_direct(magnet).await.ok().filter(|f| !f.is_empty()) {
            return Ok((String::new(), files));
        }
        let id = self.id(&self.post("/transfer/create", &[("src", magnet)]).await?["id"])?;
        let started = Instant::now();
        loop {
            let list = self.get("/transfer/list", &[]).await?;
            let transfer = list["transfers"].as_array().into_iter().flatten().find(|t| self.id(&t["id"]).ok().as_deref() == Some(id.as_str()));
            let transfer = transfer.cloned().unwrap_or(Value::Null);
            match transfer["status"].as_str().unwrap_or_default() {
                "finished" | "seeding" => break,
                s @ ("error" | "deleted" | "banned" | "timeout") => return Err(self.error(transfer["message"].as_str().unwrap_or(s))),
                _ => self.pause(started, transfer["progress"].as_f64().unwrap_or_default() * 100.0).await?,
            }
        }
        Ok((String::new(), self.premiumize_direct(magnet).await?))
    }

    /// The provider's supported hosters, fetched once per run; empty when they can't be had.
    async fn hosts(&self) -> Arc<Vec<String>> {
        if let Some(hosts) = HOSTS.lock().unwrap().get(&self.base) {
            return Arc::clone(hosts);
        }
        let (path, lists): (_, &[&str]) = match self.provider {
            Provider::RealDebrid => ("/hosts/domains", &[""]),
            Provider::AllDebrid => ("/v4/hosts/domains", &["/data/hosts"]),
            Provider::TorBox => ("/webdl/hosters", &["/data"]),
            Provider::Premiumize => ("/services/list", &["/directdl", "/cache"]),
        };
        let answer = self.get(path, &[]).await.unwrap_or_default();
        let mut hosts = Vec::new();
        for item in lists.iter().filter_map(|l| answer.pointer(l)?.as_array()).flatten() {
            // TorBox lists hosters with their domains; the others list domains.
            match item["domains"].as_array() {
                Some(domains) => hosts.extend(domains.iter().filter_map(Value::as_str).map(str::to_ascii_lowercase)),
                None => hosts.extend(item.as_str().map(str::to_ascii_lowercase)),
            }
        }
        let hosts = Arc::new(hosts);
        HOSTS.lock().unwrap().insert(self.base.clone(), Arc::clone(&hosts));
        hosts
    }
}

/// The first of `value` when it is a list, else `value` itself (AllDebrid answers both ways).
fn one(value: &Value) -> &Value {
    value.get(0).unwrap_or(value)
}

/// The (path, link) of every file in an AllDebrid file tree.
fn walk(entries: &Value, folder: &str, out: &mut Vec<(String, String)>) {
    for entry in entries.as_array().into_iter().flatten() {
        let path = format!("{}{}", folder, entry["n"].as_str().unwrap_or_default());
        match (&entry["e"], entry["l"].as_str()) {
            (Value::Array(_), _) => walk(&entry["e"], &format!("{}/", path), out),
            (_, Some(link)) => out.push((path, link.to_string())),
            _ => {}
        }
    }
}

/// One task per file. Several files go in a folder named after the torrent (when `name` is not
/// empty); every name is cleaned, so none escapes the save folder.
fn tasks(name: &str, files: Vec<File>) -> Vec<Task> {
    let folder = (files.len() > 1).then_some(name).filter(|n| !n.is_empty());
    files
        .into_iter()
        .map(|file| {
            let parts = folder.into_iter().chain(file.path.split(['/', '\\']).filter(|p| !p.is_empty()));
            let name = clean_path(parts).ok().filter(|p| !p.as_os_str().is_empty());
            Task { urls: vec![file.url], name, size: file.size, from_document: true, ..Task::default() }
        })
        .collect()
}

/// The direct downloads of the torrent `magnet` names, through the debrid `provider` (`""` when
/// unset): one task per file, once the provider has them all. A torrent it doesn't have within a
/// minute is an error (it keeps fetching it, so a retry later finds it cached).
pub async fn magnet_tasks(http: &reqwest::Client, magnet: &str, key: &str, provider: &str) -> Result<Vec<Task>, String> {
    // Each provider in turn (Real-Debrid then AllDebrid for an unnamed key); the first error.
    let mut first = None;
    for provider in providers(Some(provider)) {
        let api = Api::new(http, provider, key);
        match api.magnet(magnet).await {
            Ok((_, files)) if files.is_empty() => first = first.or(Some(api.error("no files in this torrent"))),
            Ok((name, files)) => return Ok(tasks(&name, files)),
            Err(e) => first = first.or(Some(e)),
        }
    }
    Err(first.unwrap_or_default())
}

/// The direct link the debrid `provider` gives for the hoster link `url`.
pub async fn unrestrict(client: &reqwest::Client, url: &Url, key: &str, provider: Option<&str>) -> Result<Url, ResolverError> {
    let mut first = None;
    for provider in providers(provider) {
        match Api::new(client, provider, key).link(url.as_str()).await {
            Ok(file) => return Ok(file.url),
            Err(e) => first = first.or(Some(e)),
        }
    }
    Err(ResolverError::NotFound(first.unwrap_or_default()))
}

/// Whether `url` is on a hoster debrid services unrestrict (the built-in list).
pub fn is_debrid_host(url: &Url) -> bool {
    on_host(url, BUILT_IN_HOSTS.iter().copied())
}

/// Whether the debrid `provider` takes `url`: the built-in list, or the provider's own list
/// (fetched once per run; the first provider's when none is named).
pub async fn supports(client: &reqwest::Client, url: &Url, key: &str, provider: Option<&str>) -> bool {
    if is_debrid_host(url) {
        return true;
    }
    let api = Api::new(client, providers(provider)[0], key);
    on_host(url, api.hosts().await.iter().map(String::as_str))
}

/// Whether `url`'s host is one of `domains` or under one.
fn on_host<'d>(url: &Url, mut domains: impl Iterator<Item = &'d str>) -> bool {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    !host.is_empty() && domains.any(|d| host == d || host.strip_suffix(d).is_some_and(|rest| rest.ends_with('.')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const KEY: &str = "SECRETKEY123";

    /// What a mock provider saw: request line, Authorization header, body.
    type Seen = Arc<Mutex<Vec<(String, Option<String>, String)>>>;

    /// A local stand-in for a provider API at the base URL returned, answering each request with
    /// `answer(method, target, body)` as JSON.
    async fn serve(answer: fn(&str, &str, &str) -> (u16, String)) -> (String, Seen) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Seen::default();
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut data = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                        let length = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                        if data.len() >= end + 4 + length {
                            break;
                        }
                    }
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => data.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&data).into_owned();
                let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                let mut words = head.split(' ');
                let (method, target) = (words.next().unwrap_or_default().to_string(), words.next().unwrap_or_default().to_string());
                let auth = head.lines().find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.eq_ignore_ascii_case("authorization").then(|| value.trim().to_string())
                });
                log.lock().unwrap().push((format!("{} {}", method, target), auth, body.to_string()));
                let (status, reply) = answer(&method, &target, body);
                let head = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.len());
                let _ = socket.write_all((head + &reply).as_bytes()).await;
            }
        });
        (base, seen)
    }

    fn api<'a>(http: &'a Client, provider: Provider, base: &str, key: &'a str) -> Api<'a> {
        Api { http, provider, base: base.to_string(), key, wait: Duration::from_millis(300) }
    }

    /// A query or form parameter of a request.
    fn param(target: &str, body: &str, name: &str) -> Option<String> {
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
        url::form_urlencoded::parse(format!("{}&{}", query, body).as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
    }

    const MAGNET: &str = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Pack";

    /// Real-Debrid: `cached` torrents finish as soon as their files are selected, others stay
    /// at 12%.
    fn real_debrid(cached: bool, target: &str, body: &str) -> (u16, String) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static SELECTED: AtomicBool = AtomicBool::new(false);
        let r = match target.split('?').next().unwrap() {
            "/torrents/addMagnet" => r#"{"id": "T1", "uri": "x"}"#,
            "/torrents/selectFiles/T1" if body == "files=all" => {
                SELECTED.store(true, Ordering::SeqCst);
                return (204, String::new());
            }
            "/torrents/info/T1" if cached && !SELECTED.load(Ordering::SeqCst) => r#"{"status": "waiting_files_selection", "progress": 0}"#,
            "/torrents/info/T1" if cached => {
                r#"{"status": "downloaded", "filename": "Pack", "progress": 100, "links": ["https://real-debrid.com/d/A", "https://real-debrid.com/d/B"]}"#
            }
            "/torrents/info/T1" => r#"{"status": "downloading", "progress": 12, "links": []}"#,
            "/hosts/domains" => r#"["rapidgator.net", "Example-Hoster.com"]"#,
            "/unrestrict/link" => {
                let (name, size) = match param("", body, "link").as_deref() {
                    Some("https://real-debrid.com/d/A") => ("a.mkv", 5),
                    Some("https://real-debrid.com/d/B") => ("../../evil.txt", 7),
                    Some("https://hoster.test/f/1") => ("one.zip", 9),
                    _ => return (503, r#"{"error": "hoster_unavailable", "error_code": 19}"#.into()),
                };
                return (200, format!(r#"{{"download": "https://dl.test/{name}", "filename": "{name}", "filesize": {size}}}"#));
            }
            _ => return (404, r#"{"error": "unknown_ressource", "error_code": 7}"#.into()),
        };
        (200, r.into())
    }

    #[tokio::test]
    async fn real_debrid_links_and_magnets() {
        let http = Client::new();
        let (base, seen) = serve(|_, t, b| real_debrid(true, t, b)).await;
        let rd = api(&http, Provider::RealDebrid, &base, KEY);
        let file = rd.link("https://hoster.test/f/1").await.unwrap();
        assert_eq!((file.url.as_str(), file.size), ("https://dl.test/one.zip", Some(9)));
        let err = rd.link("https://hoster.test/gone").await.unwrap_err();
        assert_eq!(err, "Real-Debrid: hoster unavailable");

        let (name, files) = rd.magnet(MAGNET).await.unwrap();
        let tasks = tasks(&name, files);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name.as_deref(), Some(std::path::Path::new("Pack/a.mkv")));
        assert_eq!((tasks[0].size, tasks[0].from_document), (Some(5), true));
        // An untrusted name never leaves the torrent's folder.
        assert!(tasks[1].name.as_ref().is_none_or(|n| n.starts_with("Pack") && !n.components().any(|c| c.as_os_str() == "..")), "{:?}", tasks[1].name);
        let seen = seen.lock().unwrap().clone();
        assert!(seen.iter().any(|(r, _, b)| r.starts_with("POST /torrents/selectFiles/T1") && b == "files=all"));
        assert!(seen.iter().all(|(r, auth, _)| auth.as_deref() == Some(&*format!("Bearer {KEY}")) && !r.contains(KEY)));

        let (base, _) = serve(|_, t, b| real_debrid(false, t, b)).await;
        let err = api(&http, Provider::RealDebrid, &base, KEY).magnet(MAGNET).await.unwrap_err();
        assert!(err.starts_with("Real-Debrid: this isn't cached yet (12% fetched"), "{err}");

        let rd = api(&http, Provider::RealDebrid, &base, KEY);
        assert!(on_host(&Url::parse("https://www.example-hoster.com/x").unwrap(), rd.hosts().await.iter().map(String::as_str)));
        assert!(!on_host(&Url::parse("https://notexample-hoster.com/x").unwrap(), rd.hosts().await.iter().map(String::as_str)));
    }

    #[tokio::test]
    async fn alldebrid_links_and_magnets() {
        fn answer(cached: bool, target: &str, body: &str) -> (u16, String) {
            let path = target.split('?').next().unwrap();
            if param(target, body, "agent").as_deref() != Some(AGENT) {
                return (400, r#"{"status": "error", "error": {"code": "NO_AGENT", "message": "No agent"}}"#.into());
            }
            let r = match path {
                "/v4/link/unlock" => match param(target, body, "link").as_deref() {
                    Some("https://hoster.test/f/1") => r#"{"status": "success", "data": {"link": "https://dl.test/one.zip", "filename": "one.zip", "filesize": 9}}"#,
                    Some("https://alldebrid.com/f/A") => r#"{"status": "success", "data": {"link": "https://dl.test/a", "filename": "a", "filesize": 5}}"#,
                    Some("https://alldebrid.com/f/B") => r#"{"status": "success", "data": {"link": "https://dl.test/b", "filename": "b", "filesize": 7}}"#,
                    _ => r#"{"status": "error", "error": {"code": "LINK_DOWN", "message": "This link is not available on the file hoster website"}}"#,
                },
                "/v4/magnet/upload" if param(target, body, "magnets[]").as_deref() == Some(MAGNET) => {
                    r#"{"status": "success", "data": {"magnets": [{"id": 42, "ready": true, "name": "Pack"}]}}"#
                }
                "/v4.1/magnet/status" if cached => r#"{"status": "success", "data": {"magnets": {"id": 42, "filename": "Pack", "statusCode": 4, "status": "Ready"}}}"#,
                "/v4.1/magnet/status" => r#"{"status": "success", "data": {"magnets": {"id": 42, "statusCode": 1, "status": "Downloading", "downloaded": 25, "size": 100}}}"#,
                "/v4/magnet/files" => {
                    r#"{"status": "success", "data": {"magnets": [{"id": "42", "files": [{"n": "Pack", "e": [{"n": "a.mkv", "s": 5, "l": "https://alldebrid.com/f/A"}, {"n": "Sub", "e": [{"n": "b.txt", "s": 7, "l": "https://alldebrid.com/f/B"}]}]}]}]}}"#
                }
                _ => r#"{"status": "error", "error": {"code": "X", "message": "unexpected"}}"#,
            };
            (200, r.into())
        }
        let http = Client::new();
        let (base, seen) = serve(|_, t, b| answer(true, t, b)).await;
        let ad = api(&http, Provider::AllDebrid, &base, KEY);
        assert_eq!(ad.link("https://hoster.test/f/1").await.unwrap().url.as_str(), "https://dl.test/one.zip");
        assert_eq!(ad.link("https://hoster.test/x").await.unwrap_err(), "AllDebrid: This link is not available on the file hoster website");
        let (name, files) = ad.magnet(MAGNET).await.unwrap();
        let tasks = tasks(&name, files);
        let names: Vec<_> = tasks.iter().map(|t| t.name.clone().unwrap()).collect();
        assert_eq!(names, [std::path::PathBuf::from("Pack/a.mkv"), std::path::PathBuf::from("Pack/Sub/b.txt")]);
        assert_eq!(tasks[1].urls[0].as_str(), "https://dl.test/b");
        // The key goes in the header, not the query (it used to be `apikey=` there).
        assert!(seen.lock().unwrap().iter().all(|(r, auth, b)| auth.as_deref() == Some(&*format!("Bearer {KEY}")) && !r.contains(KEY) && !b.contains(KEY)));

        let (base, _) = serve(|_, t, b| answer(false, t, b)).await;
        let err = api(&http, Provider::AllDebrid, &base, KEY).magnet(MAGNET).await.unwrap_err();
        assert!(err.starts_with("AllDebrid: this isn't cached yet (25% fetched"), "{err}");
    }

    #[tokio::test]
    async fn torbox_links_and_magnets() {
        fn answer(cached: bool, target: &str, body: &str) -> (u16, String) {
            let path = target.split('?').next().unwrap();
            let r = match path {
                "/torrents/createtorrent" if param(target, body, "magnet").as_deref() == Some(MAGNET) => {
                    r#"{"success": true, "detail": "Found cached torrent.", "data": {"torrent_id": 7, "hash": "h"}}"#
                }
                "/webdl/createwebdownload" => r#"{"success": true, "data": {"webdownload_id": 8, "hash": "w"}}"#,
                "/torrents/mylist" if cached => {
                    r#"{"success": true, "data": {"id": 7, "name": "Pack", "download_present": true, "files": [{"id": 0, "name": "Pack/a.mkv", "short_name": "a.mkv", "size": 5}, {"id": 1, "name": "Pack/b.txt", "short_name": "b.txt", "size": 7}]}}"#
                }
                "/torrents/mylist" => r#"{"success": true, "data": {"id": 7, "name": "Pack", "download_present": false, "download_state": "downloading", "progress": 0.5}}"#,
                "/webdl/mylist" => r#"{"success": true, "data": {"id": 8, "name": "one.zip", "download_present": true, "files": [{"id": 3, "short_name": "one.zip", "size": 9}]}}"#,
                "/torrents/requestdl" | "/webdl/requestdl" if param(target, body, "token").as_deref() == Some(KEY) => {
                    return (200, format!(r#"{{"success": true, "data": "https://dl.test/{}-{}"}}"#, path.split('/').nth(1).unwrap(), param(target, body, "file_id").unwrap()));
                }
                _ => r#"{"success": false, "error": "BAD_TOKEN", "detail": "Invalid API token."}"#,
            };
            (200, r.into())
        }
        let http = Client::new();
        let (base, seen) = serve(|_, t, b| answer(true, t, b)).await;
        let tb = api(&http, Provider::TorBox, &base, KEY);
        assert_eq!(tb.link("https://hoster.test/f/1").await.unwrap().url.as_str(), "https://dl.test/webdl-3");
        let (name, files) = tb.magnet(MAGNET).await.unwrap();
        let tasks = tasks(&name, files);
        assert_eq!(tasks.iter().map(|t| (t.name.clone().unwrap(), t.size)).collect::<Vec<_>>(), [("Pack/a.mkv".into(), Some(5)), ("Pack/b.txt".into(), Some(7))]);
        assert_eq!(tasks[1].urls[0].as_str(), "https://dl.test/torrents-1");
        assert!(seen.lock().unwrap().iter().all(|(_, auth, _)| auth.as_deref() == Some(&*format!("Bearer {KEY}"))));

        let (base, _) = serve(|_, t, b| answer(false, t, b)).await;
        let err = api(&http, Provider::TorBox, &base, KEY).magnet(MAGNET).await.unwrap_err();
        assert!(err.starts_with("TorBox: this isn't cached yet (50% fetched"), "{err}");
        let err = api(&http, Provider::TorBox, &base, "WRONG").magnet("magnet:?xt=urn:btih:other").await.unwrap_err();
        assert_eq!(err, "TorBox: Invalid API token.");
    }

    #[tokio::test]
    async fn premiumize_links_and_magnets() {
        fn answer(cached: bool, target: &str, body: &str) -> (u16, String) {
            if param(target, body, "apikey").as_deref() != Some(KEY) {
                return (200, r#"{"status": "error", "message": "Not logged in."}"#.into());
            }
            let r = match (target.split('?').next().unwrap(), param(target, body, "src").as_deref()) {
                ("/transfer/directdl", Some("https://hoster.test/f/1")) => r#"{"status": "success", "content": [{"path": "one.zip", "size": 9, "link": "https://dl.test/one.zip"}]}"#,
                ("/transfer/directdl", Some(MAGNET)) if cached => {
                    r#"{"status": "success", "content": [{"path": "Pack/a.mkv", "size": 5, "link": "https://dl.test/a"}, {"path": "Pack/b.txt", "size": 7, "link": "https://dl.test/b"}]}"#
                }
                ("/transfer/create", Some(MAGNET)) => r#"{"status": "success", "id": "P1", "name": "Pack", "type": "torrent"}"#,
                ("/transfer/list", _) => r#"{"status": "success", "transfers": [{"id": "P0", "status": "finished"}, {"id": "P1", "status": "running", "progress": 0.3}]}"#,
                _ => r#"{"status": "error", "message": "content not in cache"}"#,
            };
            (200, r.into())
        }
        let http = Client::new();
        let (base, seen) = serve(|_, t, b| answer(true, t, b)).await;
        let pm = api(&http, Provider::Premiumize, &base, KEY);
        assert_eq!(pm.link("https://hoster.test/f/1").await.unwrap().url.as_str(), "https://dl.test/one.zip");
        let (name, files) = pm.magnet(MAGNET).await.unwrap();
        let tasks = tasks(&name, files);
        assert_eq!(tasks.iter().map(|t| t.name.clone().unwrap()).collect::<Vec<_>>(), [std::path::PathBuf::from("Pack/a.mkv"), "Pack/b.txt".into()]);
        // Premiumize takes the key only as a parameter: in the body of every POST.
        assert!(seen.lock().unwrap().iter().all(|(r, _, b)| r.starts_with("POST") && !r.contains(KEY) && b.contains(KEY)));

        let (base, _) = serve(|_, t, b| answer(false, t, b)).await;
        let err = api(&http, Provider::Premiumize, &base, KEY).magnet(MAGNET).await.unwrap_err();
        assert!(err.starts_with("Premiumize: this isn't cached yet (30% fetched"), "{err}");
        let err = api(&http, Provider::Premiumize, &base, "WRONG").link("https://hoster.test/f/1").await.unwrap_err();
        assert_eq!(err, "Premiumize: Not logged in.");
    }

    #[tokio::test]
    async fn bad_keys_name_the_provider_and_hide_the_key() {
        // Every provider says "bad token" its own way, and some echo what they were sent.
        fn answer(_: &str, target: &str, _: &str) -> (u16, String) {
            match target.split('?').next().unwrap() {
                "/unrestrict/link" | "/torrents/addMagnet" => (401, r#"{"error": "bad_token", "error_code": 8}"#.into()),
                "/v4/link/unlock" | "/v4/magnet/upload" => (200, r#"{"status": "error", "error": {"code": "AUTH_BAD_APIKEY", "message": "The auth apikey is invalid"}}"#.into()),
                _ => (403, format!(r#"{{"success": false, "detail": "token {KEY} is not valid"}}"#)),
            }
        }
        let http = Client::new();
        let (base, _) = serve(answer).await;
        for (provider, expected) in [
            (Provider::RealDebrid, "Real-Debrid: bad token"),
            (Provider::AllDebrid, "AllDebrid: The auth apikey is invalid"),
            (Provider::TorBox, "TorBox: token <key> is not valid"),
        ] {
            let api = api(&http, provider, &base, KEY);
            assert_eq!(api.link("https://hoster.test/f/1").await.unwrap_err(), expected);
            assert_eq!(api.magnet(MAGNET).await.unwrap_err(), expected);
        }
    }

    #[test]
    fn provider_names_and_hosts() {
        assert_eq!(providers(Some("real-debrid")), [Provider::RealDebrid]);
        assert_eq!(providers(Some("TorBox")), [Provider::TorBox]);
        assert_eq!(providers(Some("premiumize")), [Provider::Premiumize]);
        assert_eq!(providers(Some("")), [Provider::RealDebrid, Provider::AllDebrid]);
        assert_eq!(providers(None), [Provider::RealDebrid, Provider::AllDebrid]);
        assert!(is_debrid_host(&Url::parse("https://rapidgator.net/file/12345/video.mp4.html").unwrap()));
        assert!(is_debrid_host(&Url::parse("https://1fichier.com/?abcdef123").unwrap()));
        assert!(is_debrid_host(&Url::parse("https://www.doodstream.com/d/123").unwrap()));
        assert!(is_debrid_host(&Url::parse("https://streamtape.com/v/123").unwrap()));
        assert!(!is_debrid_host(&Url::parse("https://wikipedia.org/").unwrap()));
        assert!(!is_debrid_host(&Url::parse("https://notrapidgator.net/").unwrap()));
    }
}
