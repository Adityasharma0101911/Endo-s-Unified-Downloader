//! The user's own accounts and the helper tools that reach them, in one shape for every front
//! end: what a source needs installed and who is signed in ([`status`]), how to sign in — the
//! service's own flow, as steps to show ([`steps`], [`sign_in`]) — what the account can download
//! ([`library`]), and links (`rclone://`, `epic://`, `gog://`, `steam://`) that download like any
//! other: listed by [`list`] when they are added, run by the engine (see [`route`]). The app never
//! sees a password: each tool keeps its sign-in where it keeps it, in a folder of the app's own
//! (see `helpers::account_dir`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{DownloadOptions, EngineSnapshot};
use crate::helpers::{Tool, ToolState};
use crate::ingest::{ListOptions, Task};
use crate::{epic, gog, helpers, livestream, rclone, steam};

/// A source the app reaches through an account of the user's, or a helper tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Service {
    /// Cloud drives, through rclone.
    Cloud,
    /// The user's Epic Games library, through legendary.
    Epic,
    /// The user's GOG library (the app's own code).
    Gog,
    /// Steam depots of the user's games and free ones, through DepotDownloader.
    Steam,
    /// Live streams yt-dlp cannot follow, through streamlink (no account).
    Streams,
}

impl Service {
    pub const ALL: [Service; 5] = [Service::Cloud, Service::Epic, Service::Gog, Service::Steam, Service::Streams];

    /// Its name on the command line (`cloud`, `epic`, `gog`, `steam`, `streams`).
    pub fn id(self) -> &'static str {
        match self {
            Service::Cloud => "cloud",
            Service::Epic => "epic",
            Service::Gog => "gog",
            Service::Steam => "steam",
            Service::Streams => "streams",
        }
    }

    /// What a front end calls it.
    pub fn name(self) -> &'static str {
        match self {
            Service::Cloud => "Cloud drives",
            Service::Epic => "Epic Games",
            Service::Gog => "GOG",
            Service::Steam => "Steam",
            Service::Streams => "Live streams",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.id().eq_ignore_ascii_case(text.trim()))
    }

    /// The helper it runs; None for one done by the app's own code.
    pub fn tool(self) -> Option<&'static Tool> {
        match self {
            Service::Cloud => Some(&rclone::RCLONE),
            Service::Epic => Some(&epic::LEGENDARY),
            Service::Gog => None,
            Service::Steam => Some(&steam::DEPOT_DOWNLOADER),
            Service::Streams => Some(&livestream::STREAMLINK),
        }
    }
}

/// Where a source stands on this computer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Status {
    pub service: Service,
    /// Its helper's state; None for one done by the app's own code.
    pub tool: Option<ToolState>,
    /// Who is signed in, by display name only (for cloud drives, the drives set up).
    pub account: Option<String>,
    /// What else to know about it here.
    pub note: Option<String>,
}

/// Where `service` stands (see [`Status`]). Nothing is installed and nothing goes online: who is
/// signed in is read from the sign-in files in the app's folder.
pub async fn status(service: Service) -> Status {
    let tool = match service.tool() {
        Some(tool) => Some(helpers::state(tool).await),
        None => None,
    };
    let account = tokio::task::spawn_blocking(move || match service {
        Service::Cloud => Some(rclone::remotes().join(", ")).filter(|r| !r.is_empty()),
        Service::Epic => epic::account(),
        Service::Gog => gog::account(),
        Service::Steam => steam::account(),
        Service::Streams => None,
    });
    let account = account.await.ok().flatten();
    let note = match service {
        Service::Steam => Some(steam::NOTE.to_string()),
        Service::Streams => Some(livestream::NOTE.to_string()),
        _ if tool.as_ref().is_some_and(|t| t.path.is_none()) => Some("Its helper is installed when first needed".to_string()),
        _ => None,
    };
    Status { service, tool, account, note }
}

/// One thing the user does to sign in, in order.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum Step {
    /// Open this page in the browser and sign in there.
    Open { url: String, why: String },
    /// Paste back what that page gives (a code, the address it ends on, a name).
    Paste { what: String },
    /// The tool opens the browser or waits for the phone by itself: wait for it.
    Wait { why: String },
    /// Show this as it is, in a fixed-width font: a QR code to scan with a phone.
    Show { text: String },
}

/// The steps of `service`'s sign-in that come before [`sign_in`] is called with the answer of
/// the `Paste` among them (if any; an empty answer otherwise).
pub fn steps(service: Service) -> Vec<Step> {
    match service {
        Service::Cloud => vec![
            Step::Paste { what: "A name for the drive and its kind, e.g. \"work onedrive\" (drive, onedrive, dropbox, box, pcloud, mega, ...)".to_string() },
            Step::Wait { why: "rclone opens the browser at the drive's sign-in page: sign in there and come back".to_string() },
        ],
        Service::Epic => vec![
            Step::Open { url: epic::SIGN_IN_PAGE.to_string(), why: "Sign in to Epic Games there; the page then shows a short text with an authorizationCode".to_string() },
            Step::Paste { what: "The authorizationCode (or the whole text the page shows)".to_string() },
        ],
        Service::Gog => vec![
            Step::Open { url: gog::sign_in_page(), why: "Sign in to GOG there; it ends on an almost blank page".to_string() },
            Step::Paste { what: "The address of the page it ends on (or the code in it)".to_string() },
        ],
        Service::Steam => vec![
            Step::Paste { what: "The name you sign in to Steam with (never the password)".to_string() },
            Step::Wait { why: "A QR code comes up: scan it with the Steam app on your phone and confirm there".to_string() },
        ],
        Service::Streams => Vec::new(),
    }
}

/// Signs in to `service` with `answer` (what the user pasted, see [`steps`]), through the tool's
/// own flow; `show` is told what to show meanwhile (the page rclone opens, Steam's QR code).
/// Returns who is signed in now.
pub async fn sign_in(service: Service, answer: &str, proxy: Option<&str>, mut show: impl FnMut(Step) + Send) -> Result<String, String> {
    match service {
        Service::Cloud => {
            let mut words = answer.split_whitespace();
            let (Some(name), Some(kind), None) = (words.next(), words.next(), words.next()) else {
                return Err("Give a name for the drive and its kind, e.g. \"work onedrive\"".to_string());
            };
            rclone::sign_in(name, kind, proxy, |url| show(Step::Open { url, why: "If the browser did not open, open this page and sign in there".to_string() })).await
        }
        Service::Epic => epic::sign_in(answer, proxy).await,
        Service::Gog => gog::sign_in(answer, proxy).await,
        Service::Steam => steam::sign_in(answer, proxy, |text| show(Step::Show { text })).await,
        Service::Streams => Err("Live streams need no account".to_string()),
    }
}

/// Signs out of `service`: forgets the sign-in the app's folder holds. For cloud drives `what`
/// names the drive to remove (all of them without it).
pub async fn sign_out(service: Service, what: Option<&str>, proxy: Option<&str>) -> Result<(), String> {
    match service {
        Service::Cloud => {
            let names = match what {
                Some(name) => vec![name.to_string()],
                None => tokio::task::spawn_blocking(rclone::remotes).await.unwrap_or_default(),
            };
            for name in names {
                rclone::sign_out(&name, proxy).await?;
            }
            Ok(())
        }
        Service::Epic => epic::sign_out(proxy).await,
        Service::Gog => gog::sign_out(),
        Service::Steam => steam::sign_out(),
        Service::Streams => Ok(()),
    }
}

/// One thing an account can download.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Item {
    /// The link that downloads it: added as any link (see [`list`]).
    pub link: String,
    pub title: String,
    /// Its size, when the service tells it.
    pub size: Option<u64>,
    /// The version the app downloaded before, when it knows one.
    pub installed: Option<String>,
    /// What it is ("game", "DLC", "drive"), when it is not plain.
    pub kind: Option<String>,
}

/// What `service`'s account can download.
pub async fn library(service: Service, proxy: Option<&str>) -> Result<Vec<Item>, String> {
    match service {
        Service::Cloud => {
            let remotes = tokio::task::spawn_blocking(rclone::remotes).await.unwrap_or_default();
            Ok(remotes
                .into_iter()
                .filter_map(|name| {
                    let link = rclone::link(&name, "")?.to_string();
                    Some(Item { link, title: name, size: None, installed: None, kind: Some("drive".to_string()) })
                })
                .collect())
        }
        Service::Epic => epic::library(proxy).await,
        Service::Gog => gog::library(proxy).await,
        Service::Steam | Service::Streams => Ok(Vec::new()),
    }
}

/// Whether `url` is a link of an account's helper (see the module).
pub fn handles(url: &Url) -> bool {
    matches!(url.scheme(), "rclone" | "epic" | "gog" | "steam")
}

/// Whether `token` may be such a link (see [`link_of`]), from its shape alone, reading no file:
/// for a UI thread (see `ingest::needs_reading`).
pub fn takes(token: &str) -> bool {
    let token = token.trim();
    Url::parse(token).is_ok_and(|url| handles(&url) || steam::store_app(&url).is_some()) || rclone::may_be_remote(token)
}

/// `token` as such a link: one already, a cloud drive's `remote:path` (see
/// `rclone::link_of`), or a Steam store page's while the user is signed in to Steam. Blocking (it
/// reads the app's rclone config).
pub fn link_of(token: &str) -> Option<Url> {
    let url = Url::parse(token.trim()).ok();
    if let Some(url) = url.as_ref().filter(|u| handles(u) && u.scheme() != "rclone") {
        return Some(url.clone());
    }
    if let Some(app) = url.as_ref().and_then(steam::store_app).filter(|_| steam::account().is_some()) {
        return Url::parse(&format!("steam://app/{app}")).ok();
    }
    rclone::link_of(token.trim())
}

/// The downloads of `token` when it is such a link (see [`link_of`]): a cloud drive's file or
/// every file of its folder, a GOG game's installers and extras, or the one download an Epic or
/// Steam link is. None for any other token.
pub async fn list(token: &str, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    let owned = token.to_string();
    let url = tokio::task::spawn_blocking(move || link_of(&owned)).await.ok().flatten()?;
    let proxy = options.proxy.as_deref();
    Some(match url.scheme() {
        "rclone" => rclone::list(&url, proxy).await,
        "gog" => gog::list(&url, proxy).await,
        _ => Ok(vec![Task { urls: vec![url], ..Task::default() }]),
    })
}

/// How the engine downloads such a link.
pub(crate) enum Route {
    /// The helper downloaded it: where, and whether anything of it arrived now.
    Done((PathBuf, bool)),
    /// The engine downloads it over HTTP from `url` with `options`, `keep` alive meanwhile (the
    /// rclone server it comes from).
    Http { url: Url, options: DownloadOptions, keep: Option<Arc<rclone::Served>> },
}

/// How the engine downloads `link` (see [`Route`]), with the download's `options`, into `save_dir`.
pub(crate) async fn route(
    link: &Url,
    options: &DownloadOptions,
    save_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<Route, String> {
    let proxy = options.proxy.as_deref();
    match link.scheme() {
        "rclone" => {
            let (remote, path) = rclone::parts(link).ok_or_else(|| format!("{link} names no drive"))?;
            let served = tokio::select! {
                served = rclone::served(proxy) => served?,
                () = cancel.cancelled() => return Err(crate::engine::CANCELLED.to_string()),
            };
            let url = served.url(&remote, &path).ok_or_else(|| format!("{link} names no file"))?;
            // rclone itself goes through the proxy; the engine reaches it on 127.0.0.1 directly.
            let options = DownloadOptions {
                auth_header: Some(served.auth.clone()),
                no_proxy: true,
                bind_addresses: Vec::new(),
                spread_addresses: false,
                ip_family: crate::engine::IpFamily::Any,
                ..options.clone()
            };
            Ok(Route::Http { url, options, keep: Some(served) })
        }
        "gog" => {
            let (url, auth) = gog::route(link, proxy).await?;
            Ok(Route::Http { url, options: DownloadOptions { auth_header: Some(auth), ..options.clone() }, keep: None })
        }
        "epic" => epic::download(link, options, save_dir, snapshots, cancel).await.map(Route::Done),
        "steam" => steam::download(link, options, save_dir, snapshots, cancel).await.map(Route::Done),
        other => Err(format!("{other}: links are not an account's")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_have_names_and_links_route_to_them() {
        for service in Service::ALL {
            assert_eq!(Service::parse(service.id()), Some(service));
        }
        assert_eq!(Service::parse(" GOG "), Some(Service::Gog));
        assert!(Service::parse("netflix").is_none());
        assert!(Service::Gog.tool().is_none() && Service::Cloud.tool().is_some());
        assert!(steps(Service::Streams).is_empty());
        assert!(matches!(steps(Service::Epic)[1], Step::Paste { .. }));
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(handles(&url("epic://Fortnite")) && handles(&url("gog://game/1")) && !handles(&url("https://example.com/")));
        assert_eq!(link_of("epic://Sugar").unwrap().as_str(), "epic://Sugar");
        assert!(link_of("https://store.steampowered.com/app/440/").is_none(), "only while signed in to Steam");
        assert!(link_of("https://example.com/a.zip").is_none());
    }

    #[tokio::test]
    async fn an_epic_or_steam_link_is_one_download() {
        let listed = list("steam://app/228980", &ListOptions::default()).await.unwrap().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].urls[0].as_str(), "steam://app/228980");
        assert!(list("https://example.com/a.zip", &ListOptions::default()).await.is_none());
    }
}
