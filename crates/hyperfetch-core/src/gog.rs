//! The user's GOG library, by the app's own code, no helper: GOG's sign-in (the OAuth flow of
//! GOG's own Galaxy client, whose public client id the sign-in page is opened with; the user signs
//! in on GOG's page), the games the account owns, and each game's offline installers, patches,
//! extras and DLC installers, which the engine downloads from the account's own links on GOG
//! (`gog://file/...`, sent with the account's token; they redirect to signed links on GOG's CDN,
//! which get no token). The sign-in is kept in `token.json` in the app's folder (see
//! `helpers::account_dir`), never the password, which only GOG's page sees.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use url::Url;

use crate::accounts::Item;
use crate::helpers::{self, Proxy};
use crate::ingest::Task;

/// GOG Galaxy's public client, whose sign-in the user completes on GOG's page.
const CLIENT_ID: &str = "46899977096215655";
const CLIENT_SECRET: &str = "9d85c43b1482497dbbce61f6e4aa173a433796eeae2ca8c5f6129f2dc4de46d9";
const REDIRECT: &str = "https://embed.gog.com/on_login_success?origin=client";
const AUTH: &str = "https://auth.gog.com";
const EMBED: &str = "https://embed.gog.com";
/// Most pages of games read of a library (100 games a page).
const MAX_PAGES: u32 = 100;

/// GOG's sign-in page, which ends on an almost blank page whose address holds the code.
pub fn sign_in_page() -> String {
    let redirect: String = url::form_urlencoded::byte_serialize(REDIRECT.as_bytes()).collect();
    format!("{AUTH}/auth?client_id={CLIENT_ID}&redirect_uri={redirect}&response_type=code&layout=client2")
}

/// The sign-in, as kept.
#[derive(serde::Serialize, serde::Deserialize)]
struct Token {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_in: u64,
    /// When `access_token` expires, in seconds since 1970.
    #[serde(default)]
    expires_at: u64,
    /// The account's name, shown as who is signed in.
    #[serde(default)]
    username: Option<String>,
}

fn token_file() -> PathBuf {
    helpers::account_dir("gog").join("token.json")
}

fn load() -> Option<Token> {
    serde_json::from_slice(&std::fs::read(token_file()).ok()?).ok()
}

fn save(token: &Token) -> Result<(), String> {
    helpers::own_dir("gog")?;
    helpers::write_private(&token_file(), &serde_json::to_vec(token).map_err(|e| e.to_string())?)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Who is signed in to GOG, if anyone. Blocking (a small file).
pub fn account() -> Option<String> {
    load().map(|token| token.username.unwrap_or_else(|| "your GOG account".to_string()))
}

/// Forgets the sign-in.
pub fn sign_out() -> Result<(), String> {
    match std::fs::remove_file(token_file()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Cannot delete {}: {e}", token_file().display())),
        _ => Ok(()),
    }
}

/// The code in what the user pasted: the address GOG's sign-in ended on, or the code itself.
fn code_of(answer: &str) -> Option<String> {
    let answer = answer.trim();
    let code = match Url::parse(answer) {
        Ok(url) => url.query_pairs().find(|(k, _)| k == "code")?.1.into_owned(),
        Err(_) => answer.to_string(),
    };
    (!code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))).then_some(code)
}

/// Asks GOG's sign-in at `auth` for a token with `grant`: the code or the refresh token, and the
/// client's secret, go in the body of a POST, never in an address a log may keep.
async fn token_request(http: &reqwest::Client, auth: &str, grant: &[(&str, &str)]) -> Result<Token, String> {
    let form = url::form_urlencoded::Serializer::new(String::new()).extend_pairs([("client_id", CLIENT_ID), ("client_secret", CLIENT_SECRET)]).extend_pairs(grant).finish();
    let request = http.post(format!("{auth}/token")).header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(form);
    let resp = request.send().await.map_err(|e| format!("GOG's sign-in did not answer: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!("GOG refused the sign-in ({}): sign in to GOG again", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("GOG's sign-in did not answer: {}", e.without_url()))?;
    let mut token: Token = serde_json::from_slice(&bytes).map_err(|e| format!("Unreadable answer from GOG's sign-in: {e}"))?;
    token.expires_at = now() + token.expires_in;
    Ok(token)
}

/// What GOG answers at `url` for the account of `access`.
async fn get<T: serde::de::DeserializeOwned>(http: &reqwest::Client, url: &str, access: &str) -> Result<T, String> {
    let resp = http.get(url).bearer_auth(access).send().await.map_err(|e| format!("GOG did not answer: {}", e.without_url()))?;
    match resp.status() {
        reqwest::StatusCode::UNAUTHORIZED => return Err("GOG no longer takes the sign-in: sign in to GOG again".to_string()),
        status if !status.is_success() => return Err(format!("GOG answered {status} for {url}")),
        _ => {}
    }
    let bytes = resp.bytes().await.map_err(|e| format!("GOG did not answer: {}", e.without_url()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Unreadable answer from GOG for {url}: {e}"))
}

/// Signs in with what the user pasted from GOG's sign-in page (see [`sign_in_page`]). Returns the
/// account's name.
pub async fn sign_in(answer: &str, proxy: Proxy<'_>) -> Result<String, String> {
    let code = code_of(answer).ok_or("Paste the address GOG's sign-in page ended on (it holds code=...)")?;
    signed_in(&proxy.client(TIMEOUT)?, AUTH, EMBED, &code).await
}

async fn signed_in(http: &reqwest::Client, auth: &str, embed: &str, code: &str) -> Result<String, String> {
    let mut token = token_request(http, auth, &[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", REDIRECT)]).await?;
    let user: Result<serde_json::Value, String> = get(http, &format!("{embed}/userData.json"), &token.access_token).await;
    token.username = user.ok().and_then(|u| u.get("username")?.as_str().map(str::to_string));
    save(&token)?;
    Ok(token.username.unwrap_or_else(|| "your GOG account".to_string()))
}

/// One refresh at a time: GOG may take each refresh token once.
static REFRESHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A token of the signed-in account that is good for a while yet, refreshed now if need be.
async fn access(http: &reqwest::Client, auth: &str) -> Result<String, String> {
    let _held = REFRESHING.lock().await;
    let token = load().ok_or("Sign in to GOG first (Settings, Accounts & tools; or: accounts sign-in gog)")?;
    if token.expires_at > now() + 60 {
        return Ok(token.access_token);
    }
    let mut fresh = token_request(http, auth, &[("grant_type", "refresh_token"), ("refresh_token", token.refresh_token.as_str())]).await?;
    fresh.username = token.username;
    save(&fresh)?;
    Ok(fresh.access_token)
}

#[derive(serde::Deserialize)]
struct Page {
    #[serde(default, rename = "totalPages")]
    total_pages: u32,
    #[serde(default)]
    products: Vec<Product>,
}

#[derive(serde::Deserialize)]
struct Product {
    id: u64,
    #[serde(default)]
    title: String,
}

/// The games of the signed-in account.
pub async fn library(proxy: Proxy<'_>) -> Result<Vec<Item>, String> {
    games(&proxy.client(TIMEOUT)?, AUTH, EMBED).await
}

/// Longest a request to GOG may take.
const TIMEOUT: Duration = Duration::from_secs(60);

async fn games(http: &reqwest::Client, auth: &str, embed: &str) -> Result<Vec<Item>, String> {
    let access = access(http, auth).await?;
    let mut items = Vec::new();
    for page in 1..=MAX_PAGES {
        let got: Page = get(http, &format!("{embed}/account/getFilteredProducts?mediaType=1&page={page}"), &access).await?;
        let link = |p: &Product| format!("gog://game/{}", p.id);
        items.extend(got.products.iter().map(|p| Item { link: link(p), title: p.title.clone(), size: None, installed: None, kind: Some("game".to_string()) }));
        if page >= got.total_pages {
            break;
        }
    }
    Ok(items)
}

/// A game's (or a DLC's) files, as `account/gameDetails` gives them.
#[derive(serde::Deserialize)]
struct Details {
    #[serde(default)]
    title: String,
    /// Installers and patches by language, then by system (`windows`, `mac`, `linux`).
    #[serde(default)]
    downloads: Vec<(String, HashMap<String, Vec<File>>)>,
    #[serde(default)]
    extras: Vec<File>,
    #[serde(default)]
    dlcs: Vec<Details>,
}

#[derive(serde::Deserialize)]
struct File {
    #[serde(default, rename = "manualUrl")]
    manual_url: String,
    #[serde(default)]
    name: String,
}

/// Which files of a game a `gog://game/<id>` link takes, from its query: `os=windows,mac,linux`
/// or `all` (this system's by default), `lang=<language as GOG names it>` or `all` (English, else
/// the first), `extras=1`, `patches=1`, `dlc=0`.
struct Choice {
    os: Vec<String>,
    lang: Option<String>,
    extras: bool,
    patches: bool,
    dlc: bool,
}

fn choice(url: &Url) -> Choice {
    let query: HashMap<String, String> = url.query_pairs().map(|(k, v)| (k.to_ascii_lowercase(), v.into_owned())).collect();
    let flag = |name: &str, default: bool| query.get(name).map_or(default, |v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
    let this_os = match std::env::consts::OS {
        "macos" => "mac",
        "linux" => "linux",
        _ => "windows",
    };
    let os = match query.get("os").map(|v| v.to_ascii_lowercase()) {
        Some(all) if all == "all" => vec!["windows".to_string(), "mac".to_string(), "linux".to_string()],
        Some(list) => list.split(',').map(|s| if s.trim() == "macos" { "mac".to_string() } else { s.trim().to_string() }).collect(),
        None => vec![this_os.to_string()],
    };
    Choice { os, lang: query.get("lang").cloned(), extras: flag("extras", false), patches: flag("patches", false), dlc: flag("dlc", true) }
}

/// The downloads `details` holds that `choice` takes, each in the folder of its game's (or DLC's)
/// title.
fn tasks_of(details: &Details, choice: &Choice, into: &mut Vec<Task>) {
    let folder = crate::ingest::clean_path([details.title.as_str()]).ok();
    let mut push = |file: &File| {
        let Some(url) = Some(&file.manual_url).filter(|u| u.starts_with('/')).and_then(|u| Url::parse(&format!("gog://file{u}")).ok()) else { return };
        if !into.iter().any(|t| t.urls[0] == url) {
            into.push(Task { urls: vec![url], folder: folder.clone(), ..Task::default() });
        }
    };
    let languages: Vec<&HashMap<String, Vec<File>>> = match choice.lang.as_deref() {
        Some(all) if all.eq_ignore_ascii_case("all") => details.downloads.iter().map(|(_, files)| files).collect(),
        Some(want) => details.downloads.iter().filter(|(name, _)| name.eq_ignore_ascii_case(want)).map(|(_, files)| files).collect(),
        None => details.downloads.iter().find(|(name, _)| name.eq_ignore_ascii_case("English")).or(details.downloads.first()).map(|(_, files)| files).into_iter().collect(),
    };
    for files in languages {
        for file in choice.os.iter().filter_map(|os| files.get(os)).flatten() {
            if choice.patches || !file.name.to_ascii_lowercase().contains("patch") {
                push(file);
            }
        }
    }
    if choice.extras {
        details.extras.iter().for_each(&mut push);
    }
    if choice.dlc {
        for dlc in &details.dlcs {
            tasks_of(dlc, choice, into);
        }
    }
}

/// The downloads of a `gog://` link: a game's files (see [`Choice`]), or the one file of a
/// `gog://file/...` link.
pub async fn list(url: &Url, proxy: Proxy<'_>) -> Result<Vec<Task>, String> {
    match url.host_str() {
        Some("file") => Ok(vec![Task { urls: vec![url.clone()], ..Task::default() }]),
        Some("game") => game_tasks(&proxy.client(TIMEOUT)?, AUTH, EMBED, url).await,
        _ => Err(format!("{url} names no GOG game")),
    }
}

async fn game_tasks(http: &reqwest::Client, auth: &str, embed: &str, url: &Url) -> Result<Vec<Task>, String> {
    let id: u64 = url.path().trim_matches('/').parse().map_err(|_| format!("{url} names no GOG game"))?;
    let access = access(http, auth).await?;
    let details: Details = get(http, &format!("{embed}/account/gameDetails/{id}.json"), &access).await?;
    let mut tasks = Vec::new();
    tasks_of(&details, &choice(url), &mut tasks);
    if tasks.is_empty() {
        return Err(format!("GOG lists no file of {} for this choice of system and language", details.title));
    }
    Ok(tasks)
}

/// Where the engine downloads a `gog://file/...` link from, and the Authorization header it sends
/// there (to that host alone): only a file of the account's downloads (`/downloads/...`), never
/// another of GOG's pages for the account.
pub(crate) async fn route(link: &Url, proxy: Proxy<'_>) -> Result<(Url, String), String> {
    route_at(&proxy.client(TIMEOUT)?, AUTH, EMBED, link).await
}

async fn route_at(http: &reqwest::Client, auth: &str, embed: &str, link: &Url) -> Result<(Url, String), String> {
    let refused = || format!("{link} names no GOG file");
    if link.host_str() != Some("file") {
        return Err(refused());
    }
    let query = link.query().map(|q| format!("?{q}")).unwrap_or_default();
    let url = Url::parse(&format!("{embed}{}{query}", link.path())).map_err(|e| format!("{link}: {e}"))?;
    // Checked once the address is put together, `..` and the like resolved.
    let downloads = format!("{}/downloads/", Url::parse(embed).map_err(|e| e.to_string())?.path().trim_end_matches('/'));
    if !url.path().starts_with(&downloads) {
        return Err(refused());
    }
    Ok((url, format!("Bearer {}", access(http, auth).await?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for GOG: the code becomes a token (its paths under `/private/` take only the
    /// token `secret`), the library lists a game, and the game's details become downloads.
    #[tokio::test]
    async fn a_sign_in_lists_the_games_and_their_files() {
        let (addr, _) = crate::hls::tests::serve(|path, _| {
            let json = |s: &str| (200, String::new(), s.as_bytes().to_vec());
            match path {
                "/private/userData.json" => json(r#"{"username":"tester"}"#),
                "/private/account/getFilteredProducts?mediaType=1&page=1" => json(r#"{"totalPages":1,"products":[{"id":1207,"title":"Old Game"}]}"#),
                "/private/account/gameDetails/1207.json" => json(
                    r#"{"title":"Old Game","downloads":[["English",{"windows":[{"manualUrl":"/downloads/old_game/en1installer0","name":"Old Game","size":"1 GB"},{"manualUrl":"/downloads/old_game/en1patch1","name":"Patch (1.0 to 1.1)"}],"mac":[{"manualUrl":"/downloads/old_game/en2installer0","name":"Old Game"}],"linux":[]}],["Deutsch",{"windows":[{"manualUrl":"/downloads/old_game/de1installer0","name":"Old Game"}]}]],
                    "extras":[{"manualUrl":"/downloads/old_game/5501","name":"manual","type":"manuals","size":"2 MB"}],
                    "dlcs":[{"title":"Old Game: More","downloads":[["English",{"windows":[{"manualUrl":"/downloads/old_game_more/en1installer0","name":"More"}]}]],"extras":[]}]}"#,
                ),
                _ => (404, String::new(), Vec::new()),
            }
        })
        .await;
        let ((auth, tokens), embed) = (token_server().await, format!("http://{addr}/private"));
        let http = reqwest::Client::new();
        assert_eq!(code_of("https://embed.gog.com/on_login_success?origin=client&code=abc").as_deref(), Some("abc"));
        assert_eq!(code_of(" abc ").as_deref(), Some("abc"));
        assert!(code_of("not a code!").is_none());
        assert!(signed_in(&http, &auth, &embed, "wrong").await.unwrap_err().contains("refused"));
        assert_eq!(signed_in(&http, &auth, &embed, "abc").await.unwrap(), "tester");
        assert_eq!(account().as_deref(), Some("tester"));
        let games = games(&http, &auth, &embed).await.unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!((games[0].link.as_str(), games[0].title.as_str()), ("gog://game/1207", "Old Game"));

        let urls = |tasks: &[Task]| tasks.iter().map(|t| t.urls[0].as_str().to_string()).collect::<Vec<_>>();
        let link = Url::parse("gog://game/1207?os=windows").unwrap();
        let tasks = game_tasks(&http, &auth, &embed, &link).await.unwrap();
        assert_eq!(urls(&tasks), ["gog://file/downloads/old_game/en1installer0", "gog://file/downloads/old_game_more/en1installer0"]);
        assert_eq!(tasks[0].folder.as_deref(), Some(std::path::Path::new("Old Game")));
        let link = Url::parse("gog://game/1207?os=all&lang=deutsch&extras=1&patches=1&dlc=0").unwrap();
        let tasks = game_tasks(&http, &auth, &embed, &link).await.unwrap();
        assert_eq!(urls(&tasks), ["gog://file/downloads/old_game/de1installer0", "gog://file/downloads/old_game/5501"]);
        let link = Url::parse("gog://game/1207?os=windows&patches=yes&dlc=0").unwrap();
        assert_eq!(game_tasks(&http, &auth, &embed, &link).await.unwrap().len(), 2, "the installer and its patch");

        let (url, header) = route_at(&http, &auth, &embed, &tasks[0].urls[0]).await.unwrap();
        assert_eq!(url.as_str(), format!("{embed}/downloads/old_game/de1installer0"));
        assert_eq!(header, "Bearer secret");
        assert!(tokens.lock().iter().all(|(_, body)| !body.contains("refresh_token")), "a fresh token is not refreshed");
        let asked = tokens.lock().clone();
        assert!(asked.len() == 2 && asked.iter().all(|(line, body)| line == "POST /token HTTP/1.1" && body.contains("client_secret=")), "{asked:?}");
        // Only the account's downloads get its token: not its other pages, however the path is put.
        for link in ["gog://file/account/userData.json", "gog://file/downloads/../userData.json", "gog://file/downloads/%2e%2e/userData.json"] {
            assert!(route_at(&http, &auth, &embed, &Url::parse(link).unwrap()).await.is_err(), "{link}");
        }
        sign_out().unwrap();
        assert!(account().is_none());
        assert!(game_tasks(&http, &auth, &embed, &link).await.unwrap_err().contains("Sign in to GOG first"));
        assert!(sign_in_page().starts_with("https://auth.gog.com/auth?client_id=46899977096215655&redirect_uri=https%3A%2F%2Fembed.gog.com"));
    }

    /// A stand-in for GOG's token endpoint: a POST whose form body holds the code `abc` (or the
    /// refresh token `r1`) gets a token, anything else a refusal. It keeps each request's first
    /// line and body.
    async fn token_server() -> (String, std::sync::Arc<parking_lot::Mutex<Vec<(String, String)>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let kept = std::sync::Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let kept = std::sync::Arc::clone(&kept);
                tokio::spawn(async move {
                    let (mut req, mut buf) = (Vec::new(), [0u8; 4096]);
                    let complete = |req: &[u8]| {
                        let end = req.windows(4).position(|w| w == b"\r\n\r\n")?;
                        let head = String::from_utf8_lossy(&req[..end]).to_string();
                        let length = head.lines().find_map(|l| l.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse::<usize>().ok()));
                        (req.len() >= end + 4 + length.unwrap_or(0)).then(|| (head, String::from_utf8_lossy(&req[end + 4..]).to_string()))
                    };
                    let (head, body) = loop {
                        if let Some(done) = complete(&req) {
                            break done;
                        }
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    };
                    let line = head.lines().next().unwrap_or_default().to_string();
                    let ok = line == "POST /token HTTP/1.1" && (body.contains("code=abc") || body.contains("refresh_token=r1"));
                    kept.lock().push((line, body));
                    let (status, json) = match ok {
                        true => (200, r#"{"access_token":"secret","refresh_token":"r1","expires_in":3600,"token_type":"bearer"}"#),
                        false => (400, r#"{"error":"invalid_grant"}"#),
                    };
                    let _ = socket.write_all(format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}", json.len()).as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }
}
