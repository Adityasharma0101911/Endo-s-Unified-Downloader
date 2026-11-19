//! itch.io games through its API (`api.itch.io`) with the user's own API key: a game's page, a
//! jam's entries, a collection or the user's library (`itch.io/my-purchases`) becomes the uploads
//! of each game, one folder per game. A game the user owns downloads with their download key (the
//! API's `/profile/owned-keys`); a free or pay-what-you-want one the API hands to any key; one the
//! user has not bought is an error from itch.io that says so. itch.io publishes an upload's size,
//! not a checksum. The key goes to `api.itch.io` alone; game pages and jams are read without it.
//!
//! Without a key nothing is fetched: the site's own free download is a form with a CSRF token from
//! the page, which the app does not imitate.

use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION, LOCATION};
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::ingest::{clean_path, ListOptions, Task};

const API: &str = "https://api.itch.io";
/// Between two requests to itch.io, which limits clients that go faster.
const GAP: Duration = Duration::from_millis(500);
/// Most games one listing reads (`--max-items` for more): each costs a request per upload.
const MAX_GAMES: usize = 50;
const REFUSED: &str = "itch.io refused your API key";
const NO_KEY: &str = "itch.io's API needs your own API key (from itch.io/user/settings/api-keys): set it in Settings, or with --itch-key";

/// What an itch.io link is, from its shape.
#[derive(Debug, PartialEq)]
enum Itch {
    /// `<user>.itch.io/<game>`.
    Game(Url),
    /// `itch.io/jam/<slug>[/entries]`.
    Jam(Url),
    /// `itch.io/c/<id>/<slug>`.
    Collection(u64),
    /// `itch.io/my-purchases`.
    Library,
}

fn itch_of(url: &Url) -> Option<Itch> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let page = |path: &str| Url::parse(&format!("https://{host}/{path}")).ok();
    match (host.strip_prefix("www.").unwrap_or(&host), &segs[..]) {
        ("itch.io", ["jam", slug] | ["jam", slug, "entries"]) => Some(Itch::Jam(page(&format!("jam/{slug}"))?)),
        ("itch.io", ["c", id, ..]) => Some(Itch::Collection(id.parse().ok()?)),
        ("itch.io", ["my-purchases"]) => Some(Itch::Library),
        (user, [game]) if user.strip_suffix(".itch.io").is_some_and(|u| !u.is_empty() && !u.contains('.')) => Some(Itch::Game(page(game)?)),
        _ => None,
    }
}

/// Whether `url` is an itch.io page [`list`] reads.
pub fn lists(url: &Url) -> bool {
    itch_of(url).is_some()
}

/// The uploads of the game, jam, collection or library `url` names (see the module docs);
/// `options.formats` keeps those for the platforms it names (`windows`, `linux`, `macos`), and
/// files for none.
pub async fn list(http: &Client, url: &Url, options: &ListOptions) -> Result<Vec<Task>, String> {
    let itch = itch_of(url).ok_or_else(|| format!("{url} is not an itch.io page"))?;
    let key = options.keys.itch.as_deref().map(str::trim).filter(|k| !k.is_empty()).ok_or(NO_KEY)?;
    // The download endpoint answers with a redirect to the file, which is the link wanted.
    let mut client = Client::builder().redirect(reqwest::redirect::Policy::none()).user_agent(crate::resolver::APP_USER_AGENT);
    if let Some(proxy) = &options.proxy {
        client = client.proxy(reqwest::Proxy::all(proxy).map_err(|e| format!("invalid proxy: {e}"))?);
    }
    let api = Api { http: client.build().map_err(|e| format!("cannot create HTTP client: {e}"))?, base: API.to_string(), key: key.to_string() };
    let tasks = api.list(http, itch, options).await?;
    if tasks.is_empty() {
        return Err("itch.io lists no file of it the app can download".to_string());
    }
    Ok(tasks)
}

/// A game to list: its id, title and the user's download key for it.
struct Game {
    id: u64,
    title: String,
    key: Option<u64>,
}

#[derive(Deserialize)]
struct Page {
    id: u64,
    title: String,
}

#[derive(Deserialize)]
struct Listed {
    #[serde(default)]
    game: Option<ListedGame>,
    game_id: Option<u64>,
    id: Option<u64>,
}

#[derive(Deserialize)]
struct ListedGame {
    id: u64,
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct Upload {
    id: u64,
    filename: String,
    size: Option<u64>,
    #[serde(default)]
    preorder: bool,
    #[serde(default)]
    traits: Vec<String>,
    #[serde(default)]
    platforms: serde_json::Map<String, Value>,
}

struct Api {
    /// A client that does not follow redirects, for `api.itch.io` alone.
    http: Client,
    base: String,
    key: String,
}

impl Api {
    async fn list(&self, http: &Client, itch: Itch, options: &ListOptions) -> Result<Vec<Task>, String> {
        let cap = options.max_items.unwrap_or(MAX_GAMES);
        let games = match itch {
            Itch::Game(page) => {
                let data = Url::parse(&format!("{}/data.json", page.as_str().trim_end_matches('/'))).map_err(|e| e.to_string())?;
                let game: Page = public(http, &data).await?.ok_or_else(|| format!("itch.io has no game at {page}"))?;
                let owned: Value = self.call(&format!("profile/owned-keys?game_ids={}", game.id)).await?;
                vec![Game { id: game.id, title: game.title, key: owned["owned_keys"][0]["id"].as_u64() }]
            }
            Itch::Jam(page) => {
                let html = crate::archives::body(crate::polite::get(http, &page, GAP).await?, "itch.io").await?;
                let id = jam_id(&String::from_utf8_lossy(&html)).ok_or_else(|| format!("{page} names no jam the app can read"))?;
                let entries = page.join(&format!("/jam/{id}/entries.json")).map_err(|e| e.to_string())?;
                let found: Value = public(http, &entries).await?.ok_or_else(|| format!("itch.io lists no entries of {page}"))?;
                let listed: Vec<Listed> = serde_json::from_value(found["jam_games"].clone()).unwrap_or_default();
                games(listed, false, cap, options)
            }
            Itch::Collection(id) => games(self.pages(&format!("collections/{id}/collection-games"), "collection_games", cap).await?, false, cap, options),
            Itch::Library => games(self.pages("profile/owned-keys", "owned_keys", cap).await?, true, cap, options),
        };
        let single = games.len() == 1;
        let picks: Vec<&str> = options.formats.iter().filter_map(|f| platform(f)).collect();
        let mut tasks = Vec::new();
        for game in games {
            let credentials = game.key.map(|k| format!("download_key_id={k}")).unwrap_or_default();
            // A game of several that cannot be had (one not bought) is said, and the others listed.
            let uploads = match self.call::<Value>(&format!("games/{}/uploads?{credentials}", game.id)).await {
                Ok(found) => serde_json::from_value::<Vec<Upload>>(found["uploads"].clone()).map_err(|_| "itch.io answered with uploads the app cannot read".to_string())?,
                Err(e) if single || e.starts_with(REFUSED) => return Err(e),
                Err(e) => {
                    options.fail(format!("{}: {e}", game.title));
                    continue;
                }
            };
            let folder = clean_path([game.title.trim()]).ok().or_else(|| clean_path([game.id.to_string().as_str()]).ok());
            for upload in uploads.into_iter().filter(|u| !u.preorder) {
                let made: Vec<String> = upload.traits.iter().filter_map(|t| t.strip_prefix("p_")).map(str::to_string).chain(upload.platforms.keys().cloned()).collect();
                if !picks.is_empty() && !made.is_empty() && !made.iter().any(|p| picks.contains(&p.as_str())) {
                    continue;
                }
                let link = self.download(upload.id, &credentials).await?;
                let name = clean_path([upload.filename.trim()]).or_else(|_| clean_path([upload.id.to_string().as_str()]))?;
                tasks.push(Task { urls: vec![link], name: Some(name), folder: folder.clone(), size: upload.size, from_document: true, ..Task::default() });
            }
        }
        Ok(tasks)
    }

    /// The entries under `field` of the pages of `path` (a collection's games, the user's owned
    /// keys), until one comes back empty, as itch.io asks, or past `cap`.
    async fn pages(&self, path: &str, field: &str, cap: usize) -> Result<Vec<Listed>, String> {
        let mut listed = Vec::new();
        for page in 1.. {
            let found: Value = self.call(&format!("{path}?page={page}&per_page=100")).await?;
            let more: Vec<Listed> = serde_json::from_value(found[field].clone()).unwrap_or_default();
            if more.is_empty() {
                break;
            }
            listed.extend(more);
            if listed.len() > cap {
                break;
            }
        }
        Ok(listed)
    }

    /// The JSON answer of `api.itch.io/<path>`, sent the key; its `errors` said as itch.io says them.
    async fn call<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let url = Url::parse(&format!("{}/{path}", self.base)).map_err(|e| e.to_string())?;
        let answer = crate::polite::send(&url, GAP, || self.http.get(url.clone()).header(AUTHORIZATION, self.bearer())).await?;
        read(answer).await
    }

    /// Where `api.itch.io` sends the download of `upload`: a link of itch.io's storage that
    /// expires after a while, as the site's own do.
    async fn download(&self, upload: u64, credentials: &str) -> Result<Url, String> {
        let url = Url::parse(&format!("{}/uploads/{upload}/download?{credentials}", self.base)).map_err(|e| e.to_string())?;
        let answer = crate::polite::send(&url, GAP, || self.http.get(url.clone()).header(AUTHORIZATION, self.bearer())).await?;
        if answer.status().is_redirection() {
            let to = answer.headers().get(LOCATION).and_then(|l| l.to_str().ok()).and_then(|l| url.join(l).ok());
            return to.ok_or_else(|| "itch.io sent the download nowhere".to_string());
        }
        let found: Value = read(answer).await?;
        found["url"].as_str().and_then(|u| Url::parse(u).ok()).ok_or_else(|| "itch.io gave no link to the upload".to_string())
    }

    fn bearer(&self) -> HeaderValue {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", self.key)).unwrap_or(HeaderValue::from_static(""));
        value.set_sensitive(true);
        value
    }
}

/// An answer of itch.io's API read as JSON: a refused key, its `errors` or another failure as
/// an error.
async fn read<T: DeserializeOwned>(answer: reqwest::Response) -> Result<T, String> {
    let status = answer.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(format!("{REFUSED} (HTTP {}): check it in Settings", status.as_u16()));
    }
    let body = crate::archives::body(answer, "itch.io").await?;
    let found: Value = serde_json::from_slice(&body).unwrap_or_default();
    if let Some(errors) = found["errors"].as_array() {
        let said: Vec<&str> = errors.iter().filter_map(Value::as_str).collect();
        return Err(format!("itch.io: {}", said.join("; ")));
    }
    if !status.is_success() {
        return Err(crate::polite::refused("itch.io", status));
    }
    serde_json::from_value(found).map_err(|_| "itch.io answered with something the app cannot read".to_string())
}

/// A public JSON page of itch.io's (a game's `data.json`, a jam's entries), read without the key;
/// None when there is none.
async fn public<T: DeserializeOwned>(http: &Client, url: &Url) -> Result<Option<T>, String> {
    let answer = crate::polite::get(http, url, GAP).await?;
    if answer.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !answer.status().is_success() {
        return Err(crate::polite::refused("itch.io", answer.status()));
    }
    let body = crate::archives::body(answer, "itch.io").await?;
    serde_json::from_slice(&body).map(Some).map_err(|_| "itch.io answered with something the app cannot read".to_string())
}

/// The games of `listed` (a jam's or collection's entries, or the user's keys when `owned`), each
/// once, up to `cap`; the cut said.
fn games(listed: Vec<Listed>, owned: bool, cap: usize, options: &ListOptions) -> Vec<Game> {
    let mut seen = std::collections::HashSet::new();
    let all: Vec<Game> = listed
        .into_iter()
        .filter_map(|l| {
            let id = l.game.as_ref().map(|g| g.id).or(l.game_id)?;
            let title = l.game.map(|g| g.title).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| id.to_string());
            seen.insert(id).then_some(Game { id, title, key: l.id.filter(|_| owned) })
        })
        .collect();
    if all.len() > cap {
        options.note(format!("itch.io: the first {cap} games are listed (--max-items for more)"));
    }
    all.into_iter().take(cap).collect()
}

/// The platform an upload is for, as itch.io names it, that `pick` names.
fn platform(pick: &str) -> Option<&'static str> {
    match pick.trim().to_ascii_lowercase().as_str() {
        "windows" | "win" => Some("windows"),
        "linux" => Some("linux"),
        "macos" | "mac" | "osx" => Some("osx"),
        _ => None,
    }
}

/// A jam's id, which its entries' JSON is named by, from its page's script:
/// `new I.ViewJam('#view_jam_…', {…,"id":379683,…})`.
fn jam_id(html: &str) -> Option<u64> {
    let script = &html[html.find("I.ViewJam(")?..];
    let at = script.find("\"id\":")? + "\"id\":".len();
    script[at..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{ok, serve};
    use std::sync::OnceLock;

    #[test]
    fn itch_links_are_told_by_their_shape() {
        let of = |s: &str| itch_of(&Url::parse(s).unwrap());
        assert_eq!(of("https://leafo.itch.io/x-moon"), Some(Itch::Game(Url::parse("https://leafo.itch.io/x-moon").unwrap())));
        assert_eq!(of("https://itch.io/jam/gmtk-2024/entries"), Some(Itch::Jam(Url::parse("https://itch.io/jam/gmtk-2024").unwrap())));
        assert_eq!(of("https://itch.io/c/1234/best-games"), Some(Itch::Collection(1234)));
        assert_eq!(of("https://itch.io/my-purchases"), Some(Itch::Library));
        for not in ["https://itch.io/games", "https://leafo.itch.io/", "https://leafo.itch.io/x-moon/devlog/1/post", "https://img.example.itch.io/a", "https://itch.io/c/x/y"] {
            assert_eq!(of(not), None, "{not}");
        }
        let page = r#"<script type="text/javascript">new I.ViewJam('#view_jam_9688340', {"voting_end_date":"2024-08-26 17:00:00","id":379683,"end_date":"x"});</script>"#;
        assert_eq!(jam_id(page), Some(379683));
        assert_eq!(jam_id(r#"<div data-id="42">"#), None);
        assert_eq!(platform("macOS"), Some("osx"));
    }

    /// A game page's uploads through the API with the user's key and download key, the preorder
    /// and other platforms' left out; a jam's and a library's games one folder each, capped; a
    /// refused key and itch.io's errors said.
    #[tokio::test]
    async fn games_jams_and_libraries_list_their_uploads() {
        static BASE: OnceLock<String> = OnceLock::new();
        let (addr, hits) = serve(|path, _| {
            let base = BASE.get().unwrap();
            let json = |v: Value| ok(v.to_string());
            match path {
                "/x-moon/data.json" => json(serde_json::json!({"id": 3, "title": "X-Moon"})),
                "/paid/data.json" => json(serde_json::json!({"id": 4, "title": "Odd/Name"})),
                "/jam/gmtk" => ok(r#"<script>new I.ViewJam('#view_jam_1', {"start_date":"2024-08-16","id":77})</script>"#),
                "/jam/77/entries.json" => json(serde_json::json!({"jam_games": [{"game": {"id": 3, "title": "X-Moon"}}, {"game": {"id": 4, "title": "Odd/Name"}}, {"game": {"id": 5, "title": "Third"}}]})),
                "/private/profile/owned-keys?game_ids=3" | "/private/profile/owned-keys?page=1&per_page=100" => {
                    json(serde_json::json!({"owned_keys": [{"id": 90, "game_id": 3, "game": {"id": 3, "title": "X-Moon"}}]}))
                }
                "/private/profile/owned-keys?game_ids=4" | "/private/profile/owned-keys?page=2&per_page=100" => json(serde_json::json!({"owned_keys": []})),
                p if p.starts_with("/private/games/3/uploads?") => json(serde_json::json!({"uploads": [
                    {"id": 11, "filename": "x-moon-win.zip", "size": 1000, "traits": ["p_windows"]},
                    {"id": 12, "filename": "x-moon-mac.zip", "size": 2000, "platforms": {"osx": "all"}},
                    {"id": 13, "filename": "soundtrack.zip", "size": 300},
                    {"id": 14, "filename": "next.zip", "preorder": true},
                ]})),
                "/private/games/4/uploads?" => json(serde_json::json!({"errors": ["you do not own this game"]})),
                p if p.starts_with("/private/uploads/") => {
                    let id = p.trim_start_matches("/private/uploads/").split('/').next().unwrap();
                    (302, format!("Location: {base}/files/{id}?token=signed\r\n"), Vec::new())
                }
                _ => (404, String::new(), Vec::new()),
            }
        })
        .await;
        let base = format!("http://{addr}");
        BASE.set(base.clone()).unwrap();
        let api = |key: &str| Api { http: Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap(), base: format!("{base}/private"), key: key.to_string() };
        let http = Client::new();
        let windows = ListOptions { formats: vec!["windows".into()], ..ListOptions::default() };

        let game = Itch::Game(Url::parse(&format!("{base}/x-moon")).unwrap());
        let tasks = api("secret").list(&http, game, &windows).await.unwrap();
        let got: Vec<_> = tasks.iter().map(|t| (t.urls[0].as_str().to_string(), t.name.clone().unwrap(), t.size)).collect();
        assert_eq!(got, [
            (format!("{base}/files/11?token=signed"), "x-moon-win.zip".into(), Some(1000)),
            (format!("{base}/files/13?token=signed"), "soundtrack.zip".into(), Some(300)),
        ]);
        assert!(tasks.iter().all(|t| t.folder.as_deref() == Some(std::path::Path::new("X-Moon")) && t.from_document));
        assert_eq!(hits.lock().get("/private/uploads/11/download?download_key_id=90"), Some(&1), "the download key goes with the download");

        let paid = api("secret").list(&http, Itch::Game(Url::parse(&format!("{base}/paid")).unwrap()), &windows).await.unwrap_err();
        assert_eq!(paid, "itch.io: you do not own this game");

        let (tx, notes) = std::sync::mpsc::channel();
        let capped = ListOptions { max_items: Some(2), notes: Some(tx), ..ListOptions::default() };
        let jam = Itch::Jam(Url::parse(&format!("{base}/jam/gmtk")).unwrap());
        let tasks = api("secret").list(&http, jam, &capped).await.unwrap();
        assert_eq!(tasks.len(), 3, "X-Moon's uploads, of every platform");
        let said: Vec<_> = notes.try_iter().map(|n| (n.text, n.failed)).collect();
        assert!(said.contains(&("Odd/Name: itch.io: you do not own this game".to_string(), true)), "{said:?}");
        assert!(said.iter().any(|(text, _)| text.contains("first 2 games")), "{said:?}");
        assert!(!hits.lock().keys().any(|p| p.starts_with("/private/games/5/")), "games past the cap are not asked for");

        let library = api("secret").list(&http, Itch::Library, &ListOptions::default()).await.unwrap();
        assert_eq!(library.len(), 3);
        assert_eq!(hits.lock().get("/private/uploads/13/download?download_key_id=90"), Some(&2), "the game page's and the library's");
        let refused = api("wrong").list(&http, Itch::Library, &ListOptions::default()).await.unwrap_err();
        assert!(refused.contains("refused your API key"), "{refused}");
    }
}
