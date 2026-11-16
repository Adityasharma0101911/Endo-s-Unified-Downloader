//! Code and model repositories (GitHub, GitLab, Bitbucket, Gitea and Forgejo, Hugging Face,
//! Civitai), the Internet Archive and package registries: their folders, releases, models, items
//! and packages read into one task per file, a repository itself into its default branch's zip.
//!
//! The files download from the service's own host the user typed (github.com, huggingface.co,
//! civitai.com, archive.org, the GitLab or Gitea site, bitbucket.org), so the Authorization header the user gave goes with them there and nowhere
//! else. A listing is read with the Authorization a paste gave its link (a `ghp_`/`hf_` token, say), sent
//! to that service's API alone, so a private or gated repository can be listed; without one it cannot.

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
pub(crate) const ARCHIVE: &str = "https://archive.org";

/// Formats of the files archive.org makes of an item for itself (its records, thumbnail and
/// tile), which it lists as the item's own.
const ARCHIVE_OWN_FORMATS: &[&str] = &["Metadata", "JPEG Thumb", "Item Image"];

const BITBUCKET: &str = "https://bitbucket.org";
const BITBUCKET_API: &str = "https://api.bitbucket.org/2.0";
const PYPI: &str = "https://pypi.org";
const NPM_REGISTRY: &str = "https://registry.npmjs.org";
const CRATES_IO: &str = "https://crates.io";
/// Where crates.io's index says its .crate files are (`dl` in its config.json).
const CRATE_FILES: &str = "https://static.crates.io/crates";

/// Most path segments a GitHub ref with slashes in it (`feature/x`) is tried with.
const MAX_REF_PARTS: usize = 4;
/// Most pages of one Hugging Face, GitLab, Bitbucket or Gitea listing.
const MAX_PAGES: usize = 100;

/// First path segments of gitlab.com, bitbucket.org and Gitea's sites that are their own pages,
/// not namespaces or owners.
const CODE_HOST_PAGES: &[&str] = &[
    "account", "admin", "api", "assets", "blog", "dashboard", "explore", "groups", "help", "issues", "notifications",
    "org", "product", "profile", "projects", "pulls", "repo", "search", "site", "snippets", "user", "users", "workspace",
];

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
    /// archive.org/details/{item}, /download/{item}, or a folder of it, /download/{item}/{path}/.
    Archive { item: String, path: Vec<String> },
    /// A GitLab project, {namespace...}/{project} at `origin` (gitlab.com, or a host named gitlab.*).
    GitLabRoot { origin: String, project: String },
    /// {project}/-/tree/{ref}/{path} on any host (`/-/` is GitLab's), the ref and path in `rest`.
    GitLabTree { origin: String, project: String, rest: Vec<String> },
    /// {project}/-/releases/{tag}, or /-/releases/permalink/latest.
    GitLabRelease { origin: String, project: String, tag: Option<String> },
    /// bitbucket.org/{workspace}/{repo}
    BitbucketRoot { owner: String, repo: String },
    /// bitbucket.org/{workspace}/{repo}/src/{ref}[/{path}]/: a folder's link ends in a slash.
    BitbucketTree { owner: String, repo: String, rest: Vec<String> },
    /// bitbucket.org/{workspace}/{repo}/downloads/, the files uploaded beside the code.
    BitbucketDownloads { owner: String, repo: String },
    /// A Gitea or Forgejo repository, {owner}/{repo} at `origin` (codeberg.org, gitea.com, or a host
    /// named gitea.*).
    GiteaRoot { origin: String, owner: String, repo: String },
    /// {owner}/{repo}/src/{branch|tag|commit}/{ref}/{path} on any host: a folder or a file.
    GiteaSrc { origin: String, owner: String, repo: String, kind: String, rest: Vec<String> },
    /// {owner}/{repo}/releases/latest, or /releases/tag/{tag}, on a Gitea site as above.
    GiteaRelease { origin: String, owner: String, repo: String, tag: Option<String> },
    /// pypi.org/project/{name}/[{version}/]: the release's files, the latest release's without a version.
    PyPi { name: String, version: Option<String> },
    /// npmjs.com/package/[@{scope}/]{name}[/v/{version}]: the package's tarball, the latest's without a version.
    Npm { name: String, version: Option<String> },
    /// crates.io/crates/{name}[/{version}]: the crate's .crate file, the default version's without a version.
    Crate { name: String, version: Option<String> },
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
        "archive.org" | "www.archive.org" => match segs[..] {
            ["details" | "download", item] => Some(Repo::Archive { item: item.to_string(), path: Vec::new() }),
            // A file's link names no folder: only a folder's ends in a slash.
            ["download", item, ref path @ ..] if url.path().ends_with('/') => Some(Repo::Archive { item: item.to_string(), path: owned(path) }),
            _ => None,
        },
        "bitbucket.org" | "www.bitbucket.org" => {
            let (owner, repo) = (segs.first()?.to_string(), segs.get(1)?.to_string());
            if CODE_HOST_PAGES.contains(&owner.as_str()) {
                return None;
            }
            match segs[2..] {
                [] => Some(Repo::BitbucketRoot { owner, repo }),
                ["src", ref rest @ ..] if rest.len() == 1 || (rest.len() > 1 && url.path().ends_with('/')) => {
                    Some(Repo::BitbucketTree { owner, repo, rest: owned(rest) })
                }
                ["downloads"] => Some(Repo::BitbucketDownloads { owner, repo }),
                _ => None,
            }
        }
        "pypi.org" | "www.pypi.org" => match segs[..] {
            ["project", name] => Some(Repo::PyPi { name: name.into(), version: None }),
            ["project", name, version] => Some(Repo::PyPi { name: name.into(), version: Some(version.into()) }),
            _ => None,
        },
        "npmjs.com" | "www.npmjs.com" => {
            let (name, rest) = match segs[..] {
                ["package", scope, name, ref rest @ ..] if scope.starts_with('@') => (format!("{scope}/{name}"), rest),
                ["package", name, ref rest @ ..] => (name.to_string(), rest),
                _ => return None,
            };
            match rest {
                [] => Some(Repo::Npm { name, version: None }),
                ["v", version] => Some(Repo::Npm { name, version: Some(version.to_string()) }),
                _ => None,
            }
        }
        // A crate's other pages (/versions, /dependencies) are the crate's; a version starts with a digit.
        "crates.io" | "www.crates.io" => match segs[..] {
            ["crates", name, version, ..] if version.starts_with(|c: char| c.is_ascii_digit()) => {
                Some(Repo::Crate { name: name.into(), version: Some(version.into()) })
            }
            ["crates", name, ..] => Some(Repo::Crate { name: name.into(), version: None }),
            _ => None,
        },
        host => {
            let origin = url.origin().ascii_serialization();
            // GitLab's pages of a project are under {project}/-/.
            if let Some(dash) = segs.iter().position(|s| *s == "-").filter(|&at| at >= 2) {
                let project = segs[..dash].join("/");
                return match segs[dash + 1..] {
                    ["tree", ref rest @ ..] if !rest.is_empty() => Some(Repo::GitLabTree { origin, project, rest: owned(rest) }),
                    ["releases", "permalink", "latest"] => Some(Repo::GitLabRelease { origin, project, tag: None }),
                    ["releases", tag] => Some(Repo::GitLabRelease { origin, project, tag: Some(tag.to_string()) }),
                    _ => None,
                };
            }
            if let [owner, repo, "src", kind @ ("branch" | "tag" | "commit"), ref rest @ ..] = segs[..] {
                return (!rest.is_empty()).then(|| Repo::GiteaSrc { origin, owner: owner.into(), repo: repo.into(), kind: kind.into(), rest: owned(rest) });
            }
            let named = |name: &str| host == format!("{name}.com") || host.starts_with(&format!("{name}."));
            if segs.first().is_some_and(|first| CODE_HOST_PAGES.contains(first)) {
                return None;
            }
            if named("gitlab") && segs.len() >= 2 {
                return Some(Repo::GitLabRoot { origin, project: segs.join("/") });
            }
            if !(host == "codeberg.org" || named("gitea")) {
                return None;
            }
            let (owner, repo) = (segs.first()?.to_string(), segs.get(1)?.to_string());
            match segs[2..] {
                [] => Some(Repo::GiteaRoot { origin, owner, repo }),
                ["releases", "latest"] => Some(Repo::GiteaRelease { origin, owner, repo, tag: None }),
                ["releases", "tag", tag] => Some(Repo::GiteaRelease { origin, owner, repo, tag: Some(tag.to_string()) }),
                _ => None,
            }
        }
    }
}

/// Whether `url` is a repository, folder, release, model or item link [`list`] reads.
pub fn lists(url: &Url) -> bool {
    repo_of(url).is_some()
}

/// One task per file at `url`, in a folder named after it; a repository's zip. An Internet
/// Archive item's files are those `formats` choose (see [`archive_takes`]).
pub async fn list(http: &reqwest::Client, url: &Url, auth: Option<&str>, formats: &[String]) -> Result<Vec<Task>, String> {
    let tasks = match repo_of(url) {
        Some(repo @ (Repo::GitHubRoot { .. } | Repo::GitLabRoot { .. } | Repo::BitbucketRoot { .. } | Repo::GiteaRoot { .. })) => {
            let (zip, name) = zip_of(&repo)?;
            vec![Task { urls: vec![zip], name: Some(component(&format!("{name}.zip"), "repository.zip")?), ..Task::default() }]
        }
        Some(Repo::GitHubTree { owner, repo, rest }) => github_tree(http, auth, GITHUB_API, &owner, &repo, &rest).await?,
        Some(Repo::GitHubRelease { owner, repo, tag }) => github_release(http, auth, GITHUB_API, &owner, &repo, tag.as_deref()).await?,
        Some(Repo::HuggingFace { kind, repo, rev, path }) => hugging_face(http, auth, HUGGING_FACE, kind, &repo, &rev, &path).await?,
        Some(Repo::Civitai { model, version }) => civitai(http, auth, CIVITAI, model, version).await?,
        Some(Repo::Archive { item, path }) => archive(http, auth, ARCHIVE, &item, &path, formats).await?,
        Some(Repo::GitLabTree { origin, project, rest }) => gitlab_tree(http, auth, &origin, &project, &rest).await?,
        Some(Repo::GitLabRelease { origin, project, tag }) => gitlab_release(http, auth, &origin, &project, tag.as_deref()).await?,
        Some(Repo::BitbucketTree { owner, repo, rest }) => bitbucket_tree(http, auth, BITBUCKET_API, &owner, &repo, &rest).await?,
        Some(Repo::BitbucketDownloads { owner, repo }) => bitbucket_downloads(http, auth, BITBUCKET_API, &owner, &repo).await?,
        Some(Repo::GiteaSrc { origin, owner, repo, kind, rest }) => gitea_src(http, auth, &origin, &owner, &repo, &kind, &rest).await?,
        Some(Repo::GiteaRelease { origin, owner, repo, tag }) => gitea_release(http, auth, &origin, &owner, &repo, tag.as_deref()).await?,
        Some(Repo::PyPi { name, version }) => pypi(http, PYPI, &name, version.as_deref()).await?,
        Some(Repo::Npm { name, version }) => npm(http, NPM_REGISTRY, &name, version.as_deref()).await?,
        Some(Repo::Crate { name, version }) => crates_io(http, CRATES_IO, CRATE_FILES, &name, version.as_deref()).await?,
        None => return Err(format!("{} is not a repository, folder, release, model or item link", url)),
    };
    if tasks.is_empty() {
        return Err("the link holds no files the app can download".to_string());
    }
    Ok(tasks)
}

/// Whether `url` is a repository link [`resolve`] takes (a folder's says it must be listed). An
/// archive.org item's page is not: as one download it goes on as any page does (to yt-dlp); nor
/// is a Gitea /src/ link, which may be a file's (see `resolver::CodeHostResolver`), nor a package's page.
pub fn handles(url: &Url) -> bool {
    repo_of(url).is_some_and(|repo| !matches!(repo, Repo::Archive { .. } | Repo::GiteaSrc { .. } | Repo::PyPi { .. } | Repo::Npm { .. } | Repo::Crate { .. }))
}

/// Where the zip of the repository link `url` downloads from.
pub async fn resolve(_client: &reqwest::Client, url: &Url, _proxy: Option<&str>) -> Result<Vec<Url>, ResolverError> {
    match repo_of(url) {
        Some(repo) => zip_of(&repo).map(|(zip, _)| vec![zip]).map_err(ResolverError::NotFound),
        None => Err(ResolverError::NotFound(format!("{} is not a repository link", url))),
    }
}

/// The zip of a repository's default branch, without asking its API, and the repository's name;
/// an error for a link that lists files.
fn zip_of(repo: &Repo) -> Result<(Url, &str), String> {
    match repo {
        Repo::GitHubRoot { owner, repo } => Ok((url_at(GITHUB, [owner.as_str(), repo.as_str(), "archive", "HEAD.zip"])?, repo.as_str())),
        Repo::GitLabRoot { origin, project } => {
            let name = project.rsplit('/').next().unwrap_or(project);
            let file = format!("{name}-HEAD.zip");
            Ok((url_at(origin, project.split('/').chain(["-", "archive", "HEAD", &file]))?, name))
        }
        Repo::BitbucketRoot { owner, repo } => Ok((url_at(BITBUCKET, [owner.as_str(), repo.as_str(), "get", "HEAD.zip"])?, repo.as_str())),
        Repo::GiteaRoot { origin, owner, repo } => Ok((url_at(origin, [owner.as_str(), repo.as_str(), "archive", "HEAD.zip"])?, repo.as_str())),
        _ => Err(LISTS_MANY.to_string()),
    }
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
async fn fetch(request: RequestBuilder, auth: Option<&str>, service: &str) -> Result<Option<Response>, String> {
    let auth = auth.and_then(|v| reqwest::header::HeaderValue::from_str(v).ok()).map(|mut v| {
        v.set_sensitive(true);
        v
    });
    let request = match auth {
        Some(v) => request.header(reqwest::header::AUTHORIZATION, v),
        None => request,
    };
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
async fn github_tree(http: &reqwest::Client, auth: Option<&str>, api: &str, owner: &str, repo: &str, rest: &[String]) -> Result<Vec<Task>, String> {
    for parts in 1..=rest.len().min(MAX_REF_PARTS) {
        let (git_ref, path) = (rest[..parts].join("/"), &rest[parts..]);
        let tree_ish = if path.is_empty() { git_ref.clone() } else { format!("{}:{}", git_ref, path.join("/")) };
        let mut url = url_at(api, ["repos", owner, repo, "git", "trees", &tree_ish])?;
        url.set_query(Some("recursive=1"));
        let Some(answer) = fetch(http.get(url), auth, "GitHub").await? else { continue };
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
async fn github_release(http: &reqwest::Client, auth: Option<&str>, api: &str, owner: &str, repo: &str, tag: Option<&str>) -> Result<Vec<Task>, String> {
    let url = match tag {
        Some(tag) => url_at(api, ["repos", owner, repo, "releases", "tags", tag])?,
        None => url_at(api, ["repos", owner, repo, "releases", "latest"])?,
    };
    let Some(answer) = fetch(http.get(url), auth, "GitHub").await? else {
        return Err(format!("{}/{} has no such release on GitHub (or the repository is private)", owner, repo));
    };
    release_tasks(&json(answer, "GitHub").await?, repo, GITHUB)
}

/// One task per asset of `release` (GitHub's, or a Gitea site's at `base`) of the repository
/// `repo`, in a folder named after it and the tag.
fn release_tasks(release: &Release, repo: &str, base: &str) -> Result<Vec<Task>, String> {
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
                // Only the service's own host gets the user's Authorization.
                from_document: !own(&url, base),
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
async fn hugging_face(http: &reqwest::Client, auth: Option<&str>, base: &str, kind: &str, repo: &str, rev: &str, path: &[String]) -> Result<Vec<Task>, String> {
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
        let Some(answer) = fetch(http.get(page.clone()), auth, "Hugging Face").await? else {
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
async fn civitai(http: &reqwest::Client, auth: Option<&str>, api: &str, model: u64, version: Option<u64>) -> Result<Vec<Task>, String> {
    let url = url_at(api, ["api", "v1", "models", &model.to_string()])?;
    let Some(answer) = fetch(http.get(url), auth, "Civitai").await? else {
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

/// One field of an archive.org item's metadata: `{"result": ...}`, `{"error": ...}` when there is
/// no such item.
#[derive(Deserialize)]
struct ArchiveField<T> {
    result: Option<T>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ArchiveFile {
    name: String,
    /// "original" for an uploaded file, "derivative" for one made of it, "metadata" for records.
    source: String,
    format: String,
    /// A number, written as a string.
    size: Option<String>,
    md5: String,
    sha1: String,
    /// "true" for a file lent or kept to signed-in readers.
    private: Option<String>,
}

/// Whether a listing with `formats` (see `ingest::ListOptions::formats`) takes archive.org's `file`:
/// without any, a file uploaded to the item; else one whose format ("Text PDF") or extension
/// ("pdf") one of them names, of those uploaded ("original") or made of them by archive.org
/// ("derivative") when one of those words is among them, else of both. Its records and thumbnail
/// only when their format is named, a file lent to signed-in readers never.
fn archive_takes(file: &ArchiveFile, formats: &[String]) -> bool {
    if file.private.as_deref() == Some("true") {
        return false;
    }
    let own = file.source == "metadata" || ARCHIVE_OWN_FORMATS.contains(&file.format.as_str());
    let (sources, named): (Vec<&str>, Vec<&str>) =
        formats.iter().map(|f| f.trim().trim_start_matches('.')).filter(|f| !f.is_empty()).partition(|f| f.eq_ignore_ascii_case("original") || f.eq_ignore_ascii_case("derivative"));
    let extension = file.name.rsplit_once('.').map_or("", |(_, ext)| ext);
    let names = |f: &&str| f.eq_ignore_ascii_case(&file.format) || f.eq_ignore_ascii_case(extension);
    match (sources.is_empty(), named.is_empty()) {
        (true, true) => file.source == "original" && !own,
        _ if own => named.iter().any(|f| f.eq_ignore_ascii_case(&file.format)),
        (sources_free, _) => (sources_free || sources.iter().any(|s| s.eq_ignore_ascii_case(&file.source))) && (named.is_empty() || named.iter().any(names)),
    }
}

/// The files of archive.org's `item` a listing with `formats` takes (see [`archive_takes`]), each a
/// task for its /download/ link in a folder named after the item: the one way an item becomes
/// downloads, whether its own link or a collection's or search's (`crate::iasearch`) names it.
pub(crate) async fn archive_item(http: &reqwest::Client, base: &str, item: &str, formats: &[String]) -> Result<Vec<Task>, String> {
    archive(http, None, base, item, &[], formats).await
}

/// The files of archive.org's `item` a listing with `formats` takes (see [`archive_takes`]: by
/// default those uploaded to it, not the copies archive.org made of them, nor its own records),
/// under `path` in it, subfolders kept, each a task for its /download/ link with its size and
/// SHA-1 (else MD5).
async fn archive(http: &reqwest::Client, auth: Option<&str>, base: &str, item: &str, path: &[String], formats: &[String]) -> Result<Vec<Task>, String> {
    let url = url_at(base, ["metadata", item, "files"])?;
    let missing = || format!("archive.org has no item {} (or it is dark)", item);
    let answer = fetch(http.get(url), auth, "archive.org").await?.ok_or_else(missing)?;
    let files = json::<ArchiveField<Vec<ArchiveFile>>>(answer, "archive.org").await?.result.ok_or_else(missing)?;
    let prefix = path.join("/");
    let folder = component(path.last().map_or(item, String::as_str), item)?;
    let mut tasks = Vec::new();
    for file in files {
        if !archive_takes(&file, formats) {
            continue;
        }
        let Some(name) = inside(&file.name, &prefix).and_then(relative) else { continue };
        tasks.push(Task {
            urls: vec![url_at(base, ["download", item].into_iter().chain(file.name.split('/')))?],
            name: Some(name),
            folder: Some(folder.clone()),
            size: file.size.and_then(|size| size.parse().ok()),
            checksum: checksum("sha1", &file.sha1).or_else(|| checksum("md5", &file.md5)),
            ..Task::default()
        });
    }
    Ok(tasks)
}

/// `path` (an entry's, from the repository's top) inside the folder `prefix` ("" for the top).
fn inside<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    path.strip_prefix(prefix)?.strip_prefix('/')
}

/// The name of a GitLab project, its path's last part.
fn last_part(project: &str) -> &str {
    project.rsplit('/').next().unwrap_or(project)
}

/// Every file under the folder `rest` (its ref, then its path) names in the GitLab project
/// `project` at `origin`, subfolders kept, each a task for its /-/raw/ link (which serves Git
/// LFS files too). The ref is the shortest start of `rest` GitLab knows.
async fn gitlab_tree(http: &reqwest::Client, auth: Option<&str>, origin: &str, project: &str, rest: &[String]) -> Result<Vec<Task>, String> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Entry {
        path: String,
        #[serde(rename = "type")]
        kind: String,
    }
    for parts in 1..=rest.len().min(MAX_REF_PARTS) {
        let (git_ref, path) = (rest[..parts].join("/"), rest[parts..].join("/"));
        let mut page = url_at(origin, ["api", "v4", "projects", project, "repository", "tree"])?;
        let query = [("ref", git_ref.as_str()), ("path", path.as_str()), ("recursive", "true"), ("per_page", "100"), ("pagination", "keyset")];
        page.query_pairs_mut().extend_pairs(query);
        let Some(mut answer) = fetch(http.get(page), auth, "GitLab").await? else { continue };
        let folder = component(rest[parts..].last().map_or(last_part(project), String::as_str), last_part(project))?;
        let mut tasks = Vec::new();
        for _ in 0..MAX_PAGES {
            let next = next_page(&answer, origin);
            let entries: Vec<Entry> = json(answer, "GitLab").await?;
            for entry in entries.iter().filter(|e| e.kind == "blob") {
                let Some(name) = inside(&entry.path, &path).and_then(relative) else { continue };
                let raw = project.split('/').chain(["-", "raw"]).chain(git_ref.split('/')).chain(entry.path.split('/'));
                tasks.push(Task { urls: vec![url_at(origin, raw)?], name: Some(name), folder: Some(folder.clone()), ..Task::default() });
            }
            let Some(next) = next else { return Ok(tasks) };
            answer = fetch(http.get(next), auth, "GitLab").await?.ok_or_else(|| "GitLab lost the rest of the listing".to_string())?;
        }
        return Err(format!("the folder holds more than {} files: add the links of its subfolders instead", MAX_PAGES * 100));
    }
    Err(format!("GitLab has no such folder in {} (or the project is private)", project))
}

/// Every file of the release `tag` (None the latest) of the GitLab project `project` at
/// `origin`: its asset links, each named after the file it points at.
async fn gitlab_release(http: &reqwest::Client, auth: Option<&str>, origin: &str, project: &str, tag: Option<&str>) -> Result<Vec<Task>, String> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Release {
        tag_name: String,
        assets: Assets,
    }
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Assets {
        links: Vec<Link>,
    }
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Link {
        name: String,
        url: String,
        direct_asset_url: Option<String>,
    }
    let api = ["api", "v4", "projects", project, "releases"];
    let url = match tag {
        Some(tag) => url_at(origin, api.into_iter().chain([tag]))?,
        None => url_at(origin, api.into_iter().chain(["permalink", "latest"]))?,
    };
    let Some(answer) = fetch(http.get(url), auth, "GitLab").await? else {
        return Err(format!("{} has no such release on GitLab (or the project is private)", project));
    };
    let release: Release = json(answer, "GitLab").await?;
    let folder = component(&format!("{} {}", last_part(project), release.tag_name), last_part(project))?;
    let tasks: Vec<Task> = release
        .assets
        .links
        .iter()
        .filter_map(|link| {
            let url = Url::parse(link.direct_asset_url.as_deref().unwrap_or(&link.url)).ok()?;
            // A link's name is a label ("Linux build"); the file's is the last part of its URL.
            let file = url.path_segments().and_then(|mut s| s.next_back()).map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned());
            Some(Task {
                name: file.as_deref().and_then(|f| clean_path([f]).ok()).or_else(|| clean_path([link.name.as_str()]).ok()),
                folder: Some(folder.clone()),
                from_document: !own(&url, origin),
                urls: vec![url],
                ..Task::default()
            })
        })
        .collect();
    if tasks.is_empty() {
        return Err(format!("the release {} has no files besides its source code: add the project's link to get that", release.tag_name));
    }
    Ok(tasks)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct BitbucketEntry {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    name: String,
    size: Option<u64>,
}

/// The entries of every page of the Bitbucket listing at `url`, through its `next` links on
/// `api`; None when there is no such thing.
async fn bitbucket_pages(http: &reqwest::Client, auth: Option<&str>, api: &str, mut url: Url) -> Result<Option<Vec<BitbucketEntry>>, String> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        values: Vec<BitbucketEntry>,
        next: Option<String>,
    }
    let mut entries = Vec::new();
    for _ in 0..MAX_PAGES {
        let Some(answer) = fetch(http.get(url), auth, "Bitbucket").await? else { return Ok(None) };
        let page: Page = json(answer, "Bitbucket").await?;
        entries.extend(page.values);
        match page.next.and_then(|next| Url::parse(&next).ok()).filter(|next| own(next, api)) {
            Some(next) => url = next,
            None => return Ok(Some(entries)),
        }
    }
    Err(format!("the listing holds more than {} files: add the links of its subfolders instead", MAX_PAGES * 100))
}

/// Every file under the folder `rest` (its ref, then its path) names in Bitbucket's
/// `owner`/`repo`, subfolders kept, each a task for its /raw/ link. The ref is the shortest start
/// of `rest` Bitbucket knows.
async fn bitbucket_tree(http: &reqwest::Client, auth: Option<&str>, api: &str, owner: &str, repo: &str, rest: &[String]) -> Result<Vec<Task>, String> {
    for parts in 1..=rest.len().min(MAX_REF_PARTS) {
        let (git_ref, path) = (rest[..parts].join("/"), &rest[parts..]);
        // The last empty segment makes it a folder's link.
        let segments = ["repositories", owner, repo, "src", git_ref.as_str()].into_iter().chain(path.iter().map(String::as_str)).chain([""]);
        let mut url = url_at(api, segments)?;
        url.set_query(Some("max_depth=100&pagelen=100"));
        let Some(entries) = bitbucket_pages(http, auth, api, url).await? else { continue };
        let (prefix, folder) = (path.join("/"), component(path.last().map_or(repo, String::as_str), repo)?);
        let mut tasks = Vec::new();
        for entry in entries.iter().filter(|e| e.kind == "commit_file") {
            let Some(name) = inside(&entry.path, &prefix).and_then(relative) else { continue };
            let raw = [owner, repo, "raw"].into_iter().chain(git_ref.split('/')).chain(entry.path.split('/'));
            tasks.push(Task { urls: vec![url_at(BITBUCKET, raw)?], name: Some(name), folder: Some(folder.clone()), size: entry.size, ..Task::default() });
        }
        return Ok(tasks);
    }
    Err(format!("Bitbucket has no such folder in {}/{} (or the repository is private)", owner, repo))
}

/// Every file uploaded to the Downloads of Bitbucket's `owner`/`repo`.
async fn bitbucket_downloads(http: &reqwest::Client, auth: Option<&str>, api: &str, owner: &str, repo: &str) -> Result<Vec<Task>, String> {
    let mut url = url_at(api, ["repositories", owner, repo, "downloads"])?;
    url.set_query(Some("pagelen=100"));
    let entries = bitbucket_pages(http, auth, api, url).await?;
    let entries = entries.ok_or_else(|| format!("Bitbucket has no repository {}/{} (or it is private)", owner, repo))?;
    let folder = component(&format!("{} downloads", repo), repo)?;
    entries
        .iter()
        .filter_map(|entry| Some((clean_path([entry.name.as_str()]).ok()?, entry)))
        .map(|(name, entry)| {
            let url = url_at(BITBUCKET, [owner, repo, "downloads", entry.name.as_str()])?;
            Ok(Task { urls: vec![url], name: Some(name), folder: Some(folder.clone()), size: entry.size, ..Task::default() })
        })
        .collect()
}

/// The host of `origin`, which names a Gitea site in errors.
fn site(origin: &str) -> String {
    Url::parse(origin).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| "Gitea".to_string())
}

/// Every entry of the Gitea repository `owner`/`repo` at `origin` at `git_ref`, page after page;
/// None when it has no such ref.
async fn gitea_tree(http: &reqwest::Client, auth: Option<&str>, origin: &str, owner: &str, repo: &str, git_ref: &str) -> Result<Option<Vec<GitEntry>>, String> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Tree {
        tree: Vec<GitEntry>,
        total_count: usize,
    }
    let service = site(origin);
    let mut entries = Vec::new();
    for page in 1..=MAX_PAGES {
        let mut url = url_at(origin, ["api", "v1", "repos", owner, repo, "git", "trees", git_ref])?;
        url.set_query(Some(&format!("recursive=true&per_page=1000&page={page}")));
        let answer = match fetch(http.get(url), auth, &service).await {
            Ok(Some(answer)) => answer,
            // Gitea answers a ref it does not have with 400.
            Ok(None) => return Ok(None),
            Err(e) if e.ends_with("HTTP 400") => return Ok(None),
            Err(e) => return Err(e),
        };
        let tree: Tree = json(answer, &service).await?;
        let read = tree.tree.len();
        entries.extend(tree.tree);
        if read == 0 || entries.len() >= tree.total_count {
            return Ok(Some(entries));
        }
    }
    Err(format!("the repository holds more than {} files: add the links of its subfolders instead", MAX_PAGES * 1000))
}

/// What `rest` (a ref, then a path) names in the Gitea repository `owner`/`repo` at `origin`
/// (`kind` says whether the ref is a branch, tag or commit): a folder, every file under it as a
/// task for its /media/ link (which serves Git LFS files too), subfolders kept; or a file, its one
/// task. The ref is the shortest start of `rest` with something at the path after it.
async fn gitea_src(http: &reqwest::Client, auth: Option<&str>, origin: &str, owner: &str, repo: &str, kind: &str, rest: &[String]) -> Result<Vec<Task>, String> {
    for parts in 1..=rest.len().min(MAX_REF_PARTS) {
        let (git_ref, path) = (rest[..parts].join("/"), rest[parts..].join("/"));
        let Some(entries) = gitea_tree(http, auth, origin, owner, repo, &git_ref).await? else { continue };
        let media = |file: &str| url_at(origin, [owner, repo, "media", kind].into_iter().chain(git_ref.split('/')).chain(file.split('/')));
        if let Some(file) = entries.iter().find(|e| e.kind == "blob" && e.path == path) {
            let name = rest.last().and_then(|last| clean_path([last.as_str()]).ok());
            return Ok(vec![Task { urls: vec![media(&file.path)?], name, size: file.size, ..Task::default() }]);
        }
        let folder = component(rest[parts..].last().map_or(repo, String::as_str), repo)?;
        let mut tasks = Vec::new();
        for entry in entries.iter().filter(|e| e.kind == "blob") {
            let Some(name) = inside(&entry.path, &path).and_then(relative) else { continue };
            tasks.push(Task { urls: vec![media(&entry.path)?], name: Some(name), folder: Some(folder.clone()), size: entry.size, ..Task::default() });
        }
        // Nothing there: the ref may go on past this part.
        if !tasks.is_empty() || path.is_empty() {
            return Ok(tasks);
        }
    }
    Err(format!("{} has no such file or folder in {}/{} (or the repository is private)", site(origin), owner, repo))
}

/// Every file of the release `tag` (None the latest) of the Gitea repository `owner`/`repo` at
/// `origin`.
async fn gitea_release(http: &reqwest::Client, auth: Option<&str>, origin: &str, owner: &str, repo: &str, tag: Option<&str>) -> Result<Vec<Task>, String> {
    let api = ["api", "v1", "repos", owner, repo, "releases"];
    let url = match tag {
        Some(tag) => url_at(origin, api.into_iter().chain(["tags", tag]))?,
        None => url_at(origin, api.into_iter().chain(["latest"]))?,
    };
    let Some(answer) = fetch(http.get(url), auth, &site(origin)).await? else {
        return Err(format!("{}/{} has no such release on {} (or the repository is private)", owner, repo, site(origin)));
    };
    release_tasks(&json(answer, &site(origin)).await?, repo, origin)
}

#[derive(Deserialize)]
struct PyPiRelease {
    info: PyPiInfo,
    urls: Vec<PyPiFile>,
}

#[derive(Deserialize)]
struct PyPiInfo {
    name: String,
    version: String,
}

#[derive(Deserialize)]
struct PyPiFile {
    filename: String,
    url: Url,
    size: Option<u64>,
    #[serde(default)]
    digests: HashMap<String, String>,
}

/// The files (wheels and source) of the release `version` of PyPI's project `name`, its latest
/// without one, each with its size and SHA-256, in a folder named after the release.
async fn pypi(http: &reqwest::Client, base: &str, name: &str, version: Option<&str>) -> Result<Vec<Task>, String> {
    let api = url_at(base, ["pypi", name].into_iter().chain(version).chain(["json"]))?;
    let Some(answer) = fetch(http.get(api), None, "PyPI").await? else {
        return Err(match version {
            Some(version) => format!("PyPI has no version {version} of {name}"),
            None => format!("PyPI has no project {name}"),
        });
    };
    let release: PyPiRelease = json(answer, "PyPI").await?;
    let folder = component(&format!("{}-{}", release.info.name, release.info.version), name)?;
    Ok(release
        .urls
        .into_iter()
        .filter_map(|file| {
            Some(Task {
                name: Some(relative(&file.filename)?),
                folder: Some(folder.clone()),
                size: file.size,
                checksum: file.digests.get("sha256").and_then(|hex| checksum("sha256", hex)),
                // On PyPI's file host, not the site the user typed.
                from_document: true,
                urls: vec![file.url],
                ..Task::default()
            })
        })
        .collect())
}

#[derive(Deserialize)]
struct NpmVersion {
    dist: NpmDist,
}

#[derive(Deserialize)]
struct NpmDist {
    tarball: Url,
    integrity: Option<String>,
    shasum: Option<String>,
}

/// The tarball of npm's package `name` at `version` (a version or a tag such as `next`), the
/// latest without one, with its SHA-512 integrity hash, else its SHA-1.
async fn npm(http: &reqwest::Client, registry: &str, name: &str, version: Option<&str>) -> Result<Vec<Task>, String> {
    use base64::Engine as _;
    let api = url_at(registry, name.split('/').chain([version.unwrap_or("latest")]))?;
    let Some(answer) = fetch(http.get(api), None, "npm").await? else {
        return Err(match version {
            Some(version) => format!("npm has no version {version} of {name}"),
            None => format!("npm has no package {name}"),
        });
    };
    let NpmVersion { dist } = json(answer, "npm").await?;
    let sha512 = dist.integrity.as_deref().unwrap_or_default().split_whitespace().find_map(|sri| sri.strip_prefix("sha512-"));
    let sha512 = sha512.and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok()).filter(|d| d.len() == 64);
    let file = dist.tarball.path_segments().and_then(|mut s| s.next_back()).unwrap_or_default().to_string();
    Ok(vec![Task {
        name: Some(component(&percent_decode_str(&file).decode_utf8_lossy(), "package.tgz")?),
        checksum: sha512.map(|d| crate::digest::checksum("sha512", &d)).or_else(|| checksum("sha1", dist.shasum.as_deref()?)),
        // On the registry's host, not the site the user typed.
        from_document: true,
        urls: vec![dist.tarball],
        ..Task::default()
    }])
}

#[derive(Deserialize)]
struct CrateAnswer {
    #[serde(rename = "crate")]
    krate: Option<CrateInfo>,
    version: Option<CrateVersion>,
}

#[derive(Deserialize)]
struct CrateInfo {
    default_version: Option<String>,
}

#[derive(Deserialize)]
struct CrateVersion {
    #[serde(rename = "crate")]
    name: String,
    num: String,
    checksum: Option<String>,
    crate_size: Option<u64>,
}

/// The .crate file of crates.io's crate `name` at `version`, its default version (the newest
/// stable one) without one, from `files` with its size and SHA-256.
async fn crates_io(http: &reqwest::Client, api: &str, files: &str, name: &str, version: Option<&str>) -> Result<Vec<Task>, String> {
    let version = match version {
        Some(version) => version.to_string(),
        None => {
            let url = url_at(api, ["api", "v1", "crates", name])?;
            let Some(answer) = fetch(http.get(url).query(&[("include", "default_version")]), None, "crates.io").await? else {
                return Err(format!("crates.io has no crate {name}"));
            };
            let crate_info = json::<CrateAnswer>(answer, "crates.io").await?.krate;
            crate_info.and_then(|c| c.default_version).ok_or_else(|| format!("crates.io names no version of {name}"))?
        }
    };
    let url = url_at(api, ["api", "v1", "crates", name, &version])?;
    let Some(answer) = fetch(http.get(url), None, "crates.io").await? else {
        return Err(format!("crates.io has no version {version} of {name}"));
    };
    let found = json::<CrateAnswer>(answer, "crates.io").await?.version.ok_or_else(|| format!("crates.io has no version {version} of {name}"))?;
    let file = format!("{}-{}.crate", found.name, found.num);
    Ok(vec![Task {
        urls: vec![url_at(files, [found.name.as_str(), &file])?],
        name: Some(component(&file, "package.crate")?),
        size: found.crate_size,
        checksum: found.checksum.and_then(|hex| checksum("sha256", &hex)),
        // On crates.io's file host, not the site the user typed.
        from_document: true,
        ..Task::default()
    }])
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
        let tasks = list(&http(), &root, None, &[]).await.unwrap();
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
        let tasks = github_tree(&http(), None, &api, "o", "r", &strings(&["feature", "x", "docs"])).await.unwrap();
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
        let err = github_tree(&http(), None, &api, "o", "big", &strings(&["main"])).await.unwrap_err();
        assert!(err.contains("subfolders"), "{err}");
        let err = github_tree(&http(), None, &api, "o", "busy", &strings(&["main"])).await.unwrap_err();
        assert!(err.contains("60 requests an hour") && err.contains("in 10 minutes"), "{err}");
        let err = github_tree(&http(), None, &api, "o", "r", &strings(&["nope", "docs"])).await.unwrap_err();
        assert!(err.contains("no such folder in o/r"), "{err}");
    }

    /// A release is one task per asset, under the repository and tag, with GitHub's digest; an
    /// asset on another host is listed without the user's Authorization, one named ".." not at all.
    #[tokio::test]
    async fn github_releases_list_their_assets() {
        let (addr, _) = serve(github).await;
        let api = format!("http://{addr}");
        let tasks = github_release(&http(), None, &api, "o", "r", None).await.unwrap();
        let sha = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string();
        let listed: Vec<_> = tasks.iter().map(|t| (t.name.clone().unwrap(), t.checksum.clone(), t.from_document)).collect();
        assert_eq!(listed, [(PathBuf::from("app.zip"), Some(sha), false), (PathBuf::from("mirror.zip"), None, true)]);
        assert!(tasks.iter().all(|t| t.folder == Some("r v2.0".into())));
        assert!(github_release(&http(), None, &api, "o", "r", Some("empty")).await.unwrap_err().contains("source code"));
        assert!(github_release(&http(), None, &api, "o", "r", Some("gone")).await.unwrap_err().contains("no such release"));
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
        let tasks = hugging_face(&http(), None, &base, "datasets", "o/d", "main", &strings(&["sub"])).await.unwrap();
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
            async move { hugging_face(&http(), None, &base, "models", repo, "main", &[]).await.unwrap_err() }
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
        let newest = civitai(&http(), None, &api, 7, None).await.unwrap();
        let sha = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string();
        assert_eq!(newest.len(), 1);
        assert_eq!(newest[0].urls[0].as_str(), "https://civitai.com/api/download/models/2?fileId=20");
        assert_eq!((newest[0].checksum.clone(), newest[0].from_document), (Some(sha), false));
        assert_eq!(newest[0].folder, Some("Model_ X v2".into()));
        let v1 = civitai(&http(), None, &api, 7, Some(1)).await.unwrap();
        let listed: Vec<_> = v1.iter().map(|t| (t.name.clone().unwrap(), t.from_document)).collect();
        assert_eq!(listed, [(PathBuf::from("x-v1.safetensors"), false), (PathBuf::from("11"), true)]);
        assert!(civitai(&http(), None, &api, 7, Some(9)).await.unwrap_err().contains("no such version"));
        assert!(civitai(&http(), None, &api, 8, None).await.unwrap_err().contains("no model 8"));
    }

    fn archive_api(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        match path {
            // As archive.org answers (sizes as strings), its own files and derived copies among them.
            "/metadata/item/files" => json_reply(
                r#"{"result":[
                    {"name":"__ia_thumb.jpg","source":"original","size":"5399","format":"JPEG Thumb","md5":"f2d804af1cdc63b13db38474ba26c5bb"},
                    {"name":"globe west.jpg","source":"original","size":"66065","format":"JPEG","md5":"9366a4b09386bf673c447e33d806d904","sha1":"3e20a009994405f535cdf07cdc2974cef2fce8f2"},
                    {"name":"globe west_thumb.jpg","source":"derivative","format":"JPEG Thumb","original":"globe west.jpg","size":"6168"},
                    {"name":"item_archive.torrent","source":"metadata","format":"Archive BitTorrent","size":"2246"},
                    {"name":"item_meta.xml","source":"original","format":"Metadata","size":"7105"},
                    {"name":"item_itemimage.jpg","source":"original","format":"Item Image","size":"38366"},
                    {"name":"disc 1/track.flac","source":"original","format":"Flac","size":"12","md5":"51a545810bc354a03263332b7654c5fa"},
                    {"name":"disc 1/track.mp3","source":"derivative","format":"VBR MP3","original":"disc 1/track.flac","size":"7"},
                    {"name":"disc 1/lent.pdf","source":"original","format":"Text PDF","private":"true"},
                    {"name":"disc 10/other.flac","source":"original","format":"Flac"},
                    {"name":"disc 1/../../evil.exe","source":"original","format":"Unknown"}]}"#,
            ),
            _ => json_reply(r#"{"error":"Couldn't locate item 'gone'"}"#),
        }
    }

    /// An item is one task per file uploaded to it (not archive.org's copies of them or its own
    /// records, nor a file lent to signed-in readers), each its /download/ link with its size and
    /// SHA-1, else MD5; a folder of it, the files under it; an item that does not exist says so.
    #[tokio::test]
    async fn archive_items_list_their_original_files() {
        let (addr, _) = serve(archive_api).await;
        let base = format!("http://{addr}");
        let tasks = archive(&http(), None, &base, "item", &[], &[]).await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap(), t.size, t.checksum.clone())).collect();
        assert_eq!(
            listed,
            [
                (format!("{base}/download/item/globe%20west.jpg"), PathBuf::from("globe west.jpg"), Some(66065), Some("sha1:3e20a009994405f535cdf07cdc2974cef2fce8f2".into())),
                (format!("{base}/download/item/disc%201/track.flac"), PathBuf::from("disc 1").join("track.flac"), Some(12), Some("md5:51a545810bc354a03263332b7654c5fa".into())),
                (format!("{base}/download/item/disc%2010/other.flac"), PathBuf::from("disc 10").join("other.flac"), None, None),
            ]
        );
        assert!(tasks.iter().all(|t| t.folder == Some("item".into()) && !t.from_document));
        let folder = archive(&http(), None, &base, "item", &strings(&["disc 1"]), &[]).await.unwrap();
        assert_eq!(folder.iter().map(|t| (t.folder.clone().unwrap(), t.name.clone().unwrap())).collect::<Vec<_>>(), [(PathBuf::from("disc 1"), PathBuf::from("track.flac"))]);
        assert!(archive(&http(), None, &base, "gone", &[], &[]).await.unwrap_err().contains("no item gone"));
        assert_eq!(
            repo("https://archive.org/details/item"),
            Some(Repo::Archive { item: "item".into(), path: Vec::new() })
        );
        assert_eq!(repo("https://archive.org/download/item/disc%201/"), Some(Repo::Archive { item: "item".into(), path: strings(&["disc 1"]) }));
        assert_eq!(repo("https://archive.org/download/item/"), Some(Repo::Archive { item: "item".into(), path: Vec::new() }));
        for other in ["https://archive.org/download/item/disc%201/track.flac", "https://archive.org/details/item/page/n5", "https://web.archive.org/details/item"] {
            assert_eq!(repo(other), None, "{other}");
        }
        assert!(lists(&Url::parse("https://archive.org/details/item").unwrap()) && !handles(&Url::parse("https://archive.org/details/item").unwrap()));
    }

    /// Formats choose an item's files by their format or extension, of both sources unless
    /// "original" or "derivative" says which; archive.org's own records only by their format's
    /// name, a file lent to signed-in readers never.
    #[tokio::test]
    async fn archive_formats_choose_files() {
        let (addr, _) = serve(archive_api).await;
        let base = format!("http://{addr}");
        let names = |formats: &[&str]| {
            let (http, base) = (http(), base.clone());
            let formats = strings(formats);
            async move {
                let tasks = archive_item(&http, &base, "item", &formats).await.unwrap();
                tasks.into_iter().map(|t| t.name.unwrap().to_string_lossy().replace('\\', "/")).collect::<Vec<_>>()
            }
        };
        assert_eq!(names(&["FLAC", ".mp3"]).await, ["disc 1/track.flac", "disc 1/track.mp3", "disc 10/other.flac"]);
        assert_eq!(names(&["original", "mp3", "flac"]).await, ["disc 1/track.flac", "disc 10/other.flac"]);
        assert_eq!(names(&["derivative"]).await, ["disc 1/track.mp3"]);
        assert_eq!(names(&["jpg"]).await, ["globe west.jpg"], "not archive.org's thumbnails");
        assert_eq!(names(&["JPEG Thumb", "Metadata", "Text PDF"]).await, ["__ia_thumb.jpg", "globe west_thumb.jpg", "item_meta.xml"]);
    }

    fn pypi_api(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        match path {
            // As PyPI answers, cut down.
            "/pypi/Six/json" | "/pypi/six/1.16.0/json" => json_reply(
                r#"{"info":{"name":"six","version":"1.16.0"},"urls":[
                    {"filename":"six-1.16.0-py2.py3-none-any.whl","packagetype":"bdist_wheel","size":11053,"yanked":false,
                     "url":"https://files.pythonhosted.org/packages/d9/5a/e7c31adbe875f2abbb91bd84cf2dc52d792b5a01506781dbcf25c91daf11/six-1.16.0-py2.py3-none-any.whl",
                     "digests":{"blake2b_256":"d95ae7c31adbe875f2abbb91bd84cf2dc52d792b5a01506781dbcf25c91daf11","md5":"529d7fd7e14612ccde86417b4402d6f3","sha256":"8abb2f1d86890a2dfb989f9a77cfcfd3e47c2a354b01111771326f8aa26e0254"}},
                    {"filename":"six-1.16.0.tar.gz","packagetype":"sdist","size":34041,"yanked":false,
                     "url":"https://files.pythonhosted.org/packages/71/39/171f1c67cd00715f190ba0b100d606d440a28c93c7714febeca8b79af85e/six-1.16.0.tar.gz",
                     "digests":{"sha256":"1E61C37477A1626458E36F7B1D82AA5C9B094FA4802892072E49DE9C60C4C926"}}]}"#,
            ),
            "/pypi/empty/json" => json_reply(r#"{"info":{"name":"empty","version":"0.1"},"urls":[]}"#),
            _ => (404, "Content-Type: application/json\r\n".into(), br#"{"message": "Not Found"}"#.to_vec()),
        }
    }

    /// A PyPI project is its latest release's files, a version's link that release's, each with
    /// its size and SHA-256, in a folder named after the release; a missing one says so.
    #[tokio::test]
    async fn pypi_releases_list_their_files() {
        let (addr, _) = serve(pypi_api).await;
        let base = format!("http://{addr}");
        for version in [None, Some("1.16.0")] {
            let name = if version.is_some() { "six" } else { "Six" };
            let tasks = pypi(&http(), &base, name, version).await.unwrap();
            let listed: Vec<_> = tasks.iter().map(|t| (t.name.clone().unwrap(), t.size, t.checksum.clone().unwrap())).collect();
            assert_eq!(
                listed,
                [
                    (PathBuf::from("six-1.16.0-py2.py3-none-any.whl"), Some(11053), "sha256:8abb2f1d86890a2dfb989f9a77cfcfd3e47c2a354b01111771326f8aa26e0254".to_string()),
                    (PathBuf::from("six-1.16.0.tar.gz"), Some(34041), "sha256:1e61c37477a1626458e36f7b1d82aa5c9b094fa4802892072e49de9c60c4c926".to_string()),
                ]
            );
            assert!(tasks.iter().all(|t| t.folder == Some("six-1.16.0".into()) && t.from_document && t.urls[0].host_str() == Some("files.pythonhosted.org")));
        }
        assert_eq!(pypi(&http(), &base, "six", Some("9.9")).await.unwrap_err(), "PyPI has no version 9.9 of six");
        assert_eq!(pypi(&http(), &base, "gone", None).await.unwrap_err(), "PyPI has no project gone");
        assert!(pypi(&http(), &base, "empty", None).await.unwrap().is_empty());
        let pkg = |name: &str, version: Option<&str>| Some(Repo::PyPi { name: name.into(), version: version.map(Into::into) });
        assert_eq!(repo("https://pypi.org/project/six/"), pkg("six", None));
        assert_eq!(repo("https://pypi.org/project/six/1.16.0/#files"), pkg("six", Some("1.16.0")));
        for other in ["https://pypi.org/search/?q=six", "https://pypi.org/project/six/1.16.0/x/", "https://pypi.org/user/x/"] {
            assert_eq!(repo(other), None, "{other}");
        }
        let link = Url::parse("https://pypi.org/project/six/").unwrap();
        assert!(lists(&link) && !handles(&link));
    }

    fn npm_api(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        match path {
            // As the registry answers, cut down.
            "/left-pad/latest" | "/left-pad/1.3.0" => json_reply(
                r#"{"name":"left-pad","version":"1.3.0","dist":{"shasum":"5b8a3a7765dfe001261dde915589e782f8c94d1e",
                    "tarball":"https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz","fileCount":10,
                    "integrity":"sha512-XI5MPzVNApjAyhQzphX8BkmKsKUxD4LdyK24iZeQGinBN9yTQT3bFlCBy/aVx2HrNcqQGsdot8ghrjyrvMCoEA=="}}"#,
            ),
            "/@scope/old/next" => json_reply(
                r#"{"name":"@scope/old","version":"0.1.0","dist":{"shasum":"5B8A3A7765DFE001261DDE915589E782F8C94D1E",
                    "tarball":"https://registry.npmjs.org/@scope/old/-/old-0.1.0.tgz"}}"#,
            ),
            _ => (404, "Content-Type: application/json\r\n".into(), br#""version not found: 9.9.9""#.to_vec()),
        }
    }

    /// An npm package is its latest version's tarball, a version's or tag's link that one's, with
    /// its SHA-512 integrity hash, else its SHA-1; a missing one says so.
    #[tokio::test]
    async fn npm_packages_are_their_tarball() {
        let (addr, _) = serve(npm_api).await;
        let base = format!("http://{addr}");
        let sha512 = "sha512:5c8e4c3f354d0298c0ca1433a615fc06498ab0a5310f82ddc8adb88997901a29c137dc93413ddb165081cbf695c761eb35ca901ac768b7c821ae3cabbcc0a810";
        for version in [None, Some("1.3.0")] {
            let [task] = &npm(&http(), &base, "left-pad", version).await.unwrap()[..] else { panic!("one tarball") };
            assert_eq!(task.urls[0].as_str(), "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz");
            assert_eq!((task.name.clone().unwrap(), task.checksum.as_deref(), task.from_document), (PathBuf::from("left-pad-1.3.0.tgz"), Some(sha512), true));
        }
        let [old] = &npm(&http(), &base, "@scope/old", Some("next")).await.unwrap()[..] else { panic!("one tarball") };
        assert_eq!((old.name.clone().unwrap(), old.checksum.as_deref()), (PathBuf::from("old-0.1.0.tgz"), Some("sha1:5b8a3a7765dfe001261dde915589e782f8c94d1e")));
        assert_eq!(npm(&http(), &base, "left-pad", Some("9.9.9")).await.unwrap_err(), "npm has no version 9.9.9 of left-pad");
        assert_eq!(npm(&http(), &base, "gone", None).await.unwrap_err(), "npm has no package gone");
        let pkg = |name: &str, version: Option<&str>| Some(Repo::Npm { name: name.into(), version: version.map(Into::into) });
        assert_eq!(repo("https://www.npmjs.com/package/left-pad"), pkg("left-pad", None));
        assert_eq!(repo("https://www.npmjs.com/package/left-pad/v/1.3.0?activeTab=versions"), pkg("left-pad", Some("1.3.0")));
        assert_eq!(repo("https://www.npmjs.com/package/%40types/node"), pkg("@types/node", None));
        assert_eq!(repo("https://www.npmjs.com/package/@types/node/v/22.0.0"), pkg("@types/node", Some("22.0.0")));
        for other in ["https://www.npmjs.com/search?q=pad", "https://www.npmjs.com/package/left-pad/x", "https://www.npmjs.com/~user"] {
            assert_eq!(repo(other), None, "{other}");
        }
        let link = Url::parse("https://www.npmjs.com/package/left-pad").unwrap();
        assert!(lists(&link) && !handles(&link));
    }

    fn crates_api(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        match path {
            // As crates.io answers, cut down: the name in its own spelling.
            "/api/v1/crates/Cfg_If?include=default_version" => json_reply(r#"{"crate":{"id":"cfg-if","name":"cfg-if","default_version":"1.0.0"},"versions":null}"#),
            "/api/v1/crates/Cfg_If/1.0.0" | "/api/v1/crates/cfg-if/1.0.0" => json_reply(
                r#"{"version":{"crate":"cfg-if","num":"1.0.0","crate_size":7934,"yanked":false,"dl_path":"/api/v1/crates/cfg-if/1.0.0/download",
                    "checksum":"baf1de4339761588bc0619e3cbc0120ee582ebb74b53b4efbf79117bd2da40fd"}}"#,
            ),
            "/api/v1/crates/none?include=default_version" => json_reply(r#"{"crate":{"name":"none","default_version":null}}"#),
            _ => (404, "Content-Type: application/json\r\n".into(), br#"{"errors":[{"detail":"Not Found"}]}"#.to_vec()),
        }
    }

    /// A crate is its default version's .crate file, a version's link that one's, with its size and
    /// SHA-256; a missing one says so.
    #[tokio::test]
    async fn crates_are_their_crate_file() {
        let (addr, _) = serve(crates_api).await;
        let api = format!("http://{addr}");
        for (name, version) in [("Cfg_If", None), ("cfg-if", Some("1.0.0"))] {
            let [task] = &crates_io(&http(), &api, CRATE_FILES, name, version).await.unwrap()[..] else { panic!("one file") };
            assert_eq!(task.urls[0].as_str(), "https://static.crates.io/crates/cfg-if/cfg-if-1.0.0.crate");
            assert_eq!(
                (task.name.clone().unwrap(), task.size, task.checksum.as_deref(), task.from_document),
                (PathBuf::from("cfg-if-1.0.0.crate"), Some(7934), Some("sha256:baf1de4339761588bc0619e3cbc0120ee582ebb74b53b4efbf79117bd2da40fd"), true)
            );
        }
        assert_eq!(crates_io(&http(), &api, CRATE_FILES, "cfg-if", Some("9.9.9")).await.unwrap_err(), "crates.io has no version 9.9.9 of cfg-if");
        assert_eq!(crates_io(&http(), &api, CRATE_FILES, "gone", None).await.unwrap_err(), "crates.io has no crate gone");
        assert_eq!(crates_io(&http(), &api, CRATE_FILES, "none", None).await.unwrap_err(), "crates.io names no version of none");
        let krate = |name: &str, version: Option<&str>| Some(Repo::Crate { name: name.into(), version: version.map(Into::into) });
        assert_eq!(repo("https://crates.io/crates/serde"), krate("serde", None));
        assert_eq!(repo("https://crates.io/crates/serde/versions"), krate("serde", None));
        assert_eq!(repo("https://crates.io/crates/serde/1.0.200/dependencies"), krate("serde", Some("1.0.200")));
        for other in ["https://crates.io/search?q=serde", "https://crates.io/categories/x", "https://crates.io/crates"] {
            assert_eq!(repo(other), None, "{other}");
        }
        let link = Url::parse("https://crates.io/crates/serde").unwrap();
        assert!(lists(&link) && !handles(&link));
    }

    /// GitLab (gitlab.com and self-hosted), Bitbucket and Gitea links are told by their shape; a
    /// repository is its default branch's zip, with no request made.
    #[tokio::test]
    async fn other_code_hosts_are_told_by_their_shape() {
        let origin = |host: &str| format!("https://{host}");
        let gl = |rest: &[&str]| Some(Repo::GitLabTree { origin: origin("gitlab.com"), project: "g/sub/p".into(), rest: strings(rest) });
        assert_eq!(repo("https://gitlab.com/g/sub/p/-/tree/feature/x/My%20Docs?ref_type=heads"), gl(&["feature", "x", "My Docs"]));
        assert_eq!(
            repo("https://code.example.org/g/p/-/tree/main"),
            Some(Repo::GitLabTree { origin: origin("code.example.org"), project: "g/p".into(), rest: strings(&["main"]) })
        );
        let release = |tag: Option<&str>| Some(Repo::GitLabRelease { origin: origin("gitlab.gnome.org"), project: "GNOME/gtk".into(), tag: tag.map(Into::into) });
        assert_eq!(repo("https://gitlab.gnome.org/GNOME/gtk/-/releases/4.16.0"), release(Some("4.16.0")));
        assert_eq!(repo("https://gitlab.gnome.org/GNOME/gtk/-/releases/permalink/latest"), release(None));
        assert_eq!(repo("https://gitlab.com/g/sub/p"), Some(Repo::GitLabRoot { origin: origin("gitlab.com"), project: "g/sub/p".into() }));
        let bb = |owner: &str, repo: &str| (owner.to_string(), repo.to_string());
        let (owner, name) = bb("o", "r");
        assert_eq!(repo("https://bitbucket.org/o/r"), Some(Repo::BitbucketRoot { owner: owner.clone(), repo: name.clone() }));
        assert_eq!(repo("https://bitbucket.org/o/r/src/main/doc/"), Some(Repo::BitbucketTree { owner: owner.clone(), repo: name.clone(), rest: strings(&["main", "doc"]) }));
        assert_eq!(repo("https://bitbucket.org/o/r/src/main"), Some(Repo::BitbucketTree { owner: owner.clone(), repo: name.clone(), rest: strings(&["main"]) }));
        assert_eq!(repo("https://bitbucket.org/o/r/downloads/"), Some(Repo::BitbucketDownloads { owner, repo: name }));
        let src = |host: &str, kind: &str, rest: &[&str]| Some(Repo::GiteaSrc { origin: origin(host), owner: "o".into(), repo: "r".into(), kind: kind.into(), rest: strings(rest) });
        assert_eq!(repo("https://codeberg.org/o/r/src/branch/main/docs"), src("codeberg.org", "branch", &["main", "docs"]));
        assert_eq!(repo("https://git.example.org/o/r/src/tag/v1/a.zip"), src("git.example.org", "tag", &["v1", "a.zip"]));
        let gitea = |tag: Option<&str>| Some(Repo::GiteaRelease { origin: origin("codeberg.org"), owner: "o".into(), repo: "r".into(), tag: tag.map(Into::into) });
        assert_eq!(repo("https://codeberg.org/o/r/releases/tag/v1.0"), gitea(Some("v1.0")));
        assert_eq!(repo("https://codeberg.org/o/r/releases/latest"), gitea(None));
        assert_eq!(repo("https://gitea.com/o/r"), Some(Repo::GiteaRoot { origin: origin("gitea.com"), owner: "o".into(), repo: "r".into() }));
        for other in [
            // Files, which the code host resolver takes, and pages of the sites themselves.
            "https://gitlab.com/g/p/-/blob/main/a.zip",
            "https://gitlab.com/g/p/-/raw/main/a.zip",
            "https://gitlab.com/g/p/-/releases",
            "https://gitlab.com/explore/projects",
            "https://gitlab.com/g",
            "https://about.gitlab.com/pricing",
            "https://bitbucket.org/o/r/src/main/a.zip",
            "https://bitbucket.org/o/r/downloads/a.zip",
            "https://bitbucket.org/product/pricing",
            "https://codeberg.org/o/r/src/branch",
            "https://codeberg.org/o/r/media/branch/main/a.zip",
            "https://codeberg.org/user/login",
            "https://codeberg.org/o/r/releases/download/v1/a.zip",
            "https://forgejo.org/docs/latest",
            "https://example.com/o/r",
            "https://example.com/o/r/releases/tag/v1",
        ] {
            assert_eq!(repo(other), None, "{other}");
        }
        let gitea_src = Url::parse("https://codeberg.org/o/r/src/branch/main/a.zip").unwrap();
        assert!(lists(&gitea_src) && !handles(&gitea_src), "a file's /src/ link is listed, and as one download it is the code host resolver's");
        for (root, zip) in [
            ("https://gitlab.com/g/sub/My%20P", "https://gitlab.com/g/sub/My%20P/-/archive/HEAD/My%20P-HEAD.zip"),
            ("https://bitbucket.org/o/r", "https://bitbucket.org/o/r/get/HEAD.zip"),
            ("https://codeberg.org/o/r", "https://codeberg.org/o/r/archive/HEAD.zip"),
        ] {
            let root = Url::parse(root).unwrap();
            assert_eq!(resolve(&http(), &root, None).await.unwrap()[0].as_str(), zip);
            assert_eq!(list(&http(), &root, None, &[]).await.unwrap()[0].urls[0].as_str(), zip);
        }
        assert_eq!(list(&http(), &Url::parse("https://gitlab.com/g/sub/My%20P").unwrap(), None, &[]).await.unwrap()[0].name, Some("My P.zip".into()));
    }

    fn gitlab(path: &str) -> Reply {
        let tree = "/api/v4/projects/g%2Fsub%2Fp/repository/tree?";
        match path.strip_prefix(tree) {
            Some("ref=feature%2Fx&path=docs&recursive=true&per_page=100&pagination=keyset") => (
                200,
                format!("Content-Type: application/json\r\nLink: <{{base}}{tree}page_token=2>; rel=\"next\"\r\n"),
                br#"[{"type":"tree","path":"docs/img"},{"type":"blob","path":"docs/a.md"},{"type":"blob","path":"docs/../../evil.exe"}]"#.to_vec(),
            ),
            Some("page_token=2") => json_reply(r#"[{"type":"blob","path":"docs/img/b c.png"}]"#),
            _ => match path {
                "/api/v4/projects/g%2Fsub%2Fp/releases/permalink/latest" => json_reply(
                    r#"{"tag_name":"v2.0","assets":{"sources":[{"format":"zip","url":"https://x/src.zip"}],"links":[
                        {"name":"Linux build","url":"https://cdn.example/files/app-linux.tar.gz","direct_asset_url":"{base}/g/sub/p/-/releases/v2.0/downloads/app-linux.tar.gz"},
                        {"name":"checksums","url":"https://cdn.example/files/SHA256SUMS"},
                        {"name":"docs","url":"https://cdn.example/"}]}}"#,
                ),
                "/api/v4/projects/g%2Fsub%2Fp/releases/v0" => json_reply(r#"{"tag_name":"v0","assets":{"links":[]}}"#),
                _ => (404, String::new(), br#"{"message":"404 Tree Not Found"}"#.to_vec()),
            },
        }
    }

    /// A GitLab folder is listed at the ref GitLab knows (the shortest start of the rest of the
    /// link), page after page, each file its /-/raw/ link, subfolders kept, names that would leave
    /// the folder left out; a release is one task per asset link, named after its file, on the
    /// project's host with the user's Authorization and elsewhere without.
    #[tokio::test]
    async fn gitlab_folders_and_releases() {
        let (origin, hits) = serve_naming_itself(gitlab).await;
        let tasks = gitlab_tree(&http(), None, &origin, "g/sub/p", &strings(&["feature", "x", "docs"])).await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap())).collect();
        assert_eq!(
            listed,
            [
                (format!("{origin}/g/sub/p/-/raw/feature/x/docs/a.md"), PathBuf::from("a.md")),
                (format!("{origin}/g/sub/p/-/raw/feature/x/docs/img/b%20c.png"), PathBuf::from("img").join("b c.png")),
            ]
        );
        assert!(tasks.iter().all(|t| t.folder == Some("docs".into())));
        assert_eq!(hits.lock().get("/api/v4/projects/g%2Fsub%2Fp/repository/tree?ref=feature&path=x%2Fdocs&recursive=true&per_page=100&pagination=keyset"), Some(&1));
        let err = gitlab_tree(&http(), None, &origin, "g/sub/p", &strings(&["nope"])).await.unwrap_err();
        assert!(err.contains("no such folder in g/sub/p"), "{err}");

        let tasks = gitlab_release(&http(), None, &origin, "g/sub/p", None).await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone(), t.from_document)).collect();
        assert_eq!(
            listed,
            [
                (format!("{origin}/g/sub/p/-/releases/v2.0/downloads/app-linux.tar.gz"), Some("app-linux.tar.gz".into()), false),
                ("https://cdn.example/files/SHA256SUMS".to_string(), Some("SHA256SUMS".into()), true),
                ("https://cdn.example/".to_string(), Some("docs".into()), true),
            ]
        );
        assert!(tasks.iter().all(|t| t.folder == Some("p v2.0".into())));
        assert!(gitlab_release(&http(), None, &origin, "g/sub/p", Some("v0")).await.unwrap_err().contains("source code"));
        assert!(gitlab_release(&http(), None, &origin, "g/sub/p", Some("gone")).await.unwrap_err().contains("no such release"));
    }

    /// A server whose answers name its own address (`{base}` in them), as APIs that give their
    /// next page's or a file's full link do.
    async fn serve_naming_itself(handler: fn(&str) -> Reply) -> (String, std::sync::Arc<parking_lot::Mutex<HashMap<String, usize>>>) {
        let base = std::sync::Arc::new(std::sync::OnceLock::<String>::new());
        let named = std::sync::Arc::clone(&base);
        let (addr, hits) = serve(move |path, _| {
            let (status, headers, body) = handler(path);
            let base = named.get().unwrap();
            (status, headers.replace("{base}", base), String::from_utf8(body).unwrap().replace("{base}", base).into_bytes())
        })
        .await;
        base.set(format!("http://{addr}")).unwrap();
        (format!("http://{addr}"), hits)
    }

    fn bitbucket(path: &str) -> Reply {
        match path {
            "/repositories/o/r/src/main/doc/?max_depth=100&pagelen=100" => json_reply(
                r#"{"values":[{"type":"commit_directory","path":"doc/sub"},{"type":"commit_file","path":"doc/a.txt","size":3}],
                    "next":"{base}/repositories/o/r/src/main/doc/?max_depth=100&pagelen=100&page=2"}"#,
            ),
            "/repositories/o/r/src/main/doc/?max_depth=100&pagelen=100&page=2" => {
                json_reply(r#"{"values":[{"type":"commit_file","path":"doc/sub/b c.txt","size":5}],"next":"https://elsewhere.example/x"}"#)
            }
            "/repositories/o/r/downloads?pagelen=100" => {
                json_reply(r#"{"values":[{"name":"r-1.0.tar.gz","size":7,"type":"download"},{"name":"..","type":"download"}]}"#)
            }
            _ => (404, String::new(), br#"{"type":"error","error":{"message":"Commit not found"}}"#.to_vec()),
        }
    }

    /// A Bitbucket folder is listed at the ref Bitbucket knows, page after page on its API's own
    /// host, each file its /raw/ link with its size; Downloads are one task per file.
    #[tokio::test]
    async fn bitbucket_folders_and_downloads() {
        let (api, hits) = serve_naming_itself(bitbucket).await;
        let tasks = bitbucket_tree(&http(), None, &api, "o", "r", &strings(&["main", "doc"])).await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap(), t.size)).collect();
        assert_eq!(
            listed,
            [
                ("https://bitbucket.org/o/r/raw/main/doc/a.txt".to_string(), PathBuf::from("a.txt"), Some(3)),
                ("https://bitbucket.org/o/r/raw/main/doc/sub/b%20c.txt".to_string(), PathBuf::from("sub").join("b c.txt"), Some(5)),
            ]
        );
        assert!(tasks.iter().all(|t| t.folder == Some("doc".into())));
        assert_eq!(hits.lock().len(), 2, "the page on another host was not asked for");
        let err = bitbucket_tree(&http(), None, &api, "o", "r", &strings(&["nope", "doc"])).await.unwrap_err();
        assert!(err.contains("no such folder in o/r"), "{err}");
        let downloads = bitbucket_downloads(&http(), None, &api, "o", "r").await.unwrap();
        let listed: Vec<_> = downloads.iter().map(|t| (t.urls[0].as_str(), t.name.clone().unwrap(), t.size, t.folder.clone().unwrap())).collect();
        assert_eq!(listed, [("https://bitbucket.org/o/r/downloads/r-1.0.tar.gz", PathBuf::from("r-1.0.tar.gz"), Some(7), PathBuf::from("r downloads"))]);
    }

    fn gitea(path: &str) -> Reply {
        match path {
            "/api/v1/repos/o/r/git/trees/feature?recursive=true&per_page=1000&page=1" => {
                (400, "Content-Type: application/json\r\n".into(), br#"{"message":"sha not supported: feature"}"#.to_vec())
            }
            "/api/v1/repos/o/r/git/trees/feature%2Fx?recursive=true&per_page=1000&page=1" => json_reply(
                r#"{"total_count":4,"truncated":true,"page":1,"tree":[
                    {"path":"docs","type":"tree"},{"path":"docs/a.md","type":"blob","size":3},{"path":"docs/img/b c.png","type":"blob","size":5}]}"#,
            ),
            "/api/v1/repos/o/r/git/trees/feature%2Fx?recursive=true&per_page=1000&page=2" => {
                json_reply(r#"{"total_count":4,"truncated":true,"page":2,"tree":[{"path":"README.md","type":"blob","size":9}]}"#)
            }
            "/api/v1/repos/o/r/releases/tags/v1.0" => json_reply(
                r#"{"tag_name":"v1.0","assets":[{"name":"app.zip","size":7,"browser_download_url":"{base}/o/r/releases/download/v1.0/app.zip"}]}"#,
            ),
            _ => (404, String::new(), br#"{"message":"not found"}"#.to_vec()),
        }
    }

    /// A Gitea /src/ link is a folder (every file under it, each its /media/ link with its size)
    /// or a file (its one task), at the ref the repository has (it answers 400 to one it does not),
    /// read page after page; a release is one task per asset.
    #[tokio::test]
    async fn gitea_folders_files_and_releases() {
        let (origin, _) = serve_naming_itself(gitea).await;
        let tasks = gitea_src(&http(), None, &origin, "o", "r", "branch", &strings(&["feature", "x", "docs"])).await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap(), t.size)).collect();
        assert_eq!(
            listed,
            [
                (format!("{origin}/o/r/media/branch/feature/x/docs/a.md"), PathBuf::from("a.md"), Some(3)),
                (format!("{origin}/o/r/media/branch/feature/x/docs/img/b%20c.png"), PathBuf::from("img").join("b c.png"), Some(5)),
            ]
        );
        assert!(tasks.iter().all(|t| t.folder == Some("docs".into())));
        let file = gitea_src(&http(), None, &origin, "o", "r", "branch", &strings(&["feature", "x", "README.md"])).await.unwrap();
        assert_eq!(file, [Task { urls: vec![Url::parse(&format!("{origin}/o/r/media/branch/feature/x/README.md")).unwrap()], name: Some("README.md".into()), size: Some(9), ..Task::default() }]);
        let err = gitea_src(&http(), None, &origin, "o", "r", "branch", &strings(&["feature", "x", "nope"])).await.unwrap_err();
        assert!(err.contains("no such file or folder in o/r"), "{err}");
        let release = gitea_release(&http(), None, &origin, "o", "r", Some("v1.0")).await.unwrap();
        assert_eq!((release[0].name.clone(), release[0].size, release[0].folder.clone()), (Some("app.zip".into()), Some(7), Some("r v1.0".into())));
        assert!(!release[0].from_document, "the site's own host gets the user's Authorization");
        assert!(gitea_release(&http(), None, &origin, "o", "r", None).await.unwrap_err().contains("no such release"));
    }

    /// A token pasted with the link goes to the service's API with the listing.
    #[tokio::test]
    async fn a_pasted_token_goes_with_the_listing() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let n = socket.read(&mut request).await.unwrap();
            let body = br#"{"name":"M","modelVersions":[{"id":2,"name":"v2","files":[{"id":20,"name":"m.bin","downloadUrl":"https://civitai.com/api/download/models/2?fileId=20"}]}]}"#;
            let head = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
            String::from_utf8_lossy(&request[..n]).to_ascii_lowercase()
        });
        assert_eq!(civitai(&http(), Some("Bearer t0ken"), &api, 7, None).await.unwrap().len(), 1);
        assert!(server.await.unwrap().contains("authorization: bearer t0ken"));
    }

    /// What `link` lists, through the client the front ends list with (GitHub wants its user agent).
    async fn live(link: &str) -> Result<Vec<Task>, String> {
        list(&crate::ingest::descriptor_client(None).unwrap(), &Url::parse(link).unwrap(), None, &[]).await
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

    /// Live: a small archive.org item lists its uploaded files with their checksums, and none of
    /// the copies and records archive.org made.
    #[tokio::test]
    #[ignore]
    async fn live_archive_item() {
        let tasks = live("https://archive.org/details/nasa").await.unwrap();
        let globe = tasks.iter().find(|t| t.name == Some("globe_west_540.jpg".into())).unwrap();
        assert_eq!(globe.checksum.as_deref(), Some("sha1:3e20a009994405f535cdf07cdc2974cef2fce8f2"));
        assert_eq!(globe.size, Some(66065));
        assert!(tasks.iter().all(|t| !t.name.as_ref().unwrap().to_string_lossy().contains("thumb") && t.name != Some("nasa_meta.xml".into())), "{tasks:?}");
    }

    /// Live: a PyPI release lists its wheel and source files with their SHA-256.
    #[tokio::test]
    #[ignore]
    async fn live_pypi() {
        let tasks = live("https://pypi.org/project/six/1.16.0/").await.unwrap();
        let wheel = tasks.iter().find(|t| t.name == Some("six-1.16.0-py2.py3-none-any.whl".into())).unwrap();
        assert_eq!(wheel.checksum.as_deref(), Some("sha256:8abb2f1d86890a2dfb989f9a77cfcfd3e47c2a354b01111771326f8aa26e0254"));
        assert_eq!((tasks.len(), wheel.size), (2, Some(11053)));
        assert!(!live("https://pypi.org/project/six/").await.unwrap().is_empty());
    }

    /// Live: an npm package, scoped or not, is its tarball with its SHA-512.
    #[tokio::test]
    #[ignore]
    async fn live_npm() {
        let [pad] = &live("https://www.npmjs.com/package/left-pad/v/1.3.0").await.unwrap()[..] else { panic!("one tarball") };
        assert_eq!(pad.urls[0].as_str(), "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz");
        assert!(pad.checksum.as_deref().unwrap().starts_with("sha512:5c8e4c3f354d0298"));
        let [node] = &live("https://www.npmjs.com/package/@types/node").await.unwrap()[..] else { panic!("one tarball") };
        assert!(node.urls[0].as_str().starts_with("https://registry.npmjs.org/@types/node/-/node-"), "{node:?}");
    }

    /// Live: a crate is its .crate file on crates.io's file host, with its SHA-256.
    #[tokio::test]
    #[ignore]
    async fn live_crates_io() {
        let [old] = &live("https://crates.io/crates/cfg-if/1.0.0").await.unwrap()[..] else { panic!("one file") };
        assert_eq!(old.urls[0].as_str(), "https://static.crates.io/crates/cfg-if/cfg-if-1.0.0.crate");
        assert_eq!((old.size, old.checksum.as_deref()), (Some(7934), Some("sha256:baf1de4339761588bc0619e3cbc0120ee582ebb74b53b4efbf79117bd2da40fd")));
        let [latest] = &live("https://crates.io/crates/cfg-if").await.unwrap()[..] else { panic!("one file") };
        assert!(latest.checksum.is_some() && latest.urls[0].as_str().ends_with(".crate"), "{latest:?}");
    }

    /// Live: a GitLab folder lists its files, and its latest release its asset links.
    #[tokio::test]
    #[ignore]
    async fn live_gitlab() {
        let tasks = live("https://gitlab.com/gitlab-org/gitlab-runner/-/tree/main/docs/install").await.unwrap();
        assert!(tasks.iter().any(|t| t.urls[0].as_str() == "https://gitlab.com/gitlab-org/gitlab-runner/-/raw/main/docs/install/_index.md"), "{tasks:?}");
        let release = live("https://gitlab.com/gitlab-org/gitlab-runner/-/releases/permalink/latest").await.unwrap();
        assert!(release.len() > 1 && release.iter().all(|t| t.name.is_some()), "{release:?}");
    }

    /// Live: a Bitbucket folder lists its files with their sizes, and Downloads its files.
    #[tokio::test]
    #[ignore]
    async fn live_bitbucket() {
        let tasks = live("https://bitbucket.org/multicoreware/x265_git/src/master/doc/").await.unwrap();
        let rst = tasks.iter().find(|t| t.name == Some(PathBuf::from("reST").join("api.rst"))).unwrap();
        assert!(rst.size.is_some() && rst.urls[0].as_str() == "https://bitbucket.org/multicoreware/x265_git/raw/master/doc/reST/api.rst", "{rst:?}");
        let downloads = live("https://bitbucket.org/multicoreware/x265_git/downloads/").await.unwrap();
        assert!(downloads.iter().any(|t| t.name.as_ref().is_some_and(|n| n.to_string_lossy().ends_with(".tar.gz"))), "{downloads:?}");
    }

    /// Live: a Codeberg folder lists its files, a file's /src/ link is that file, and the latest
    /// release lists its assets.
    #[tokio::test]
    #[ignore]
    async fn live_codeberg() {
        let tasks = live("https://codeberg.org/mikf/gallery-dl/src/branch/master/docs").await.unwrap();
        assert!(tasks.len() > 1 && tasks.iter().all(|t| t.folder == Some("docs".into())), "{tasks:?}");
        let file = live("https://codeberg.org/mikf/gallery-dl/src/branch/master/README.rst").await.unwrap();
        assert_eq!(file[0].urls[0].as_str(), "https://codeberg.org/mikf/gallery-dl/media/branch/master/README.rst");
        let release = live("https://codeberg.org/mikf/gallery-dl/releases/latest").await.unwrap();
        assert!(release.iter().any(|t| t.name.as_ref().is_some_and(|n| n.to_string_lossy().ends_with(".whl"))), "{release:?}");
    }

    /// Live: a Civitai model lists its newest version's files.
    #[tokio::test]
    #[ignore]
    async fn live_civitai_model() {
        let tasks = live("https://civitai.com/models/4201").await.unwrap();
        assert!(tasks.iter().all(|t| t.urls[0].as_str().starts_with("https://civitai.com/api/download/models/")), "{tasks:?}");
    }
}
