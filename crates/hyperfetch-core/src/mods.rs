//! Game mods: Modrinth projects, versions and modpacks (`.mrpack`), CurseForge projects, files and
//! modpacks through its official API with the user's own key, Thunderstore packages with their
//! dependencies, and Nexus Mods files with the user's own key. Each file is a task with the size
//! and hash its service publishes; a modpack's files go to the paths it states, and a task unpacks
//! its `overrides` folders from the pack (see [`run`]).
//!
//! What a service keeps to its own site stays there: a CurseForge file whose author turned off
//! downloads by other apps, and a Nexus Mods file for an account without premium (Nexus's API
//! gives those no link), are reported with the page to open in the browser, not fetched otherwise.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use percent_encoding::percent_decode_str;
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{EngineSnapshot, CANCELLED};
use crate::ingest::{clean_path, ListOptions, Task};
use crate::repos::{checksum, url_at};
use crate::worker::RateLimiter;
use crate::zipdir::ZipEntry;

const MODRINTH_API: &str = "https://api.modrinth.com/v2";
const CURSEFORGE_API: &str = "https://api.curseforge.com/v1";
const THUNDERSTORE: &str = "https://thunderstore.io";
const NEXUS_API: &str = "https://api.nexusmods.com/v1";
/// Between two requests to one of these APIs (Modrinth allows 300 a minute).
const GAP: Duration = Duration::from_millis(250);
/// The hosts a .mrpack's files may download from, as Modrinth's format allows.
const PACK_HOSTS: &[&str] = &["cdn.modrinth.com", "github.com", "raw.githubusercontent.com", "gitlab.com"];
/// Most files one modpack lists, and the largest index or manifest read out of one.
const MAX_PACK_FILES: usize = 5000;
const MAX_INDEX: u64 = 16 << 20;
/// How deep and how many Thunderstore dependencies are followed.
const MAX_DEPTH: usize = 5;
const MAX_PACKAGES: usize = 100;
/// Most files of one Nexus Mods mod asked for a download link.
const MAX_NEXUS_FILES: usize = 20;
/// The scheme of the task unpacking a modpack's overrides: `modpack:<the pack's link>#<folders>`.
const SCHEME: &str = "modpack";
/// Modrinth's kinds of project, the first part of a project's path.
const MODRINTH_KINDS: &[&str] = &["mod", "plugin", "datapack", "resourcepack", "shader", "modpack"];
/// First path parts of curseforge.com that are its own pages, not games.
const CURSEFORGE_PAGES: &[&str] = &["api", "account", "download", "legal", "linkout", "members", "moderation", "projects", "search"];
/// Mod loaders a version may be picked by (see [`Pick`]), as Modrinth names them.
const LOADERS: &[&str] = &[
    "fabric", "forge", "neoforge", "quilt", "liteloader", "paper", "spigot", "bukkit", "purpur", "folia", "sponge", "velocity",
    "bungeecord", "waterfall", "iris", "optifine", "canvas", "datapack",
];
const CURSEFORGE_KEY: &str =
    "CurseForge's API needs your own API key (from console.curseforge.com): set it in Settings, or with --curseforge-key";
const NEXUS_KEY: &str =
    "Nexus Mods' API needs your own personal API key (from the API keys page of your account on nexusmods.com): set it in Settings, or with --nexus-key";

/// What a mod link is, from its shape.
#[derive(Debug, PartialEq)]
enum Mod {
    /// modrinth.com/{mod|plugin|datapack|resourcepack|shader|modpack}/{slug}[/version/{id or number}]
    Modrinth { project: String, version: Option<String> },
    /// A link to a .mrpack.
    Pack(Url),
    /// curseforge.com/{game}/{class}/{slug}[/files/{id}]
    CurseForge { game: String, class: String, slug: String, file: Option<u64> },
    /// thunderstore.io/c/{community}/p/{namespace}/{name}[/v/{version}], or /package/{namespace}/{name}[/{version}]
    Thunderstore { namespace: String, name: String, version: Option<String> },
    /// nexusmods.com/[games/]{game}/mods/{id}[?tab=files&file_id={file}]
    Nexus { game: String, id: u64, file: Option<u64> },
    /// nxm://{game}/mods/{id}/files/{file}?key=..&expires=..: the link the download button of Nexus
    /// Mods' site makes, whose key gives an account without premium its download link.
    Nxm { game: String, id: u64, file: u64, key: String, expires: String },
}

fn mod_of(url: &Url) -> Option<Mod> {
    let decoded: Vec<String> =
        url.path_segments()?.filter(|s| !s.is_empty()).map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let segs: Vec<&str> = decoded.iter().map(String::as_str).collect();
    let query = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    let owned = |s: &str| s.to_string();
    if url.scheme() == "nxm" {
        let ["mods", id, "files", file] = segs[..] else { return None };
        let (key, expires) = (query("key")?, query("expires")?);
        return Some(Mod::Nxm { game: url.host_str()?.to_string(), id: id.parse().ok()?, file: file.parse().ok()?, key, expires });
    }
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.trim_end_matches('.');
    match (host.strip_prefix("www.").unwrap_or(host), &segs[..]) {
        ("modrinth.com", [kind, slug, rest @ ..]) if MODRINTH_KINDS.contains(kind) => match rest {
            [] | ["versions"] => Some(Mod::Modrinth { project: owned(slug), version: None }),
            ["version", version] => Some(Mod::Modrinth { project: owned(slug), version: Some(owned(version)) }),
            _ => None,
        },
        ("curseforge.com", [game, class, slug, rest @ ..]) if !CURSEFORGE_PAGES.contains(game) => {
            let file = match rest {
                [] | ["files"] => None,
                ["files" | "download", id] => Some(id.parse().ok()?),
                _ => return None,
            };
            Some(Mod::CurseForge { game: owned(game), class: owned(class), slug: owned(slug), file })
        }
        ("thunderstore.io", ["c", _, "p", namespace, name, rest @ ..] | ["package", namespace, name, rest @ ..]) if *namespace != "download" => {
            let version = match rest {
                [] => None,
                ["v", version] | [version] if version.starts_with(|c: char| c.is_ascii_digit()) => Some(owned(version)),
                _ => return None,
            };
            Some(Mod::Thunderstore { namespace: owned(namespace), name: owned(name), version })
        }
        ("nexusmods.com", [game, "mods", id] | ["games", game, "mods", id]) => {
            Some(Mod::Nexus { game: owned(game), id: id.parse().ok()?, file: query("file_id").and_then(|f| f.parse().ok()) })
        }
        _ if url.path().to_ascii_lowercase().ends_with(".mrpack") => Some(Mod::Pack(url.clone())),
        _ => None,
    }
}

/// Whether `url` is a mod, modpack or `.mrpack` link [`list`] reads.
pub fn lists(url: &Url) -> bool {
    mod_of(url).is_some()
}

/// Whether `url` is the task unpacking a modpack's overrides (see [`run`]).
pub fn handles(url: &Url) -> bool {
    url.scheme() == SCHEME
}

/// The files the mod, modpack or `.mrpack` link `url` names: a project's newest version that
/// `options.formats` picks (see [`Pick`]), a version's or file's own, a modpack's files.
pub async fn list(http: &Client, url: &Url, options: &ListOptions) -> Result<Vec<Task>, String> {
    let pick = Pick::of(&options.formats);
    let keys = &options.keys;
    let tasks = match mod_of(url) {
        Some(Mod::Modrinth { project, version }) => modrinth(http, MODRINTH_API, &project, version.as_deref(), &pick, options).await?,
        Some(Mod::Pack(link)) => pack(http, &link, &pick, options).await?,
        Some(Mod::CurseForge { game, class, slug, file }) => {
            let cf = CurseForge { http, api: CURSEFORGE_API, key: key(&keys.curseforge, CURSEFORGE_KEY)? };
            cf.list(&game, &class, &slug, file, &pick, options).await?
        }
        Some(Mod::Thunderstore { namespace, name, version }) => thunderstore(http, THUNDERSTORE, &namespace, &name, version.as_deref(), options).await?,
        Some(Mod::Nexus { game, id, file }) => nexus(http, NEXUS_API, key(&keys.nexus, NEXUS_KEY)?, &game, id, file, None, options).await?,
        Some(Mod::Nxm { game, id, file, key: link_key, expires }) => {
            nexus(http, NEXUS_API, key(&keys.nexus, NEXUS_KEY)?, &game, id, Some(file), Some((link_key.as_str(), expires.as_str())), options).await?
        }
        None => return Err(format!("{url} is not a mod link")),
    };
    if tasks.is_empty() {
        return Err("the link holds no files the app can download".to_string());
    }
    Ok(tasks)
}

fn key<'a>(key: &'a Option<String>, missing: &str) -> Result<&'a str, String> {
    key.as_deref().map(str::trim).filter(|k| !k.is_empty()).ok_or_else(|| missing.to_string())
}

/// A file or folder name made safe as one path component.
fn component(name: &str) -> Result<PathBuf, String> {
    clean_path([name.trim()])
}

/// What a project's version is picked by, from `ListOptions::formats`: game versions ("1.21.1"),
/// loaders ("fabric"), and "server" for a modpack's server side (else its client's).
#[derive(Default)]
struct Pick {
    games: Vec<String>,
    loaders: Vec<String>,
    server: bool,
}

impl Pick {
    fn of(formats: &[String]) -> Pick {
        let mut pick = Pick::default();
        for term in formats.iter().map(|f| f.trim().to_ascii_lowercase()) {
            if term == "server" {
                pick.server = true;
            } else if LOADERS.contains(&term.as_str()) {
                pick.loaders.push(term);
            } else if term.starts_with(|c: char| c.is_ascii_digit()) && term.contains('.') {
                pick.games.push(term);
            }
        }
        pick
    }

    fn any(&self) -> bool {
        !self.games.is_empty() || !self.loaders.is_empty()
    }

    fn terms(&self) -> String {
        [&self.games[..], &self.loaders[..]].concat().join(", ")
    }
}

/// The JSON `site` answers to `request` (made for `url`), paced to [`GAP`]; None for 404. A 401
/// or 403 from a site asked with the user's key (`keyed`) says the key was refused.
async fn api<T: DeserializeOwned>(url: &Url, site: &str, keyed: bool, request: impl Fn() -> RequestBuilder) -> Result<Option<T>, String> {
    let answer = crate::polite::send(url, GAP, request).await?;
    match answer.status() {
        StatusCode::NOT_FOUND => return Ok(None),
        status @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) if keyed => {
            return Err(format!("{site} refused your API key (HTTP {}): check it in Settings", status.as_u16()))
        }
        status if !status.is_success() => return Err(crate::polite::refused(site, status)),
        _ => {}
    }
    let body = crate::archives::body(answer, site).await?;
    serde_json::from_slice(&body).map(Some).map_err(|_| format!("{site} answered with something the app cannot read"))
}

/// A GET of `url` from `site`, which needs no key (see [`api`]).
async fn open<T: DeserializeOwned>(http: &Client, url: Url, site: &str) -> Result<Option<T>, String> {
    api(&url, site, false, || http.get(url.clone())).await
}

/// The user's key as a header value never logged.
fn secret(key: &str) -> HeaderValue {
    let mut value = HeaderValue::from_str(key).unwrap_or(HeaderValue::from_static(""));
    value.set_sensitive(true);
    value
}

#[derive(Deserialize)]
struct Version {
    files: Vec<VersionFile>,
}

#[derive(Deserialize)]
struct VersionFile {
    url: Url,
    filename: String,
    #[serde(default)]
    hashes: Hashes,
    size: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Hashes {
    sha512: Option<String>,
    sha1: Option<String>,
}

impl Hashes {
    fn checksum(&self) -> Option<String> {
        let sha512 = self.sha512.as_deref().and_then(|hex| checksum("sha512", hex));
        sha512.or_else(|| checksum("sha1", self.sha1.as_deref()?))
    }
}

/// The files of Modrinth's `project` at `version` (an id or a version number), else of its
/// newest version for the game versions and loaders `pick` names; a modpack's are its .mrpack's.
async fn modrinth(http: &Client, api: &str, project: &str, version: Option<&str>, pick: &Pick, options: &ListOptions) -> Result<Vec<Task>, String> {
    let found: Version = match version {
        Some(v) => open(http, url_at(api, ["project", project, "version", v])?, "Modrinth").await?.ok_or_else(|| format!("Modrinth has no version {v} of {project}"))?,
        None => {
            let mut url = url_at(api, ["project", project, "version"])?;
            for (name, terms) in [("game_versions", &pick.games), ("loaders", &pick.loaders)] {
                if !terms.is_empty() {
                    url.query_pairs_mut().append_pair(name, &serde_json::to_string(terms).unwrap_or_default());
                }
            }
            let versions: Vec<Version> = open(http, url, "Modrinth").await?.ok_or_else(|| format!("Modrinth has no project {project}"))?;
            let none = || match pick.any() {
                true => format!("Modrinth has no version of {project} for {}", pick.terms()),
                false => format!("Modrinth lists no version of {project}"),
            };
            versions.into_iter().next().ok_or_else(none)?
        }
    };
    // A modpack's version is its .mrpack: the files it lists are the downloads.
    if let Some(mrpack) = found.files.iter().find(|f| f.filename.ends_with(".mrpack")) {
        return pack(http, &mrpack.url, pick, options).await;
    }
    found
        .files
        .into_iter()
        .map(|f| Ok(Task { name: Some(component(&f.filename)?), checksum: f.hashes.checksum(), size: f.size, from_document: true, urls: vec![f.url], ..Task::default() }))
        .collect()
}

#[derive(Deserialize)]
struct Index {
    name: Option<String>,
    #[serde(default)]
    files: Vec<IndexFile>,
}

#[derive(Deserialize)]
struct IndexFile {
    path: String,
    #[serde(default)]
    hashes: Hashes,
    env: Option<Env>,
    #[serde(default)]
    downloads: Vec<String>,
    #[serde(rename = "fileSize")]
    size: Option<u64>,
}

#[derive(Deserialize)]
struct Env {
    client: Option<String>,
    server: Option<String>,
}

/// The files of the .mrpack at `link`: its index's, at their paths in a folder named after the
/// pack, with their hashes, and the task unpacking its overrides (see [`overrides_task`]). A file
/// the side `pick` names does not use ("unsupported") is left out, and so is one whose links are
/// all on hosts the format does not allow.
async fn pack(http: &Client, link: &Url, pick: &Pick, options: &ListOptions) -> Result<Vec<Task>, String> {
    let entries = crate::zipdir::list_zip(http, link).await?;
    let index: Index = read_entry(http, link, &entries, "modrinth.index.json").await?;
    let folder = component(index.name.as_deref().unwrap_or("modpack")).or_else(|_| component("modpack"))?;
    if index.files.len() > MAX_PACK_FILES {
        options.note(format!("Only the first {MAX_PACK_FILES} of the modpack's {} files are listed", index.files.len()));
    }
    let (mut tasks, mut refused) = (Vec::new(), Vec::new());
    for file in index.files.into_iter().take(MAX_PACK_FILES) {
        let env = file.env.as_ref().and_then(|env| if pick.server { env.server.as_deref() } else { env.client.as_deref() });
        let Some(name) = crate::repos::relative(&file.path).filter(|_| env != Some("unsupported")) else { continue };
        let allowed = |u: &Url| u.scheme() == "https" && u.host_str().is_some_and(|h| PACK_HOSTS.contains(&h));
        let urls: Vec<Url> = file.downloads.iter().filter_map(|d| Url::parse(d).ok()).filter(allowed).collect();
        if urls.is_empty() {
            refused.push(file.path);
            continue;
        }
        tasks.push(Task { urls, name: Some(name), folder: Some(folder.clone()), checksum: file.hashes.checksum(), size: file.size, from_document: true, ..Task::default() });
    }
    if !refused.is_empty() {
        options.note(format!("Left out what the modpack links on hosts its format does not allow: {}", refused.join(", ")));
    }
    let side = format!("{}-overrides", if pick.server { "server" } else { "client" });
    let dirs = ["overrides", side.as_str()];
    if entries.iter().any(|e| !e.is_folder() && dirs.iter().any(|dir| inside(&e.name, dir).is_some())) {
        tasks.push(overrides_task(link, &dirs, &folder)?);
    }
    Ok(tasks)
}

/// The JSON file `name` of the ZIP at `link` whose `entries` are given, read out of it alone.
async fn read_entry<T: DeserializeOwned>(http: &Client, link: &Url, entries: &[ZipEntry], name: &str) -> Result<T, String> {
    let entry = entries.iter().find(|e| e.name == name).ok_or_else(|| format!("the pack has no {name}"))?;
    if entry.size > MAX_INDEX {
        return Err(format!("the pack's {name} is larger than the app reads"));
    }
    let mut bytes = Vec::new();
    crate::zipdir::extract(http, None, link, entry, &mut bytes, None, &mut |_: u64| {}).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("the pack's {name} cannot be read: {e}"))
}

/// What of the ZIP entry `name` lies inside the folder `dir`, if anything.
fn inside<'a>(name: &'a str, dir: &str) -> Option<&'a str> {
    name.strip_prefix(dir)?.strip_prefix('/').filter(|rest| !rest.is_empty())
}

/// The task that unpacks the folders `dirs` of the pack at `link` into the pack's `folder`.
fn overrides_task(link: &Url, dirs: &[&str], folder: &Path) -> Result<Task, String> {
    let mut zip = link.clone();
    zip.set_fragment(None);
    let url = Url::parse(&format!("{SCHEME}:{zip}#{}", dirs.join(","))).map_err(|e| e.to_string())?;
    Ok(Task { urls: vec![url], name: Some(PathBuf::from("overrides")), folder: Some(folder.to_path_buf()), from_document: true, ..Task::default() })
}

/// The pack's link and the folders an [`overrides_task`] link names.
fn pack_link(link: &Url) -> Option<(Url, Vec<String>)> {
    let (zip, dirs) = link.as_str().strip_prefix(SCHEME)?.strip_prefix(':')?.rsplit_once('#')?;
    Some((Url::parse(zip).ok()?, dirs.split(',').map(|d| percent_decode_str(d).decode_utf8_lossy().into_owned()).collect()))
}

/// Unpacks the folders the link of an [`overrides_task`] names from its pack into `save_dir`, at
/// their paths inside them: each file read out of the pack alone and checked against its CRC-32,
/// a later folder's over an earlier one's (a side's own overrides over the shared ones).
pub(crate) async fn run(
    link: &Url,
    http: &Client,
    limiter: Option<Arc<RateLimiter>>,
    save_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let (zip, dirs) = pack_link(link).ok_or("not a modpack's overrides link")?;
    let entries = crate::zipdir::list_zip(http, &zip).await?;
    let chosen: Vec<(&ZipEntry, PathBuf)> = dirs
        .iter()
        .flat_map(|dir| entries.iter().filter(|e| !e.is_folder()).filter_map(move |e| Some((e, crate::repos::relative(inside(&e.name, dir)?)?))))
        .collect();
    let total: u64 = chosen.iter().map(|(e, _)| e.size).sum();
    let mut done = 0;
    for (entry, path) in chosen {
        let target = save_dir.join(path);
        let failed = |e: std::io::Error| format!("Failed to write {}: {e}", target.display());
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(failed)?;
        }
        let part = crate::engine::part_path(&target);
        let mut file = tokio::fs::File::create(&part).await.map_err(failed)?;
        let mut shown = |_: u64| {};
        let extracted = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(CANCELLED.to_string()),
            got = crate::zipdir::extract(http, None, &zip, entry, &mut file, limiter.as_deref(), &mut shown) => got,
        };
        drop(file);
        if let Err(e) = extracted {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        tokio::fs::rename(&part, &target).await.map_err(failed)?;
        done += entry.size;
        if let Some(tx) = &snapshots {
            let progress_ratio = if total > 0 { done as f64 / total as f64 } else { 1.0 };
            let _ = tx.send(EngineSnapshot { total_bytes: total, downloaded_bytes: done, progress_ratio, target_path: Some(save_dir.to_path_buf()), ..Default::default() });
        }
    }
    Ok((save_dir.to_path_buf(), true))
}

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

#[derive(Deserialize)]
struct CfGame {
    id: u64,
    slug: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CfMod {
    id: u64,
    class_id: Option<u64>,
    #[serde(default)]
    links: CfLinks,
    main_file_id: Option<u64>,
    #[serde(default)]
    latest_files_indexes: Vec<CfIndex>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CfLinks {
    website_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CfIndex {
    game_version: String,
    file_id: u64,
    mod_loader: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CfFile {
    id: u64,
    mod_id: u64,
    file_name: String,
    file_length: Option<u64>,
    download_url: Option<Url>,
    #[serde(default)]
    hashes: Vec<CfHash>,
}

#[derive(Deserialize)]
struct CfHash {
    value: String,
    /// 1 SHA-1, 2 MD5.
    algo: u32,
}

#[derive(Deserialize)]
struct Manifest {
    name: Option<String>,
    #[serde(default)]
    files: Vec<ManifestFile>,
    overrides: Option<String>,
}

#[derive(Deserialize)]
struct ManifestFile {
    #[serde(rename = "projectID")]
    project: u64,
    #[serde(rename = "fileID")]
    file: u64,
    required: Option<bool>,
}

/// CurseForge's loader number for `loader`.
fn loader_id(loader: &str) -> Option<u32> {
    Some(match loader {
        "forge" => 1,
        "liteloader" => 3,
        "fabric" => 4,
        "quilt" => 5,
        "neoforge" => 6,
        _ => return None,
    })
}

/// CurseForge's API at `api`, asked with the user's `key`.
struct CurseForge<'a> {
    http: &'a Client,
    api: &'a str,
    key: &'a str,
}

impl CurseForge<'_> {
    async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<Option<T>, String> {
        api(&url, "CurseForge", true, || self.http.get(url.clone()).header("x-api-key", secret(self.key))).await
    }

    /// What CurseForge answers to `body` posted to `path` (its batch lookups).
    async fn post<T: DeserializeOwned>(&self, path: &[&str], body: serde_json::Value) -> Result<T, String> {
        let url = url_at(self.api, path.iter().copied())?;
        let (body, key) = (body.to_string(), secret(self.key));
        let request = || self.http.post(url.clone()).header("x-api-key", key.clone()).header(CONTENT_TYPE, "application/json").body(body.clone());
        let found: Option<Data<T>> = api(&url, "CurseForge", true, request).await?;
        found.map(|d| d.data).ok_or_else(|| "CurseForge has no such files".to_string())
    }

    async fn files(&self, ids: &[u64]) -> Result<Vec<CfFile>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.post(&["mods", "files"], serde_json::json!({ "fileIds": ids })).await
    }

    /// The file a CurseForge link names: its own, or the project's newest for what `pick` names
    /// (its main file without a pick); a modpack's files instead of a modpack's zip.
    async fn list(&self, game: &str, class: &str, slug: &str, file: Option<u64>, pick: &Pick, options: &ListOptions) -> Result<Vec<Task>, String> {
        let page = format!("https://www.curseforge.com/{game}/{class}/{slug}");
        let id = match file {
            Some(id) => id,
            None => {
                let project = self.project(game, class, slug).await?;
                let fits = |i: &&CfIndex| {
                    (pick.games.is_empty() || pick.games.iter().any(|g| g.eq_ignore_ascii_case(&i.game_version)))
                        && (pick.loaders.is_empty() || pick.loaders.iter().any(|l| loader_id(l).is_some() && loader_id(l) == i.mod_loader))
                };
                let picked = if pick.any() { project.latest_files_indexes.iter().find(fits).map(|i| i.file_id) } else { project.main_file_id };
                picked.ok_or_else(|| format!("CurseForge has no file of {slug} for {}", pick.terms()))?
            }
        };
        let file = self.files(&[id]).await?.into_iter().find(|f| f.id == id).ok_or_else(|| format!("CurseForge has no file {id}"))?;
        if class == "modpacks" {
            return self.modpack(file, &page, options).await;
        }
        let task = cf_task(file, None, &page).map_err(|open| format!("Its author lets only CurseForge's own site and app download this file: open {open} in the browser"))?;
        Ok(vec![task])
    }

    /// CurseForge's project `slug` of the game `game`: the one its site shows at
    /// /{game}/{class}/{slug}.
    async fn project(&self, game: &str, class: &str, slug: &str) -> Result<CfMod, String> {
        let mut game_id = None;
        for index in (0..10).map(|page| page * 50) {
            let mut url = url_at(self.api, ["games"])?;
            url.query_pairs_mut().append_pair("index", &index.to_string()).append_pair("pageSize", "50");
            let games: Data<Vec<CfGame>> = self.get(url).await?.ok_or("CurseForge lists no games")?;
            let last = games.data.len() < 50;
            game_id = games.data.into_iter().find(|g| g.slug == game).map(|g| g.id);
            if game_id.is_some() || last {
                break;
            }
        }
        let game_id = game_id.ok_or_else(|| format!("CurseForge has no game {game}"))?;
        let mut url = url_at(self.api, ["mods", "search"])?;
        url.query_pairs_mut().append_pair("gameId", &game_id.to_string()).append_pair("slug", slug);
        let found: Data<Vec<CfMod>> = self.get(url).await?.ok_or_else(|| format!("CurseForge has no project {slug}"))?;
        let (path, only) = (format!("/{game}/{class}/{slug}"), found.data.len() == 1);
        let shown_at = |m: &CfMod| m.links.website_url.as_deref().and_then(|w| Url::parse(w).ok()).is_some_and(|w| w.path().trim_end_matches('/') == path);
        found.data.into_iter().find(|m| only || shown_at(m)).ok_or_else(|| format!("CurseForge has no project {slug} among its {game} {class}"))
    }

    /// The files the CurseForge modpack `file`'s manifest.json lists, to the folders its game
    /// keeps them in (mods, resource packs, shader packs), and the task unpacking its overrides.
    async fn modpack(&self, file: CfFile, page: &str, options: &ListOptions) -> Result<Vec<Task>, String> {
        let Some(link) = file.download_url.clone() else {
            return Err(format!("Its author lets only CurseForge's own site and app download this modpack: open {page}/files/{} in the browser", file.id));
        };
        let entries = crate::zipdir::list_zip(self.http, &link).await?;
        let manifest: Manifest = read_entry(self.http, &link, &entries, "manifest.json").await?;
        let folder = component(manifest.name.as_deref().unwrap_or(&file.file_name)).or_else(|_| component("modpack"))?;
        let wanted: Vec<&ManifestFile> = manifest.files.iter().filter(|f| f.required != Some(false)).take(MAX_PACK_FILES).collect();
        let ids: Vec<u64> = wanted.iter().map(|f| f.file).collect();
        let files = self.files(&ids).await?.into_iter().filter(|f| ids.contains(&f.id));
        let projects: Vec<u64> = wanted.iter().map(|f| f.project).collect();
        let mods: Vec<CfMod> = if projects.is_empty() { Vec::new() } else { self.post(&["mods"], serde_json::json!({ "modIds": projects })).await? };
        let (mut tasks, mut refused) = (Vec::new(), Vec::new());
        for file in files {
            let project = mods.iter().find(|m| m.id == file.mod_id);
            let dir = match project.and_then(|m| m.class_id) {
                Some(12) => "resourcepacks",
                Some(6552) => "shaderpacks",
                _ => "mods",
            };
            let page = project.and_then(|m| m.links.website_url.clone()).unwrap_or_else(|| format!("https://www.curseforge.com/projects/{}", file.mod_id));
            match cf_task(file, Some(dir), &page) {
                Ok(task) => tasks.push(Task { folder: Some(folder.clone()), ..task }),
                Err(open) => refused.push(open),
            }
        }
        if !refused.is_empty() {
            options.note(format!("Their authors let only CurseForge's own site and app download these files of the modpack: open {} in the browser", refused.join(", ")));
        }
        let overrides = manifest.overrides.as_deref().unwrap_or("overrides");
        if entries.iter().any(|e| !e.is_folder() && inside(&e.name, overrides).is_some()) {
            tasks.push(overrides_task(&link, &[overrides], &folder)?);
        }
        Ok(tasks)
    }
}

/// The task of CurseForge's `file`, under the folder `dir` if given, with its SHA-1 (else MD5);
/// Err with the page to open when its author turned off downloads by other apps (CurseForge then
/// gives it no download link).
fn cf_task(file: CfFile, dir: Option<&str>, page: &str) -> Result<Task, String> {
    let Some(url) = file.download_url else { return Err(format!("{page}/files/{}", file.id)) };
    let name = clean_path(dir.into_iter().chain([file.file_name.as_str()]))?;
    let hash = |algo: u32, name: &str| file.hashes.iter().find(|h| h.algo == algo).and_then(|h| checksum(name, &h.value));
    Ok(Task { urls: vec![url], name: Some(name), size: file.file_length, checksum: hash(1, "sha1").or_else(|| hash(2, "md5")), from_document: true, ..Task::default() })
}

#[derive(Deserialize)]
struct TsPackage {
    latest: TsVersion,
}

#[derive(Deserialize)]
struct TsVersion {
    full_name: String,
    #[serde(default)]
    dependencies: Vec<String>,
    download_url: Url,
}

/// Thunderstore's `namespace`-`name` at `version`, else its latest version; None when it has none.
async fn ts_version(http: &Client, base: &str, namespace: &str, name: &str, version: Option<&str>) -> Result<Option<TsVersion>, String> {
    let path = ["api", "experimental", "package", namespace, name];
    match version {
        Some(v) => open(http, url_at(base, path.into_iter().chain([v, ""]))?, "Thunderstore").await,
        None => Ok(open::<TsPackage>(http, url_at(base, path.into_iter().chain([""]))?, "Thunderstore").await?.map(|p| p.latest)),
    }
}

/// Thunderstore's package (or modpack) and the packages it depends on, each at the version named
/// first, [`MAX_DEPTH`] deep and [`MAX_PACKAGES`] in all, in a folder named after it.
async fn thunderstore(http: &Client, base: &str, namespace: &str, name: &str, version: Option<&str>, options: &ListOptions) -> Result<Vec<Task>, String> {
    let first = ts_version(http, base, namespace, name, version).await?;
    let first = first.ok_or_else(|| format!("Thunderstore has no package {namespace}-{name}{}", version.map(|v| format!(" {v}")).unwrap_or_default()))?;
    let folder = component(&first.full_name)?;
    let mut seen = HashSet::from([format!("{namespace}-{name}").to_ascii_lowercase()]);
    let (mut queue, mut tasks, mut cut) = (VecDeque::from([(first, 0)]), Vec::new(), false);
    while let Some((package, depth)) = queue.pop_front() {
        for dependency in &package.dependencies {
            // namespace-name-version: a name has no dash, a team's namespace may.
            let mut parts = dependency.rsplitn(3, '-');
            let (Some(v), Some(n), Some(ns)) = (parts.next(), parts.next(), parts.next()) else { continue };
            if !seen.insert(format!("{ns}-{n}").to_ascii_lowercase()) {
                continue;
            }
            if depth >= MAX_DEPTH || seen.len() > MAX_PACKAGES {
                cut = true;
                continue;
            }
            match ts_version(http, base, ns, n, Some(v)).await? {
                Some(found) => queue.push_back((found, depth + 1)),
                None => options.note(format!("Thunderstore has no package {dependency}, which {} depends on", package.full_name)),
            }
        }
        let name = component(&format!("{}.zip", package.full_name))?;
        tasks.push(Task { urls: vec![package.download_url], name: Some(name), folder: Some(folder.clone()), from_document: true, ..Task::default() });
    }
    if cut {
        options.note(format!("Only the first {MAX_PACKAGES} packages, at most {MAX_DEPTH} dependencies deep, are listed"));
    }
    Ok(tasks)
}

#[derive(Deserialize)]
struct NxFiles {
    files: Vec<NxFile>,
}

#[derive(Deserialize)]
struct NxFile {
    file_id: u64,
    file_name: String,
    category_name: Option<String>,
    size_in_bytes: Option<u64>,
}

#[derive(Deserialize)]
struct NxLink {
    #[serde(rename = "URI")]
    uri: Url,
}

/// Nexus Mods' files of the mod `id` of `game`: the file `file`, else its main files (else those
/// not old or archived), each through the download link Nexus's API gives the user's `key`, or
/// gives for the key and expiry of an `nxm` link. Those it gives no link (an account without
/// premium) are reported with their page to open in the browser.
#[allow(clippy::too_many_arguments)]
async fn nexus(
    http: &Client,
    api_base: &str,
    key: &str,
    game: &str,
    id: u64,
    file: Option<u64>,
    nxm: Option<(&str, &str)>,
    options: &ListOptions,
) -> Result<Vec<Task>, String> {
    let site = "Nexus Mods";
    let keyed = |request: RequestBuilder| {
        request.header("apikey", secret(key)).header("Application-Name", "Endo's Unified Downloader").header("Application-Version", env!("CARGO_PKG_VERSION"))
    };
    let mod_id = id.to_string();
    let url = url_at(api_base, ["games", game, "mods", &mod_id, "files.json"])?;
    let listed: NxFiles = api(&url, site, true, || keyed(http.get(url.clone()))).await?.ok_or_else(|| format!("Nexus Mods has no mod {id} of {game}"))?;
    let category = |f: &NxFile| f.category_name.clone().unwrap_or_default();
    let main = listed.files.iter().any(|f| category(f) == "MAIN");
    let chosen: Vec<&NxFile> = listed
        .files
        .iter()
        .filter(|f| match file {
            Some(wanted) => f.file_id == wanted,
            None if main => category(f) == "MAIN",
            None => !matches!(category(f).as_str(), "OLD_VERSION" | "ARCHIVED" | "DELETED"),
        })
        .take(MAX_NEXUS_FILES)
        .collect();
    if chosen.is_empty() {
        return Err(format!("Nexus Mods lists no such file of mod {id} of {game}"));
    }
    let (mut tasks, mut browser) = (Vec::new(), Vec::new());
    for f in chosen {
        let file_id = f.file_id.to_string();
        let mut link = url_at(api_base, ["games", game, "mods", &mod_id, "files", &file_id, "download_link.json"])?;
        if let Some((link_key, expires)) = nxm {
            link.query_pairs_mut().append_pair("key", link_key).append_pair("expires", expires);
        }
        let answer = crate::polite::send(&link, GAP, || keyed(http.get(link.clone()))).await?;
        let links: Vec<NxLink> = match answer.status() {
            // Its API gives links to premium accounts only, and for an nxm link's key.
            StatusCode::FORBIDDEN | StatusCode::GONE => Vec::new(),
            StatusCode::UNAUTHORIZED => return Err(format!("{site} refused your API key (HTTP 401): check it in Settings")),
            status if !status.is_success() => return Err(crate::polite::refused(site, status)),
            _ => serde_json::from_slice(&crate::archives::body(answer, site).await?).map_err(|_| format!("{site} answered with something the app cannot read"))?,
        };
        if links.is_empty() {
            browser.push(format!("https://www.nexusmods.com/{game}/mods/{id}?tab=files&file_id={file_id}"));
            continue;
        }
        tasks.push(Task { urls: links.into_iter().map(|l| l.uri).collect(), name: Some(component(&f.file_name)?), size: f.size_in_bytes, from_document: true, ..Task::default() });
    }
    if !browser.is_empty() {
        let why = format!(
            "Nexus Mods gives other apps download links only for premium accounts: open {} and use its download button (the browser extension then hands the file to the app)",
            browser.join(", ")
        );
        if tasks.is_empty() {
            return Err(why);
        }
        options.note(why);
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{serve, Reply};
    use crate::range::ByteRange;

    fn http() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    fn json(body: &str) -> Reply {
        (200, "Content-Type: application/json\r\n".into(), body.as_bytes().to_vec())
    }

    fn missing() -> Reply {
        (404, String::new(), b"{}".to_vec())
    }

    fn link(text: &str) -> Option<Mod> {
        mod_of(&Url::parse(text).unwrap())
    }

    /// A stored ZIP of `files`, as a pack's.
    fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let (mut out, mut directory) = (Vec::new(), Vec::new());
        for (name, data) in files {
            let mut crc = flate2::Crc::new();
            crc.update(data);
            let offset = out.len() as u32;
            let sizes = [crc.sum(), data.len() as u32, data.len() as u32];
            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend([20u16, 0x800, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()));
            out.extend(sizes.iter().flat_map(|v| v.to_le_bytes()));
            out.extend([name.len() as u16, 0].iter().flat_map(|v| v.to_le_bytes()));
            out.extend(name.as_bytes());
            out.extend(*data);
            directory.extend(0x0201_4b50u32.to_le_bytes());
            directory.extend([20u16, 20, 0x800, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()));
            directory.extend(sizes.iter().flat_map(|v| v.to_le_bytes()));
            directory.extend([name.len() as u16, 0, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()));
            directory.extend([0u32, offset].iter().flat_map(|v| v.to_le_bytes()));
            directory.extend(name.as_bytes());
        }
        let (offset, size, count) = (out.len() as u32, directory.len() as u32, files.len() as u16);
        out.extend(directory);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0u16, 0, count, count].iter().flat_map(|v| v.to_le_bytes()));
        out.extend([size, offset].iter().flat_map(|v| v.to_le_bytes()));
        out.extend(0u16.to_le_bytes());
        out
    }

    /// `data` as a server of parts of files answers a range request (the last bytes refused, as
    /// some servers do; `zipdir` then asks by place).
    fn part(data: &[u8], range: Option<ByteRange>) -> Reply {
        let Some(range) = range.filter(|r| r.start < data.len() as u64) else { return (416, String::new(), Vec::new()) };
        let end = range.end.min(data.len() as u64 - 1);
        let headers = format!("Content-Range: bytes {}-{end}/{}\r\n", range.start, data.len());
        (206, headers, data[range.start as usize..=end as usize].to_vec())
    }

    #[test]
    fn mod_links_are_told_by_their_shape() {
        let modrinth = |project: &str, version: Option<&str>| Some(Mod::Modrinth { project: project.into(), version: version.map(Into::into) });
        assert_eq!(link("https://modrinth.com/mod/sodium"), modrinth("sodium", None));
        assert_eq!(link("https://modrinth.com/modpack/fabulously-optimized/versions?g=1.21.1"), modrinth("fabulously-optimized", None));
        assert_eq!(link("https://modrinth.com/plugin/luckperms/version/v5.4.145-bukkit"), modrinth("luckperms", Some("v5.4.145-bukkit")));
        let pack = Url::parse("https://cdn.modrinth.com/data/1KVo5zza/versions/x/Pack%201.0.MRPACK").unwrap();
        assert_eq!(mod_of(&pack), Some(Mod::Pack(pack.clone())));
        assert_eq!(
            link("https://www.curseforge.com/minecraft/mc-mods/jei/files/5846880"),
            Some(Mod::CurseForge { game: "minecraft".into(), class: "mc-mods".into(), slug: "jei".into(), file: Some(5846880) })
        );
        assert_eq!(link("https://www.curseforge.com/minecraft/modpacks/all-the-mods-9"), Some(Mod::CurseForge { game: "minecraft".into(), class: "modpacks".into(), slug: "all-the-mods-9".into(), file: None }));
        let ts = |version: Option<&str>| Some(Mod::Thunderstore { namespace: "RiskofThunder".into(), name: "HookGenPatcher".into(), version: version.map(Into::into) });
        assert_eq!(link("https://thunderstore.io/c/riskofrain2/p/RiskofThunder/HookGenPatcher/"), ts(None));
        assert_eq!(link("https://thunderstore.io/c/riskofrain2/p/RiskofThunder/HookGenPatcher/v/1.2.9/"), ts(Some("1.2.9")));
        assert_eq!(link("https://thunderstore.io/package/RiskofThunder/HookGenPatcher/1.2.9/"), ts(Some("1.2.9")));
        assert_eq!(link("https://www.nexusmods.com/skyrimspecialedition/mods/266?tab=files&file_id=1000172397"), Some(Mod::Nexus { game: "skyrimspecialedition".into(), id: 266, file: Some(1000172397) }));
        assert_eq!(link("https://www.nexusmods.com/games/stardewvalley/mods/2400"), Some(Mod::Nexus { game: "stardewvalley".into(), id: 2400, file: None }));
        assert_eq!(
            link("nxm://skyrimspecialedition/mods/266/files/1000172397?key=k1&expires=1700000000&user_id=1"),
            Some(Mod::Nxm { game: "skyrimspecialedition".into(), id: 266, file: 1000172397, key: "k1".into(), expires: "1700000000".into() })
        );
        for other in [
            "https://modrinth.com/mods?q=sodium",
            "https://modrinth.com/mod/sodium/gallery",
            "https://modrinth.com/user/jellysquid3",
            "https://www.curseforge.com/minecraft/search?search=jei",
            "https://www.curseforge.com/members/x/projects",
            "https://thunderstore.io/package/download/RiskofThunder/HookGenPatcher/1.2.9/",
            "https://thunderstore.io/c/riskofrain2/p/RiskofThunder/HookGenPatcher/wiki/",
            "https://www.nexusmods.com/skyrimspecialedition/mods/",
            "nxm://skyrimspecialedition/mods/266/files/1",
            "https://example.com/pack.zip",
        ] {
            assert_eq!(link(other), None, "{other}");
        }
        let terms: Vec<String> = ["1.21.1", "Fabric", "server", "pdf"].map(String::from).to_vec();
        let pick = Pick::of(&terms);
        assert_eq!((pick.games, pick.loaders, pick.server), (vec!["1.21.1".to_string()], vec!["fabric".to_string()], true));
    }

    const SODIUM: &str = r#"[{"name":"Sodium 0.8.13 for Fabric 1.21.1","files":[{"hashes":{"sha1":"003c114c85ca88ef3362e018deb6aca0c682d6a1",
        "sha512":"8614a374593698069a80ac74de8cd9c28f3928d16819a0e0d6320b538e823f7e54e8be06d5ea5a9c268beb0e02073209d1f831a02dede851588b972367680853"},
        "url":"https://cdn.modrinth.com/data/AANobbMI/versions/SMxNOGZ6/sodium-fabric-0.8.13%2Bmc1.21.1.jar","filename":"sodium-fabric-0.8.13+mc1.21.1.jar",
        "primary":true,"size":1574609,"file_type":null}]}]"#;

    /// A Modrinth project is its newest version's files for the game version and loader picked,
    /// with their SHA-512; a modpack's version is the files its .mrpack lists, to their paths, the
    /// side's files only, its overrides unpacked by a task of their own.
    #[tokio::test]
    async fn modrinth_projects_and_packs_list_their_files() {
        let index = br#"{"formatVersion":1,"game":"minecraft","versionId":"1.0","name":"Tiny Pack","files":[
            {"path":"mods/a.jar","hashes":{"sha1":"0000000000000000000000000000000000000001","sha512":"aa"},"env":{"client":"required","server":"required"},
             "downloads":["https://cdn.modrinth.com/data/x/a.jar"],"fileSize":10},
            {"path":"mods/shaders-only.jar","hashes":{"sha1":"0000000000000000000000000000000000000002"},"env":{"client":"required","server":"unsupported"},
             "downloads":["https://cdn.modrinth.com/data/x/s.jar"],"fileSize":20},
            {"path":"mods/elsewhere.jar","hashes":{"sha1":"0000000000000000000000000000000000000003"},"downloads":["https://example.com/e.jar"],"fileSize":30},
            {"path":"../escape.jar","hashes":{},"downloads":["https://cdn.modrinth.com/data/x/e.jar"]}]}"#;
        let mrpack = zip(&[("modrinth.index.json", &index[..]), ("overrides/config/a.toml", b"x = 1"), ("server-overrides/server.properties", b"motd=hi")]);
        let host = Arc::new(std::sync::OnceLock::<String>::new());
        let named = Arc::clone(&host);
        let (addr, hits) = serve(move |path, range| match path {
            "/v2/project/sodium/version?game_versions=%5B%221.21.1%22%5D&loaders=%5B%22fabric%22%5D" => json(SODIUM),
            "/v2/project/sodium/version?game_versions=%5B%229.9%22%5D" => json("[]"),
            "/v2/project/tiny/version/1.0" => json(&r#"{"files":[{"url":"http://HOST/tiny.mrpack","filename":"Tiny Pack 1.0.mrpack","hashes":{}}]}"#.replace("HOST", named.get().unwrap())),
            "/tiny.mrpack" => part(&mrpack, range),
            _ => missing(),
        })
        .await;
        host.set(addr.to_string()).unwrap();
        let api = format!("http://{addr}/v2");
        let options = ListOptions::default();
        let terms = |t: &[&str]| Pick::of(&t.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let [jar] = &modrinth(&http(), &api, "sodium", None, &terms(&["1.21.1", "fabric"]), &options).await.unwrap()[..] else { panic!("one file") };
        assert_eq!(jar.urls[0].as_str(), "https://cdn.modrinth.com/data/AANobbMI/versions/SMxNOGZ6/sodium-fabric-0.8.13%2Bmc1.21.1.jar");
        assert_eq!((jar.name.clone().unwrap(), jar.size), (PathBuf::from("sodium-fabric-0.8.13+mc1.21.1.jar"), Some(1574609)));
        assert!(jar.checksum.as_deref().unwrap().starts_with("sha512:8614a374"));
        let none = modrinth(&http(), &api, "sodium", None, &terms(&["9.9"]), &options).await.unwrap_err();
        assert_eq!(none, "Modrinth has no version of sodium for 9.9");
        assert_eq!(modrinth(&http(), &api, "gone", Some("1"), &terms(&[]), &options).await.unwrap_err(), "Modrinth has no version 1 of gone");

        // A modpack's version is what its .mrpack lists.
        let link = Url::parse(&format!("http://{addr}/tiny.mrpack")).unwrap();
        let tasks = modrinth(&http(), &api, "tiny", Some("1.0"), &terms(&[]), &options).await.unwrap();
        let names: Vec<_> = tasks.iter().map(|t| t.name.clone().unwrap()).collect();
        assert_eq!(names, [PathBuf::from("mods").join("a.jar"), PathBuf::from("mods").join("shaders-only.jar"), PathBuf::from("overrides")]);
        assert!(tasks.iter().all(|t| t.folder.as_deref() == Some(Path::new("Tiny Pack"))));
        // A SHA-512 that is not one leaves the SHA-1.
        assert_eq!(tasks[0].checksum.as_deref(), Some("sha1:0000000000000000000000000000000000000001"));
        assert_eq!(tasks[2].urls[0].as_str(), format!("modpack:http://{addr}/tiny.mrpack#overrides,client-overrides"));
        let server = pack(&http(), &link, &terms(&["server"]), &options).await.unwrap();
        assert_eq!(server.len(), 2, "the client-only file is left out");
        assert!(server[1].urls[0].as_str().ends_with("#overrides,server-overrides"));

        // The overrides task unpacks the shared and the server's folders into the pack's folder.
        let dir = tempfile::tempdir().unwrap();
        let (done, fresh) = run(&server[1].urls[0], &http(), None, dir.path(), None, CancellationToken::new()).await.unwrap();
        assert!(fresh && done == dir.path());
        assert_eq!(std::fs::read(dir.path().join("config").join("a.toml")).unwrap(), b"x = 1");
        assert_eq!(std::fs::read(dir.path().join("server.properties")).unwrap(), b"motd=hi");
        assert!(hits.lock().get("/tiny.mrpack").is_some());
    }

    /// CurseForge answers with the user's key: a file link is that file with its SHA-1, a file its
    /// author keeps to CurseForge's site is the page to open, and a modpack's manifest lists its
    /// files into the folders their classes go in.
    #[tokio::test]
    async fn curseforge_files_and_modpacks_list_through_its_api() {
        let manifest = br#"{"manifestType":"minecraftModpack","name":"Big Pack","overrides":"overrides","files":[
            {"projectID":1,"fileID":11,"required":true},{"projectID":2,"fileID":22,"required":true},{"projectID":3,"fileID":33,"required":false}]}"#;
        let pack = zip(&[("manifest.json", &manifest[..]), ("overrides/options.txt", b"fov:90")]);
        let host = Arc::new(std::sync::OnceLock::<String>::new());
        let named = Arc::clone(&host);
        let (addr, _) = serve(move |path, range| match path {
            // The batch lookups are POSTs: the canned server answers by path alone, all it knows.
            "/v1/mods/files" => json(
                &r#"{"data":[{"id":5,"modId":9,"fileName":"jei.jar","fileLength":42,"downloadUrl":"https://edge.forgecdn.net/files/5/jei.jar",
                    "hashes":[{"value":"ABCDEF0123456789ABCDEF0123456789ABCDEF01","algo":1},{"value":"00","algo":2}]},
                {"id":6,"modId":9,"fileName":"kept.jar","downloadUrl":null,"hashes":[]},
                {"id":7,"modId":4,"fileName":"Big Pack-1.zip","downloadUrl":"http://HOST/pack.zip","hashes":[]},
                {"id":11,"modId":1,"fileName":"lib.jar","downloadUrl":"https://edge.forgecdn.net/files/11/lib.jar","hashes":[]},
                {"id":22,"modId":2,"fileName":"look.zip","downloadUrl":"https://edge.forgecdn.net/files/22/look.zip","hashes":[]}]}"#
                    .replace("HOST", named.get().unwrap()),
            ),
            "/v1/mods" => json(r#"{"data":[{"id":1,"classId":6,"links":{"websiteUrl":"https://www.curseforge.com/minecraft/mc-mods/lib"}},{"id":2,"classId":12,"links":{}}]}"#),
            "/pack.zip" => part(&pack, range),
            _ => missing(),
        })
        .await;
        host.set(addr.to_string()).unwrap();
        let api = format!("http://{addr}/v1");
        let cf = CurseForge { http: &http(), api: &api, key: "key" };
        let (options, pick) = (ListOptions::default(), Pick::default());
        let [jar] = &cf.list("minecraft", "mc-mods", "jei", Some(5), &pick, &options).await.unwrap()[..] else { panic!("one file") };
        assert_eq!((jar.urls[0].as_str(), jar.size), ("https://edge.forgecdn.net/files/5/jei.jar", Some(42)));
        assert_eq!(jar.checksum.as_deref(), Some("sha1:abcdef0123456789abcdef0123456789abcdef01"));
        let kept = cf.list("minecraft", "mc-mods", "kept", Some(6), &pick, &options).await.unwrap_err();
        assert!(kept.contains("open https://www.curseforge.com/minecraft/mc-mods/kept/files/6 in the browser"), "{kept}");
        assert_eq!(cf.list("minecraft", "mc-mods", "x", Some(8), &pick, &options).await.unwrap_err(), "CurseForge has no file 8");
        let tasks = cf.list("minecraft", "modpacks", "big-pack", Some(7), &pick, &options).await.unwrap();
        let names: Vec<_> = tasks.iter().map(|t| t.name.clone().unwrap()).collect();
        let into = |dir: &str, file: &str| PathBuf::from(dir).join(file);
        assert_eq!(names, [into("mods", "lib.jar"), into("resourcepacks", "look.zip"), PathBuf::from("overrides")]);
        assert!(tasks.iter().all(|t| t.folder.as_deref() == Some(Path::new("Big Pack"))));
        assert_eq!(tasks[2].urls[0].as_str(), format!("modpack:http://{addr}/pack.zip#overrides"));
        let keyless = list(&http(), &Url::parse("https://www.curseforge.com/minecraft/mc-mods/jei").unwrap(), &ListOptions::default()).await.unwrap_err();
        assert_eq!(keyless, CURSEFORGE_KEY);
    }

    /// A Thunderstore package lists its dependencies' packages too, each once, at the versions it
    /// names; a missing one is a note.
    #[tokio::test]
    async fn thunderstore_packages_bring_their_dependencies() {
        let (addr, _) = serve(|path, _| match path {
            "/api/experimental/package/Team/Pack/" => json(
                r#"{"latest":{"full_name":"Team-Pack-2.0.0","download_url":"https://thunderstore.io/package/download/Team/Pack/2.0.0/",
                    "dependencies":["bbepis-BepInExPack-5.4.2100","Some-Team-Lib-1.0.0","Gone-Away-1.0.0"]}}"#,
            ),
            "/api/experimental/package/bbepis/BepInExPack/5.4.2100/" => {
                json(r#"{"full_name":"bbepis-BepInExPack-5.4.2100","download_url":"https://thunderstore.io/package/download/bbepis/BepInExPack/5.4.2100/","dependencies":[]}"#)
            }
            "/api/experimental/package/Some-Team/Lib/1.0.0/" => json(
                r#"{"full_name":"Some-Team-Lib-1.0.0","download_url":"https://thunderstore.io/package/download/Some-Team/Lib/1.0.0/","dependencies":["bbepis-BepInExPack-5.4.1801"]}"#,
            ),
            _ => missing(),
        })
        .await;
        let (tx, notes) = std::sync::mpsc::channel();
        let options = ListOptions { notes: Some(tx), ..ListOptions::default() };
        let tasks = thunderstore(&http(), &format!("http://{addr}"), "Team", "Pack", None, &options).await.unwrap();
        let names: Vec<_> = tasks.iter().map(|t| t.name.clone().unwrap()).collect();
        let zips = ["Team-Pack-2.0.0.zip", "bbepis-BepInExPack-5.4.2100.zip", "Some-Team-Lib-1.0.0.zip"].map(PathBuf::from);
        assert_eq!(names, zips);
        assert!(tasks.iter().all(|t| t.folder.as_deref() == Some(Path::new("Team-Pack-2.0.0"))));
        assert!(notes.try_iter().any(|n| n.text.contains("Gone-Away-1.0.0")));
        assert_eq!(thunderstore(&http(), &format!("http://{addr}"), "No", "Such", Some("1.0.0"), &options).await.unwrap_err(), "Thunderstore has no package No-Such 1.0.0");
    }

    /// Nexus Mods lists a mod's main files; a file its API gives no link (no premium) is its page
    /// to open, and an nxm link's key and expiry go with the request for its link.
    #[tokio::test]
    async fn nexus_files_need_a_link_from_its_api() {
        let (addr, hits) = serve(|path, _| match path {
            "/v1/games/skyrim/mods/266/files.json" => json(
                r#"{"files":[{"file_id":1,"file_name":"Main-1.7z","category_name":"MAIN","size_in_bytes":100},
                    {"file_id":2,"file_name":"Old.7z","category_name":"OLD_VERSION"},{"file_id":3,"file_name":"Main-2.7z","category_name":"MAIN"}]}"#,
            ),
            "/v1/games/skyrim/mods/266/files/1/download_link.json" => json(r#"[{"name":"Nexus CDN","short_name":"Nexus CDN","URI":"https://cf-files.nexus-cdn.com/1/Main-1.7z?md5=x"}]"#),
            "/v1/games/skyrim/mods/266/files/3/download_link.json" => (403, String::new(), br#"{"code":403,"message":"premium only"}"#.to_vec()),
            "/v1/games/skyrim/mods/266/files/3/download_link.json?key=k&expires=9" => json(r#"[{"URI":"https://cf-files.nexus-cdn.com/3/Main-2.7z"}]"#),
            _ => missing(),
        })
        .await;
        let api = format!("http://{addr}/v1");
        let (tx, notes) = std::sync::mpsc::channel();
        let options = ListOptions { notes: Some(tx), ..ListOptions::default() };
        let [main] = &nexus(&http(), &api, "key", "skyrim", 266, None, None, &options).await.unwrap()[..] else { panic!("one file has a link") };
        assert_eq!((main.name.clone().unwrap(), main.size), (PathBuf::from("Main-1.7z"), Some(100)));
        assert!(notes.try_iter().any(|n| n.text.contains("https://www.nexusmods.com/skyrim/mods/266?tab=files&file_id=3")));
        let premium = nexus(&http(), &api, "key", "skyrim", 266, Some(3), None, &options).await.unwrap_err();
        assert!(premium.starts_with("Nexus Mods gives other apps download links only for premium accounts"), "{premium}");
        let [nxm] = &nexus(&http(), &api, "key", "skyrim", 266, Some(3), Some(("k", "9")), &options).await.unwrap()[..] else { panic!("the nxm key gives a link") };
        assert_eq!(nxm.urls[0].as_str(), "https://cf-files.nexus-cdn.com/3/Main-2.7z");
        assert!(hits.lock().get("/v1/games/skyrim/mods/266/files/2/download_link.json").is_none(), "an old file is not asked for");
        let keyless = list(&http(), &Url::parse("https://www.nexusmods.com/skyrim/mods/266").unwrap(), &ListOptions::default()).await.unwrap_err();
        assert_eq!(keyless, NEXUS_KEY);
    }
}
