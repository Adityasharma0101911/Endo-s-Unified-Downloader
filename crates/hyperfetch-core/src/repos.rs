//! Code and model repositories (GitHub, Hugging Face, Civitai): their folders, releases and
//! models read into one task per file, a repository itself into its default branch's zip.
//!
//! The files download from the service's own host the user typed (github.com, huggingface.co,
//! civitai.com), so the Authorization header the user gave goes with them there and nowhere
//! else. The listings are read without it: a private or gated repository cannot be listed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use percent_encoding::percent_decode_str;
use reqwest::header::LINK;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use crate::ingest::{clean_path, Task};
use crate::resolver::ResolverError;

const GITHUB: &str = "https://github.com";
const GITHUB_API: &str = "https://api.github.com";
const HUGGING_FACE: &str = "https://huggingface.co";
const CIVITAI: &str = "https://civitai.com";

/// Most path segments a GitHub ref with slashes in it (`feature/x`) is tried with.
const MAX_REF_PARTS: usize = 4;
/// Most pages of one Hugging Face listing (1000 files each).
const MAX_PAGES: usize = 100;

/// First path segments of github.com that are its own pages, not owners.
const GITHUB_PAGES: &[&str] = &[
    "about", "account", "apps", "collections", "codespaces", "contact", "customer-stories", "dashboard", "enterprise",
    "events", "explore", "features", "issues", "join", "login", "logout", "marketplace", "new", "notifications",
    "organizations", "orgs", "pricing", "pulls", "readme", "search", "security", "sessions", "settings", "signup",
    "site", "sponsors", "topics", "trending", "users",
];

/// First path segments of huggingface.co that are its own pages, not owners of models.
const HUGGING_FACE_PAGES: &[&str] = &[
    "api", "blog", "changelog", "chat", "collections", "docs", "enterprise", "inference-endpoints", "join", "learn",
    "login", "models", "new", "oauth", "organizations", "papers", "posts", "pricing", "privacy", "settings", "tasks",
    "terms-of-service",
];

const LISTS_MANY: &str = "this link lists several files: add it as a new download to get one download per file";

/// What a repository link is, from its shape.
#[derive(Debug, PartialEq)]
enum Repo {
    /// github.com/{owner}/{repo}
    GitHubRoot { owner: String, repo: String },
    /// github.com/{owner}/{repo}/tree/{ref}/{path}, the ref and path in `rest`: a ref may hold
    /// slashes too.
    GitHubTree { owner: String, repo: String, rest: Vec<String> },
    /// github.com/{owner}/{repo}/releases/latest, or /releases/tag/{tag}.
    GitHubRelease { owner: String, repo: String, tag: Option<String> },
    /// huggingface.co[/datasets|/spaces]/{repo}[/tree/{rev}/{path}]: `kind` is the API's
    /// "models", "datasets" or "spaces", `repo` is {owner}/{name} or an older repo's {name}.
    HuggingFace { kind: &'static str, repo: String, rev: String, path: Vec<String> },
    /// civitai.com/models/{id}[?modelVersionId={version}]
    Civitai { model: u64, version: Option<u64> },
}

fn repo_of(url: &Url) -> Option<Repo> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let decoded: Vec<String> =
        url.path_segments()?.filter(|s| !s.is_empty()).map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let segs: Vec<&str> = decoded.iter().map(String::as_str).collect();
    let owned = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match url.host_str()?.trim_end_matches('.') {
        "github.com" | "www.github.com" => {
            let (owner, repo) = (*segs.first()?, segs.get(1)?.trim_end_matches(".git"));
            if GITHUB_PAGES.contains(&owner) || repo.is_empty() {
                return None;
            }
            let (owner, repo) = (owner.to_string(), repo.to_string());
            match segs[2..] {
                [] => Some(Repo::GitHubRoot { owner, repo }),
                ["tree", ref rest @ ..] if !rest.is_empty() => Some(Repo::GitHubTree { owner, repo, rest: owned(rest) }),
                ["releases", "latest"] => Some(Repo::GitHubRelease { owner, repo, tag: None }),
                ["releases", "tag", tag] => Some(Repo::GitHubRelease { owner, repo, tag: Some(tag.to_string()) }),
                _ => None,
            }
        }
        "huggingface.co" | "www.huggingface.co" | "hf.co" => {
            let (kind, segs) = match segs[..] {
                ["datasets", ref rest @ ..] => ("datasets", rest),
                ["spaces", ref rest @ ..] => ("spaces", rest),
                ref rest => ("models", rest),
            };
            if kind == "models" && segs.first().is_some_and(|s| HUGGING_FACE_PAGES.contains(s)) {
                return None;
            }
            let (repo, rev, path) = match segs {
                [owner, name, "tree", rev, path @ ..] => (format!("{owner}/{name}"), *rev, path),
                [name, "tree", rev, path @ ..] => (name.to_string(), *rev, path),
                [owner, name] => (format!("{owner}/{name}"), "main", &[][..]),
                _ => return None,
            };
            Some(Repo::HuggingFace { kind, repo, rev: rev.to_string(), path: owned(path) })
        }
        "civitai.com" | "www.civitai.com" => match segs[..] {
            ["models", id, ..] => {
                let version = url.query_pairs().find(|(k, _)| k == "modelVersionId").and_then(|(_, v)| v.parse().ok());
                Some(Repo::Civitai { model: id.parse().ok()?, version })
            }
            _ => None,
        },
        _ => None,
    }
}

/// Whether `url` is a repository, folder, release or model link [`list`] reads.
pub fn lists(url: &Url) -> bool {
    repo_of(url).is_some()
}

/// One task per file at `url`, in a folder named after it; a repository's zip.
pub async fn list(http: &reqwest::Client, url: &Url) -> Result<Vec<Task>, String> {
    let tasks = match repo_of(url) {
        Some(Repo::GitHubRoot { owner, repo }) => {
            vec![Task { urls: vec![zip_of(&owner, &repo)?], name: Some(component(&format!("{repo}.zip"), "repository.zip")?), ..Task::default() }]
        }
        Some(Repo::GitHubTree { owner, repo, rest }) => github_tree(http, GITHUB_API, &owner, &repo, &rest).await?,
        Some(Repo::GitHubRelease { owner, repo, tag }) => github_release(http, GITHUB_API, &owner, &repo, tag.as_deref()).await?,
        Some(Repo::HuggingFace { kind, repo, rev, path }) => hugging_face(http, HUGGING_FACE, kind, &repo, &rev, &path).await?,
        Some(Repo::Civitai { model, version }) => civitai(http, CIVITAI, model, version).await?,
        None => return Err(format!("{} is not a repository, folder, release or model link", url)),
    };
    if tasks.is_empty() {
        return Err("the link holds no files the app can download".to_string());
    }
    Ok(tasks)
}

/// Whether `url` is a repository link [`resolve`] takes (a folder's says it must be listed).
pub fn handles(url: &Url) -> bool {
    repo_of(url).is_some()
}

/// Where the zip of the GitHub repository link `url` downloads from.
pub async fn resolve(_client: &reqwest::Client, url: &Url, _proxy: Option<&str>) -> Result<Vec<Url>, ResolverError> {
    match repo_of(url) {
        Some(Repo::GitHubRoot { owner, repo }) => zip_of(&owner, &repo).map(|zip| vec![zip]).map_err(ResolverError::Parse),
        Some(_) => Err(ResolverError::NotFound(LISTS_MANY.to_string())),
        None => Err(ResolverError::NotFound(format!("{} is not a repository link", url))),
    }
}

/// The zip of the default branch of GitHub's `owner`/`repo`, without asking its API.
fn zip_of(owner: &str, repo: &str) -> Result<Url, String> {
    url_at(GITHUB, [owner, repo, "archive", "HEAD.zip"])
}

/// `base` with `segments` added to its path, each percent-encoded as one segment.
fn url_at<'a>(base: &str, segments: impl IntoIterator<Item = &'a str>) -> Result<Url, String> {
    let mut url = Url::parse(base).map_err(|e| e.to_string())?;
    url.path_segments_mut().map_err(|_| format!("{} takes no path", base))?.pop_if_empty().extend(segments);
    Ok(url)
}

/// Whether `url` is on the same scheme, host and port as `base`.
fn own(url: &Url, base: &str) -> bool {
    Url::parse(base).is_ok_and(|base| base.origin() == url.origin())
}

/// A file or folder name made safe as one path component, else `fallback`.
fn component(name: &str, fallback: &str) -> Result<PathBuf, String> {
    clean_path([name]).or_else(|_| clean_path([fallback]))
}

/// `path` (a listing's, so untrusted) as a relative path whose every part is a usable name;
/// None leaves the file out (a part like "..", or an absolute path).
fn relative(path: &str) -> Option<PathBuf> {
    let clean = clean_path(path.split('/')).ok();
    if clean.is_none() {
        tracing::warn!("left out {:?}: not a usable file name", path);
    }
    clean
}

fn checksum(algo: &str, hex: &str) -> Option<String> {
    Some(format!("{}:{}", algo, hex.to_ascii_lowercase())).filter(|c| crate::storage::validate_checksum(c).is_ok())
}

/// What `request` to `service` answers when it succeeds; None when there is no such thing (404).
async fn fetch(request: RequestBuilder, service: &str) -> Result<Option<Response>, String> {
    let answer = request.send().await.map_err(|e| format!("Cannot reach {}: {}", service, e))?;
    let status = answer.status();
    let number = |name: &str| answer.headers().get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN && (number("x-ratelimit-remaining") == Some(0) || number("retry-after").is_some()));
    if status.is_success() {
        Ok(Some(answer))
    } else if status == StatusCode::NOT_FOUND {
        Ok(None)
    } else if limited && service == "GitHub" {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let wait = number("x-ratelimit-reset").map(|reset| reset.saturating_sub(now)).or_else(|| number("retry-after"));
        let when = match wait.map(|s| s.div_ceil(60).max(1)) {
            Some(1) => "in a minute".to_string(),
            Some(minutes) => format!("in {} minutes", minutes),
            None => "later".to_string(),
        };
        Err(format!("GitHub's limit of 60 requests an hour without signing in is used up: try again {}", when))
    } else if limited {
        Err(format!("{} is limiting requests: try again in a few minutes", service))
    } else if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        Err(format!("{} refused the listing (HTTP {}): the repository may be private or gated", service, status.as_u16()))
    } else {
        Err(format!("{} answered HTTP {}", service, status.as_u16()))
    }
}

async fn json<T: DeserializeOwned>(answer: Response, service: &str) -> Result<T, String> {
    let body = answer.bytes().await.map_err(|e| format!("Cannot reach {}: {}", service, e))?;
    serde_json::from_slice(&body).map_err(|_| format!("{} answered with something the app cannot read", service))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct GitTree {
    tree: Vec<GitEntry>,
    truncated: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct GitEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    size: Option<u64>,
}

/// Every file under the folder `rest` (its ref, then its path) names in GitHub's `owner`/`repo`,
/// subfolders kept, each a task for its /raw/ link (which serves Git LFS files too). The ref is
/// the shortest start of `rest` GitHub knows.
async fn github_tree(http: &reqwest::Client, api: &str, owner: &str, repo: &str, rest: &[String]) -> Result<Vec<Task>, String> {
    for parts in 1..=rest.len().min(MAX_REF_PARTS) {
        let (git_ref, path) = (rest[..parts].join("/"), &rest[parts..]);
        let tree_ish = if path.is_empty() { git_ref.clone() } else { format!("{}:{}", git_ref, path.join("/")) };
        let mut url = url_at(api, ["repos", owner, repo, "git", "trees", &tree_ish])?;
        url.set_query(Some("recursive=1"));
        let Some(answer) = fetch(http.get(url), "GitHub").await? else { continue };
        let tree: GitTree = json(answer, "GitHub").await?;
        if tree.truncated {
            return Err("the folder holds more files than GitHub lists at once: add the links of its subfolders instead".to_string());
        }
        let folder = component(path.last().map_or(repo, String::as_str), repo)?;
        let mut tasks = Vec::new();
        for entry in tree.tree.iter().filter(|e| e.kind == "blob") {
            let Some(name) = relative(&entry.path) else { continue };
            let raw = [owner, repo, "raw"].into_iter().chain(git_ref.split('/')).chain(path.iter().map(String::as_str));
            let url = url_at(GITHUB, raw.chain(entry.path.split('/')))?;
            tasks.push(Task { urls: vec![url], name: Some(name), folder: Some(folder.clone()), size: entry.size, ..Task::default() });
        }
        return Ok(tasks);
    }
    Err(format!("GitHub has no such folder in {}/{} (or the repository is private)", owner, repo))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Asset {
    name: String,
    size: Option<u64>,
    browser_download_url: String,
    /// "sha256:<hex>", for assets uploaded since GitHub keeps it.
    digest: Option<String>,
}

/// Every file of the release `tag` (None the latest) of GitHub's `owner`/`repo`.
async fn github_release(http: &reqwest::Client, api: &str, owner: &str, repo: &str, tag: Option<&str>) -> Result<Vec<Task>, String> {
    let url = match tag {
        Some(tag) => url_at(api, ["repos", owner, repo, "releases", "tags", tag])?,
        None => url_at(api, ["repos", owner, repo, "releases", "latest"])?,
    };
    let Some(answer) = fetch(http.get(url), "GitHub").await? else {
        return Err(format!("{}/{} has no such release on GitHub (or the repository is private)", owner, repo));
    };
    let release: Release = json(answer, "GitHub").await?;
    let folder = component(&format!("{} {}", repo, release.tag_name), repo)?;
    let tasks: Vec<Task> = release
        .assets
        .iter()
        .filter_map(|asset| {
            let url = Url::parse(&asset.browser_download_url).ok()?;
            Some(Task {
                name: Some(relative(&asset.name)?),
                folder: Some(folder.clone()),
                size: asset.size,
                checksum: asset.digest.as_deref().and_then(|d| d.split_once(':')).and_then(|(algo, hex)| checksum(algo, hex)),
                // Only GitHub's own host gets the user's Authorization.
                from_document: !own(&url, GITHUB),
                urls: vec![url],
                ..Task::default()
            })
        })
        .collect();
    if tasks.is_empty() {
        return Err(format!("the release {} has no files besides its source code: add the repository's link to get that", release.tag_name));
    }
    Ok(tasks)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct HfEntry {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    size: Option<u64>,
    lfs: Option<HfLfs>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct HfLfs {
    /// The SHA-256 of the file.
    oid: String,
}

/// The next page of a listing, from its Link header, when it is on `base` too.
fn next_page(answer: &Response, base: &str) -> Option<Url> {
    let link = answer.headers().get(LINK)?.to_str().ok()?;
    let next = link.split(',').find(|l| l.contains("rel=\"next\""))?;
    let url = Url::parse(base).ok()?.join(next.split_once('<')?.1.split_once('>')?.0).ok()?;
    own(&url, base).then_some(url)
}

/// Every file under `path` at revision `rev` of the Hugging Face repository `repo` of `kind`,
/// subfolders kept, each a task for its /resolve/ link with its size and (Git LFS files) SHA-256.
async fn hugging_face(http: &reqwest::Client, base: &str, kind: &str, repo: &str, rev: &str, path: &[String]) -> Result<Vec<Task>, String> {
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    let mut page = url_at(base, ["api", kind].into_iter().chain(repo.split('/')).chain(["tree", rev]).chain(path.iter().copied()))?;
    page.set_query(Some("recursive=true"));
    // Models are at the top of the site, datasets and spaces under their own path.
    let web: Vec<&str> = [kind].into_iter().filter(|k| *k != "models").chain(repo.split('/')).chain(["resolve", rev]).collect();
    let prefix = path.join("/");
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let folder = component(path.last().copied().unwrap_or(name), name)?;
    let mut tasks = Vec::new();
    for _ in 0..MAX_PAGES {
        let Some(answer) = fetch(http.get(page.clone()), "Hugging Face").await? else {
            return Err(format!("Hugging Face has no such folder at {} in {} (or the repository is private or gated)", rev, repo));
        };
        let next = next_page(&answer, base);
        let entries: Vec<HfEntry> = json(answer, "Hugging Face").await?;
        for entry in entries.iter().filter(|e| e.kind == "file") {
            // The API names files from the repository's top.
            let inside = if prefix.is_empty() { Some(entry.path.as_str()) } else { entry.path.strip_prefix(&prefix).and_then(|p| p.strip_prefix('/')) };
            let Some(name) = inside.and_then(relative) else { continue };
            tasks.push(Task {
                urls: vec![url_at(base, web.iter().copied().chain(entry.path.split('/')))?],
                name: Some(name),
                folder: Some(folder.clone()),
                size: entry.size,
                checksum: entry.lfs.as_ref().and_then(|lfs| checksum("sha256", &lfs.oid)),
                ..Task::default()
            });
        }
        match next {
            Some(next) => page = next,
            None => return Ok(tasks),
        }
    }
    Err(format!("the folder holds more than {} files: add the links of its subfolders instead", MAX_PAGES * 1000))
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Model {
    name: String,
    model_versions: Vec<ModelVersion>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ModelVersion {
    id: u64,
    name: String,
    files: Vec<ModelFile>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ModelFile {
    id: u64,
    name: String,
    download_url: String,
    hashes: HashMap<String, String>,
}

/// Every file of the version `version` (None the newest) of Civitai's model `model`.
async fn civitai(http: &reqwest::Client, api: &str, model: u64, version: Option<u64>) -> Result<Vec<Task>, String> {
    let url = url_at(api, ["api", "v1", "models", &model.to_string()])?;
    let Some(answer) = fetch(http.get(url), "Civitai").await? else {
        return Err(format!("Civitai has no model {}", model));
    };
    let found: Model = json(answer, "Civitai").await?;
    // The newest version is listed first.
    let chosen = match version {
        Some(version) => found.model_versions.iter().find(|v| v.id == version),
        None => found.model_versions.first(),
    };
    let chosen = chosen.ok_or_else(|| format!("the Civitai model {} has no such version", model))?;
    let folder = component(&format!("{} {}", found.name, chosen.name), &model.to_string())?;
    Ok(chosen
        .files
        .iter()
        .filter_map(|file| {
            let url = Url::parse(&file.download_url).ok()?;
            Some(Task {
                name: Some(component(&file.name, &file.id.to_string()).ok()?),
                folder: Some(folder.clone()),
                checksum: file.hashes.get("SHA256").and_then(|hex| checksum("sha256", hex)),
                // Only Civitai's own host gets the user's Authorization.
                from_document: !own(&url, CIVITAI),
                urls: vec![url],
                ..Task::default()
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{serve, Reply};

    fn repo(link: &str) -> Option<Repo> {
        repo_of(&Url::parse(link).unwrap())
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn strings(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn repository_links_are_told_by_their_shape() {
        let gh = |owner: &str, repo: &str| (owner.to_string(), repo.to_string());
        let (owner, name) = gh("o", "r");
        assert_eq!(repo("https://github.com/o/r"), Some(Repo::GitHubRoot { owner: owner.clone(), repo: name.clone() }));
        assert_eq!(repo("https://github.com/o/r.git/"), Some(Repo::GitHubRoot { owner: owner.clone(), repo: name.clone() }));
        assert_eq!(
            repo("https://github.com/o/r/tree/feature/x/My%20Docs"),
            Some(Repo::GitHubTree { owner: owner.clone(), repo: name.clone(), rest: strings(&["feature", "x", "My Docs"]) })
        );
        assert_eq!(repo("https://github.com/o/r/releases/latest"), Some(Repo::GitHubRelease { owner: owner.clone(), repo: name.clone(), tag: None }));
        assert_eq!(
            repo("https://github.com/o/r/releases/tag/v1.0%2Brc"),
            Some(Repo::GitHubRelease { owner, repo: name, tag: Some("v1.0+rc".into()) })
        );
        let hf = |kind, repo: &str, rev: &str, path: &[&str]| Some(Repo::HuggingFace { kind, repo: repo.into(), rev: rev.into(), path: strings(path) });
        assert_eq!(repo("https://huggingface.co/openai-community/gpt2"), hf("models", "openai-community/gpt2", "main", &[]));
        assert_eq!(repo("https://huggingface.co/o/m/tree/refs%2Fpr%2F1/onnx/x"), hf("models", "o/m", "refs/pr/1", &["onnx", "x"]));
        assert_eq!(repo("https://hf.co/gpt2/tree/main"), hf("models", "gpt2", "main", &[]));
        assert_eq!(repo("https://huggingface.co/datasets/stanfordnlp/imdb"), hf("datasets", "stanfordnlp/imdb", "main", &[]));
        assert_eq!(repo("https://huggingface.co/spaces/gradio/hello_world/tree/main/sub"), hf("spaces", "gradio/hello_world", "main", &["sub"]));
        assert_eq!(repo("https://civitai.com/models/4201/realistic-vision?modelVersionId=501240"), Some(Repo::Civitai { model: 4201, version: Some(501240) }));
        assert_eq!(repo("https://civitai.com/models/4201"), Some(Repo::Civitai { model: 4201, version: None }));
        for other in [
            // Files, which the code host resolver takes, and direct downloads.
            "https://github.com/o/r/blob/main/a.zip",
            "https://github.com/o/r/raw/main/a.zip",
            "https://github.com/o/r/releases/download/v1/a.zip",
            "https://github.com/o/r/releases/latest/download/a.zip",
            "https://github.com/o/r/archive/HEAD.zip",
            "https://github.com/o/r/tree",
            "https://github.com/o",
            "https://github.com/settings/tokens",
            "https://github.com/orgs/rust-lang",
            "https://huggingface.co/o/m/blob/main/model.safetensors",
            "https://huggingface.co/o/m/resolve/main/model.safetensors",
            "https://huggingface.co/docs/transformers",
            "https://huggingface.co/gpt2",
            "https://civitai.com/models/abc",
            "https://civitai.com/api/download/models/501240",
            "https://example.com/o/r",
            "ftp://github.com/o/r",
        ] {
            assert_eq!(repo(other), None, "{other}");
        }
        assert!(lists(&Url::parse("https://github.com/o/r").unwrap()) && handles(&Url::parse("https://civitai.com/models/1").unwrap()));
    }

    /// A repository is its default branch's zip, named after it, with no request made; any other
    /// repository link must be listed.
    #[tokio::test]
    async fn a_repository_is_its_default_branchs_zip() {
        let root = Url::parse("https://github.com/o/My%20Repo").unwrap();
        let zip = "https://github.com/o/My%20Repo/archive/HEAD.zip";
        let tasks = list(&http(), &root).await.unwrap();
        assert_eq!(tasks, [Task { urls: vec![Url::parse(zip).unwrap()], name: Some("My Repo.zip".into()), ..Task::default() }]);
        assert_eq!(resolve(&http(), &root, None).await.unwrap()[0].as_str(), zip);
        let tree = Url::parse("https://github.com/o/r/tree/main").unwrap();
        assert!(resolve(&http(), &tree, None).await.unwrap_err().to_string().contains("lists several files"));
    }

    fn json_reply(body: &str) -> Reply {
        (200, "Content-Type: application/json\r\n".into(), body.into())
    }

    fn github(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        match path {
            "/repos/o/r/git/trees/feature%2Fx:docs?recursive=1" => json_reply(
                r#"{"truncated":false,"tree":[
                    {"path":"a.md","type":"blob","size":3},
                    {"path":"img","type":"tree"},
                    {"path":"img/b c.png","type":"blob","size":5},
                    {"path":"vendor/lib","type":"commit"},
                    {"path":"img/../../../evil.exe","type":"blob"},
                    {"path":"/etc/passwd","type":"blob"},
                    {"path":"..","type":"blob"}]}"#,
            ),
            "/repos/o/big/git/trees/main?recursive=1" => json_reply(r#"{"truncated":true,"tree":[]}"#),
            "/repos/o/busy/git/trees/main?recursive=1" => {
                let reset = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 600;
                (403, format!("X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n"), b"{}".to_vec())
            }
            "/repos/o/r/releases/latest" => json_reply(
                r#"{"tag_name":"v2.0","assets":[
                    {"name":"app.zip","size":7,"browser_download_url":"https://github.com/o/r/releases/download/v2.0/app.zip",
                     "digest":"sha256:2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824"},
                    {"name":"mirror.zip","browser_download_url":"https://mirror.example/app.zip","digest":"sha256:nothex"},
                    {"name":"..","browser_download_url":"https://github.com/o/r/releases/download/v2.0/x"}]}"#,
            ),
            "/repos/o/r/releases/tags/empty" => json_reply(r#"{"tag_name":"empty","assets":[]}"#),
            _ => (404, String::new(), br#"{"message":"Not Found"}"#.to_vec()),
        }
    }

    /// A folder is listed through the trees API at the ref GitHub knows (the shortest start of the
    /// rest of the link), each file its /raw/ link, subfolders kept, names that would leave the
    /// folder left out. A tree too big, the rate limit and a missing folder are said plainly.
    #[tokio::test]
    async fn github_folders_are_listed_with_their_ref() {
        let (addr, hits) = serve(github).await;
        let api = format!("http://{addr}");
        let tasks = github_tree(&http(), &api, "o", "r", &strings(&["feature", "x", "docs"])).await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap(), t.size)).collect();
        assert_eq!(
            listed,
            [
                ("https://github.com/o/r/raw/feature/x/docs/a.md".to_string(), PathBuf::from("a.md"), Some(3)),
                ("https://github.com/o/r/raw/feature/x/docs/img/b%20c.png".to_string(), PathBuf::from("img").join("b c.png"), Some(5)),
            ]
        );
        assert!(tasks.iter().all(|t| t.folder == Some("docs".into()) && !t.from_document));
        // "feature" was tried first.
        assert_eq!(hits.lock().get("/repos/o/r/git/trees/feature:x%2Fdocs?recursive=1"), Some(&1));
        let err = github_tree(&http(), &api, "o", "big", &strings(&["main"])).await.unwrap_err();
        assert!(err.contains("subfolders"), "{err}");
        let err = github_tree(&http(), &api, "o", "busy", &strings(&["main"])).await.unwrap_err();
        assert!(err.contains("60 requests an hour") && err.contains("in 10 minutes"), "{err}");
        let err = github_tree(&http(), &api, "o", "r", &strings(&["nope", "docs"])).await.unwrap_err();
        assert!(err.contains("no such folder in o/r"), "{err}");
    }

    /// A release is one task per asset, under the repository and tag, with GitHub's digest; an
    /// asset on another host is listed without the user's Authorization, one named ".." not at all.
    #[tokio::test]
    async fn github_releases_list_their_assets() {
        let (addr, _) = serve(github).await;
        let api = format!("http://{addr}");
        let tasks = github_release(&http(), &api, "o", "r", None).await.unwrap();
        let sha = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string();
        let listed: Vec<_> = tasks.iter().map(|t| (t.name.clone().unwrap(), t.checksum.clone(), t.from_document)).collect();
        assert_eq!(listed, [(PathBuf::from("app.zip"), Some(sha), false), (PathBuf::from("mirror.zip"), None, true)]);
        assert!(tasks.iter().all(|t| t.folder == Some("r v2.0".into())));
        assert!(github_release(&http(), &api, "o", "r", Some("empty")).await.unwrap_err().contains("source code"));
        assert!(github_release(&http(), &api, "o", "r", Some("gone")).await.unwrap_err().contains("no such release"));
    }

    fn hugging(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        match (path, query) {
            ("/api/datasets/o/d/tree/main/sub", "recursive=true") => (
                200,
                "Link: </api/datasets/o/d/tree/main/sub?recursive=true&cursor=2>; rel=\"next\"\r\n".into(),
                br#"[{"type":"directory","path":"sub/deep"},
                     {"type":"file","path":"sub/a.bin","size":5,"lfs":{"oid":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}},
                     {"type":"file","path":"subway/x.bin"},
                     {"type":"file","path":"sub/../../evil.bin"}]"#
                    .to_vec(),
            ),
            ("/api/datasets/o/d/tree/main/sub", "recursive=true&cursor=2") => (
                200,
                "Link: <https://elsewhere.example/api/x>; rel=\"next\"\r\n".into(),
                br#"[{"type":"file","path":"sub/deep/b.txt","size":2}]"#.to_vec(),
            ),
            ("/api/models/o/gated/tree/main", _) => (401, String::new(), br#"{"error":"Access denied"}"#.to_vec()),
            ("/api/models/o/busy/tree/main", _) => (429, String::new(), Vec::new()),
            _ => (404, String::new(), br#"{"error":"Invalid rev id"}"#.to_vec()),
        }
    }

    /// A folder is listed from the top of the repository down through every page on the site's
    /// own host, each file its /resolve/ link with its LFS SHA-256; files outside it, and names
    /// that would leave it, are left out.
    #[tokio::test]
    async fn hugging_face_folders_are_listed_page_by_page() {
        let (addr, hits) = serve(hugging).await;
        let base = format!("http://{addr}");
        let tasks = hugging_face(&http(), &base, "datasets", "o/d", "main", &strings(&["sub"])).await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap(), t.checksum.clone())).collect();
        let sha = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string();
        assert_eq!(
            listed,
            [
                (format!("{base}/datasets/o/d/resolve/main/sub/a.bin"), PathBuf::from("a.bin"), Some(sha)),
                (format!("{base}/datasets/o/d/resolve/main/sub/deep/b.txt"), PathBuf::from("deep").join("b.txt"), None),
            ]
        );
        assert!(tasks.iter().all(|t| t.folder == Some("sub".into()) && !t.from_document));
        assert_eq!(hits.lock().len(), 2, "the page on another host was not asked for");
        let say = |repo: &'static str| {
            let base = base.clone();
            async move { hugging_face(&http(), &base, "models", repo, "main", &[]).await.unwrap_err() }
        };
        assert!(say("o/gated").await.contains("private or gated"));
        assert!(say("o/busy").await.contains("limiting requests"));
        assert!(say("o/gone").await.contains("no such folder at main in o/gone"));
    }

    fn civitai_api(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        match path {
            "/api/v1/models/7" => json_reply(
                r#"{"name":"Model: X","modelVersions":[
                    {"id":2,"name":"v2","files":[{"id":20,"name":"x-v2.safetensors","downloadUrl":"https://civitai.com/api/download/models/2?fileId=20",
                      "hashes":{"SHA256":"2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824","CRC32":"1"}}]},
                    {"id":1,"name":"v1","files":[
                      {"id":10,"name":"x-v1.safetensors","downloadUrl":"https://civitai.com/api/download/models/1?fileId=10"},
                      {"id":11,"name":"..","downloadUrl":"https://cdn.example/x"}]}]}"#,
            ),
            _ => (404, String::new(), br#"{"error":"No model with id"}"#.to_vec()),
        }
    }

    /// A model is the files of the version its link names, else of the newest one, under the
    /// model's and version's names; a file on another host goes without the user's Authorization.
    #[tokio::test]
    async fn civitai_models_list_a_versions_files() {
        let (addr, _) = serve(civitai_api).await;
        let api = format!("http://{addr}");
        let newest = civitai(&http(), &api, 7, None).await.unwrap();
        let sha = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string();
        assert_eq!(newest.len(), 1);
        assert_eq!(newest[0].urls[0].as_str(), "https://civitai.com/api/download/models/2?fileId=20");
        assert_eq!((newest[0].checksum.clone(), newest[0].from_document), (Some(sha), false));
        assert_eq!(newest[0].folder, Some("Model_ X v2".into()));
        let v1 = civitai(&http(), &api, 7, Some(1)).await.unwrap();
        let listed: Vec<_> = v1.iter().map(|t| (t.name.clone().unwrap(), t.from_document)).collect();
        assert_eq!(listed, [(PathBuf::from("x-v1.safetensors"), false), (PathBuf::from("11"), true)]);
        assert!(civitai(&http(), &api, 7, Some(9)).await.unwrap_err().contains("no such version"));
        assert!(civitai(&http(), &api, 8, None).await.unwrap_err().contains("no model 8"));
    }

    /// What `link` lists, through the client the front ends list with (GitHub wants its user agent).
    async fn live(link: &str) -> Result<Vec<Task>, String> {
        list(&crate::ingest::descriptor_client(None).unwrap(), &Url::parse(link).unwrap()).await
    }

    /// Live: a small public GitHub folder lists its files.
    #[tokio::test]
    #[ignore]
    async fn live_github_folder() {
        let tasks = live("https://github.com/github/gitignore/tree/main/Global").await.unwrap();
        assert!(tasks.iter().any(|t| t.name == Some("Windows.gitignore".into())), "{tasks:?}");
    }

    /// Live: a GitHub release lists its assets.
    #[tokio::test]
    #[ignore]
    async fn live_github_release() {
        let tasks = live("https://github.com/BurntSushi/ripgrep/releases/tag/14.1.0").await.unwrap();
        assert!(tasks.iter().any(|t| t.urls[0].as_str().ends_with(".zip")), "{tasks:?}");
    }

    /// Live: a tiny Hugging Face model lists its files with their LFS checksums.
    #[tokio::test]
    #[ignore]
    async fn live_hugging_face_model() {
        let tasks = live("https://huggingface.co/hf-internal-testing/tiny-random-gpt2").await.unwrap();
        let model = tasks.iter().find(|t| t.name == Some("model.safetensors".into())).unwrap();
        assert!(model.checksum.as_deref().is_some_and(|c| c.starts_with("sha256:")), "{model:?}");
    }

    /// Live: a Civitai model lists its newest version's files.
    #[tokio::test]
    #[ignore]
    async fn live_civitai_model() {
        let tasks = live("https://civitai.com/models/4201").await.unwrap();
        assert!(tasks.iter().all(|t| t.urls[0].as_str().starts_with("https://civitai.com/api/download/models/")), "{tasks:?}");
    }
}
