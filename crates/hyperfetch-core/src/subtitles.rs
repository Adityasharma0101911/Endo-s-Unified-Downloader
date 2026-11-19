//! Subtitles for a downloaded video from OpenSubtitles' REST API (`api.opensubtitles.com`), with
//! the user's own API key and, optionally, their account (more downloads a day): found by the
//! video's OpenSubtitles hash and size first, then by a title made of its file name, one per
//! language the user asks for (the one matching the hash, else the most downloaded), saved beside
//! it as `<name>.<language>.srt`. The daily download quota OpenSubtitles states is respected: when
//! it is used up the app stops and says when it resets.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const API: &str = "https://api.opensubtitles.com/api/v1";
/// Between two requests: OpenSubtitles allows five a second.
const GAP: Duration = Duration::from_millis(250);
/// The largest subtitle file read.
const MAX_SUBTITLE: usize = 8 << 20;
/// What the hash reads at each end of the file.
const CHUNK: u64 = 64 * 1024;

/// What finding subtitles needs (see `postprocess::PostOptions::subtitles`). Its `Debug` shows
/// the languages alone.
#[derive(Clone, Default)]
pub struct Subtitles {
    pub api_key: String,
    /// Language codes as OpenSubtitles writes them (`en`, `fr`, `pt-br`).
    pub languages: Vec<String>,
    /// The user's account, signed in for this run only: neither is saved by the app.
    pub user: Option<String>,
    pub password: Option<String>,
}

impl fmt::Debug for Subtitles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Subtitles {{ languages: {:?}, .. }}", self.languages)
    }
}

/// Finds and saves subtitles for `video` (see the module docs), through `proxy` when set:
/// the files written, or why there are none.
pub async fn find(video: &Path, subs: &Subtitles, proxy: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let client = |redirect: reqwest::redirect::Policy| {
        let mut client = Client::builder().connect_timeout(Duration::from_secs(15)).timeout(Duration::from_secs(60)).redirect(redirect);
        if let Some(proxy) = proxy {
            client = client.proxy(reqwest::Proxy::all(proxy).map_err(|e| format!("invalid proxy: {e}"))?);
        }
        client.build().map_err(|e| e.to_string())
    };
    // The API key and sign-in go to OpenSubtitles' API alone, never on to where it redirects.
    let (api, files) = (client(reqwest::redirect::Policy::none())?, client(reqwest::redirect::Policy::default())?);
    find_at(&api, &files, API, video, subs).await
}

/// [`find`], asking the API at `api` through `http`, which follows no redirect, and fetching the
/// subtitle files it links through `files`.
async fn find_at(http: &Client, files: &Client, api: &str, video: &Path, subs: &Subtitles) -> Result<Vec<PathBuf>, String> {
    let key = subs.api_key.trim();
    if key.is_empty() {
        return Err("OpenSubtitles needs your API key (from your account's API consumers page on opensubtitles.com): set it in Settings, or with --opensubtitles-key".to_string());
    }
    let mut languages: Vec<String> = subs.languages.iter().map(|l| l.trim().to_ascii_lowercase()).filter(|l| !l.is_empty()).collect();
    languages.sort();
    languages.dedup();
    if languages.is_empty() {
        return Err("no subtitle language is set".to_string());
    }
    // Subtitles already beside the video are kept, and not asked for again.
    let (have, languages): (Vec<String>, Vec<String>) = languages.into_iter().partition(|l| beside(video, l).exists());
    let mut saved: Vec<PathBuf> = have.iter().map(|l| beside(video, l)).collect();
    if languages.is_empty() {
        return Ok(saved);
    }
    let mut headers = HeaderMap::new();
    let mut secret = HeaderValue::from_str(key).map_err(|_| "the OpenSubtitles API key cannot be sent".to_string())?;
    secret.set_sensitive(true);
    headers.insert("Api-Key", secret);
    // OpenSubtitles asks each app to name itself and its version.
    headers.insert(USER_AGENT, HeaderValue::from_static(concat!("EndosUnifiedDownloader v", env!("CARGO_PKG_VERSION"))));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let mut api = Api { http, files, base: api.to_string(), headers };
    if let (Some(user), Some(password)) = (subs.user.as_deref().filter(|u| !u.is_empty()), subs.password.as_deref().filter(|p| !p.is_empty())) {
        api.sign_in(user, password).await?;
    }

    let found = match movie_hash(video).await {
        Some(hash) => api.search(&[("languages", languages.join(",")), ("moviehash", hash)]).await?,
        None => Vec::new(),
    };
    let mut picks: Vec<(String, u64)> = Vec::new();
    let mut missing = Vec::new();
    for language in &languages {
        match best(&found, language, true) {
            Some(pick) => picks.push((language.clone(), pick)),
            None => missing.push(language.clone()),
        }
    }
    if !missing.is_empty() {
        let title = title_of(video);
        if !title.is_empty() {
            let more = api.search(&[("languages", missing.join(",")), ("query", title)]).await?;
            picks.extend(missing.iter().filter_map(|l| Some((l.clone(), best(&more, l, false)?))));
        }
    }
    if picks.is_empty() {
        return Err(format!("OpenSubtitles has no {} subtitles for {}", languages.join(", "), display_name(video)));
    }
    for (language, file_id) in picks {
        let target = beside(video, &language);
        let (bytes, remaining) = api.download(file_id).await?;
        tokio::fs::write(&target, &bytes).await.map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
        saved.push(target);
        if remaining == Some(0) {
            break;
        }
    }
    Ok(saved)
}

#[derive(Deserialize)]
struct Found {
    #[serde(default)]
    data: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    attributes: Attributes,
}

#[derive(Deserialize)]
struct Attributes {
    #[serde(default)]
    language: String,
    #[serde(default)]
    download_count: u64,
    #[serde(default)]
    moviehash_match: bool,
    #[serde(default)]
    files: Vec<SubFile>,
}

#[derive(Deserialize)]
struct SubFile {
    file_id: u64,
}

/// Of `found`, the file id of the subtitle in `language` matching the video's hash (when
/// `by_hash`), else the most downloaded; of one file (subtitles in parts fit no single video).
fn best(found: &[Attributes], language: &str, by_hash: bool) -> Option<u64> {
    found
        .iter()
        .filter(|a| a.language.eq_ignore_ascii_case(language) && a.files.len() == 1 && (a.moviehash_match || !by_hash))
        .max_by_key(|a| a.download_count)
        .map(|a| a.files[0].file_id)
}

struct Api<'a> {
    http: &'a Client,
    files: &'a Client,
    base: String,
    headers: HeaderMap,
}

impl Api<'_> {
    /// Signs the user in for this run: their token goes with each request after, to the server
    /// OpenSubtitles names for them.
    async fn sign_in(&mut self, user: &str, password: &str) -> Result<(), String> {
        // One sign-in per user and half a day, as OpenSubtitles asks (its logins are limited; a
        // token lasts a day).
        static SIGNED_IN: std::sync::Mutex<Option<(String, String, HeaderValue, String, std::time::Instant)>> = std::sync::Mutex::new(None);
        let asked_at = self.base.clone();
        if let Some((_, _, bearer, base, _)) = SIGNED_IN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().filter(|(u, at, _, _, when)| u == user && *at == asked_at && when.elapsed() < Duration::from_secs(12 * 3600)) {
            self.headers.insert(AUTHORIZATION, bearer.clone());
            self.base = base.clone();
            return Ok(());
        }
        let url = format!("{}/login", self.base);
        let answer = self.post(&url, json!({"username": user, "password": password})).await?;
        if answer.status() == StatusCode::UNAUTHORIZED {
            return Err("OpenSubtitles refused your user name or password".to_string());
        }
        let found = read(answer).await?;
        let token = found["token"].as_str().ok_or("OpenSubtitles gave no sign-in token")?;
        let mut bearer = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| "OpenSubtitles gave a token that cannot be sent".to_string())?;
        bearer.set_sensitive(true);
        self.headers.insert(AUTHORIZATION, bearer.clone());
        if let Some(host) = found["base_url"].as_str().filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))) {
            // Only another host of OpenSubtitles' own.
            if host == "opensubtitles.com" || host.ends_with(".opensubtitles.com") {
                self.base = format!("https://{host}/api/v1");
            }
        }
        *SIGNED_IN.lock().unwrap_or_else(|e| e.into_inner()) = Some((user.to_string(), asked_at, bearer, self.base.clone(), std::time::Instant::now()));
        Ok(())
    }

    /// The subtitles a search with `params` finds; OpenSubtitles wants them sorted by name.
    async fn search(&self, params: &[(&str, String)]) -> Result<Vec<Attributes>, String> {
        let mut url = url::Url::parse(&format!("{}/subtitles", self.base)).map_err(|e| e.to_string())?;
        let mut sorted = params.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        url.query_pairs_mut().extend_pairs(sorted.iter().map(|(k, v)| (*k, v.to_ascii_lowercase())));
        let answer = crate::polite::send(&url, GAP, || self.http.get(url.clone()).headers(self.headers.clone())).await?;
        let found: Found = serde_json::from_value(read(answer).await?).map_err(|_| "OpenSubtitles answered with something the app cannot read".to_string())?;
        Ok(found.data.into_iter().map(|e| e.attributes).collect())
    }

    /// The SRT of subtitle file `file_id`, and the downloads left today; a used-up quota is an
    /// error that says when it resets.
    async fn download(&self, file_id: u64) -> Result<(Vec<u8>, Option<u64>), String> {
        let url = format!("{}/download", self.base);
        let answer = self.post(&url, json!({"file_id": file_id, "sub_format": "srt"})).await?;
        let quota = answer.status() == StatusCode::NOT_ACCEPTABLE;
        let found = read(answer).await;
        let found = match found {
            Err(e) if quota => return Err(format!("{e} (your daily OpenSubtitles downloads are used up)")),
            other => other?,
        };
        let link = found["link"].as_str().and_then(|l| url::Url::parse(l).ok()).ok_or("OpenSubtitles gave no link to the subtitles")?;
        let mut answer = crate::polite::get(self.files, &link, GAP).await?;
        if !answer.status().is_success() {
            return Err(crate::polite::refused("OpenSubtitles", answer.status()));
        }
        let too_large = || "OpenSubtitles sent a subtitle file too large to be one".to_string();
        if answer.content_length().is_some_and(|n| n > MAX_SUBTITLE as u64) {
            return Err(too_large());
        }
        // Read a block at a time, so that an endless answer stops at the most a subtitle holds.
        let mut bytes = Vec::new();
        while let Some(chunk) = answer.chunk().await.map_err(|e| format!("OpenSubtitles: {}", e.without_url()))? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_SUBTITLE {
                return Err(too_large());
            }
        }
        Ok((bytes, found["remaining"].as_i64().map(|n| n.max(0) as u64)))
    }

    async fn post(&self, url: &str, body: Value) -> Result<reqwest::Response, String> {
        let url = url::Url::parse(url).map_err(|e| e.to_string())?;
        let body = body.to_string();
        crate::polite::send(&url, GAP, || self.http.post(url.clone()).headers(self.headers.clone()).header(CONTENT_TYPE, "application/json").body(body.clone())).await
    }
}

/// An answer of OpenSubtitles' API as JSON: its `message` (or `errors`) said on a failure.
async fn read(answer: reqwest::Response) -> Result<Value, String> {
    let status = answer.status();
    let body = crate::archives::body(answer, "OpenSubtitles").await?;
    let found: Value = serde_json::from_slice(&body).unwrap_or_default();
    if status.is_success() {
        return Ok(found);
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) && found["message"].is_null() {
        return Err(format!("OpenSubtitles refused your API key (HTTP {}): check it in Settings", status.as_u16()));
    }
    let said = found["message"].as_str().map(str::to_string).or_else(|| found["errors"].as_array().map(|e| e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")));
    let reset = found["reset_time"].as_str().map(|t| format!("; it resets in {t}")).unwrap_or_default();
    Err(match said.filter(|s| !s.is_empty()) {
        Some(said) => format!("OpenSubtitles: {said}{reset}"),
        None => crate::polite::refused("OpenSubtitles", status),
    })
}

/// OpenSubtitles' hash of the file at `path`: its size plus the 64-bit little-endian words of its
/// first and last 64 KiB, wrapping. None for a file smaller than 128 KiB or unreadable.
async fn movie_hash(path: &Path) -> Option<String> {
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let size = file.metadata().await.ok()?.len();
    if size < 2 * CHUNK {
        return None;
    }
    let mut hash = size;
    let mut buf = vec![0u8; CHUNK as usize];
    for at in [0, size - CHUNK] {
        file.seek(std::io::SeekFrom::Start(at)).await.ok()?;
        file.read_exact(&mut buf).await.ok()?;
        for word in buf.chunks_exact(8) {
            hash = hash.wrapping_add(u64::from_le_bytes(word.try_into().ok()?));
        }
    }
    Some(format!("{hash:016x}"))
}

/// A title to search by, from the video's file name: without its extension, a `[id]` the media
/// downloader adds, and dots or underscores between words.
fn title_of(video: &Path) -> String {
    let stem = video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut title = String::new();
    let mut depth = 0;
    for c in stem.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = (depth - 1).max(0),
            '.' | '_' if depth == 0 => title.push(' '),
            c if depth == 0 => title.push(c),
            _ => {}
        }
    }
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `<name>.<language>.srt` beside `video`.
fn beside(video: &Path, language: &str) -> PathBuf {
    let stem = video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    video.with_file_name(format!("{stem}.{}.srt", crate::engine::sanitize_component(language)))
}

fn display_name(video: &Path) -> String {
    video.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{ok, serve};
    use std::sync::OnceLock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A client of OpenSubtitles' API, as [`find`] makes it: it follows no redirect.
    fn api() -> Client {
        Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap()
    }

    /// The API key goes to OpenSubtitles' API alone: a redirect to another host is not followed.
    /// A subtitle file larger than one can be is refused as it comes, without a length to say so.
    #[tokio::test]
    async fn the_key_stays_with_the_api_and_an_endless_file_is_cut_short() {
        let (elsewhere, seen) = serve(|_, _| ok("{}")).await;
        let (moved, _) = serve(move |_, _| (302, format!("Location: http://{elsewhere}/stolen\r\n"), Vec::new())).await;
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("clip.mkv");
        std::fs::write(&video, vec![0u8; 131_072]).unwrap();
        let subs = Subtitles { api_key: "k".into(), languages: vec!["en".into()], ..Subtitles::default() };
        assert!(find_at(&api(), &Client::new(), &format!("http://{moved}/api"), &video, &subs).await.is_err());
        assert!(seen.lock().is_empty(), "nothing reached the other host");

        // A file of 9 MiB sent with no Content-Length, then the connection closed.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let big = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = socket.read(&mut [0u8; 4096]).await;
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n").await;
                    let block = vec![b'x'; 1 << 20];
                    for _ in 0..9 {
                        if socket.write_all(&block).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let (addr, _) = serve(move |path, _| match path {
            p if p.starts_with("/api/subtitles") => ok(json!({"data": [{"attributes": {"language": "en", "moviehash_match": true, "files": [{"file_id": 7}]}}]}).to_string()),
            "/api/download" => ok(json!({"link": format!("http://{big}/big.srt"), "remaining": 5}).to_string()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let refused = find_at(&api(), &Client::new(), &format!("http://{addr}/api"), &video, &subs).await.unwrap_err();
        assert!(refused.contains("too large"), "{refused}");
        assert!(!beside(&video, "en").exists());
    }

    #[tokio::test]
    async fn hashes_titles_and_names_are_made_as_opensubtitles_and_players_want() {
        let dir = tempfile::tempdir().unwrap();
        // OpenSubtitles' own example: a file of 131072 zero bytes hashes to its size.
        let zeros = dir.path().join("zeros.bin");
        std::fs::write(&zeros, vec![0u8; 131_072]).unwrap();
        assert_eq!(movie_hash(&zeros).await.as_deref(), Some("0000000000020000"));
        let ones = dir.path().join("ones.bin");
        std::fs::write(&ones, [vec![1u8; 8], vec![0u8; 131_072 - 8]].concat()).unwrap();
        assert_eq!(movie_hash(&ones).await.as_deref(), Some(format!("{:016x}", 131_072u64 + 0x0101_0101_0101_0101).as_str()));
        std::fs::write(dir.path().join("small.mp4"), b"tiny").unwrap();
        assert_eq!(movie_hash(&dir.path().join("small.mp4")).await, None);
        assert_eq!(title_of(Path::new("The.Big.Film_2019 [abc123].mp4")), "The Big Film 2019");
        assert_eq!(beside(Path::new("/v/The Film [x].mkv"), "pt-br"), Path::new("/v/The Film [x].pt-br.srt"));
    }

    /// The hash's match in one language, the most downloaded by title in the other, each saved
    /// beside the video; the account's token sent; the quota's end stops it and is said.
    #[tokio::test]
    async fn subtitles_are_found_by_hash_then_title_and_saved_beside_the_video() {
        static BASE: OnceLock<String> = OnceLock::new();
        let (addr, hits) = serve(|path, _| {
            let base = BASE.get().unwrap();
            let json = |v: Value| ok(v.to_string());
            let entry = |lang: &str, id: u64, count: u64, matched: bool| json!({"attributes": {"language": lang, "download_count": count, "moviehash_match": matched, "files": [{"file_id": id}]}});
            match path {
                "/api/login" => json(json!({"token": "tok", "base_url": "evil.example"})),
                "/api/subtitles?languages=en%2Cfr&moviehash=0000000000020000" => {
                    json(json!({"data": [entry("en", 1, 5, false), entry("en", 2, 1, true), entry("fr", 3, 9, false)]}))
                }
                "/api/subtitles?languages=fr&query=the+film" => json(json!({"data": [entry("fr", 3, 9, false), entry("fr", 4, 20, false)]})),
                "/api/download" => json(json!({"link": format!("{base}/file/1.srt"), "remaining": 5})),
                "/file/1.srt" => ok("1\n00:00:01,000 --> 00:00:02,000\nHello\n"),
                _ => (404, String::new(), Vec::new()),
            }
        })
        .await;
        let base = format!("http://{addr}");
        BASE.set(base.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("The.Film [x1].mp4");
        std::fs::write(&video, vec![0u8; 131_072]).unwrap();
        let subs = Subtitles { api_key: "k".into(), languages: vec!["FR".into(), "en".into()], user: Some("me".into()), password: Some("pw".into()) };
        let saved = find_at(&api(), &Client::new(), &format!("{base}/api"), &video, &subs).await.unwrap();
        assert_eq!(saved, [dir.path().join("The.Film [x1].en.srt"), dir.path().join("The.Film [x1].fr.srt")]);
        assert!(std::fs::read_to_string(&saved[1]).unwrap().contains("Hello"));
        assert_eq!(hits.lock().get("/api/download"), Some(&2));

        let asked = hits.lock().len();
        let again = find_at(&api(), &Client::new(), &format!("{base}/api"), &video, &subs).await.unwrap();
        assert_eq!(again, saved, "files already there are kept");
        let seen = hits.lock();
        assert_eq!((seen.len(), seen.get("/api/download")), (asked, Some(&2)), "and not asked for again");
        drop(seen);

        let none = Subtitles { api_key: String::new(), ..subs.clone() };
        assert!(find_at(&api(), &Client::new(), &base, &video, &none).await.unwrap_err().contains("API key"));
    }

    #[tokio::test]
    async fn a_used_up_quota_says_when_it_resets() {
        let (addr, _) = serve(|path, _| match path {
            p if p.starts_with("/api/subtitles") => ok(json!({"data": [{"attributes": {"language": "en", "moviehash_match": true, "files": [{"file_id": 7}]}}]}).to_string()),
            "/api/download" => (406, String::new(), json!({"message": "You have downloaded your allowed 5 subtitles for 24h", "remaining": 0, "reset_time": "3 hours"}).to_string().into_bytes()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("clip.mkv");
        std::fs::write(&video, vec![0u8; 131_072]).unwrap();
        let subs = Subtitles { api_key: "k".into(), languages: vec!["en".into()], ..Subtitles::default() };
        let said = find_at(&api(), &Client::new(), &format!("http://{addr}/api"), &video, &subs).await.unwrap_err();
        assert!(said.contains("allowed 5 subtitles") && said.contains("resets in 3 hours") && said.contains("used up"), "{said}");
    }
}
