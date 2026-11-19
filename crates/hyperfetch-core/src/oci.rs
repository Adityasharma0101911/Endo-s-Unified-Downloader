//! Container images from a registry (Docker Hub, GitHub's ghcr.io, Quay, or any `docker://`
//! registry): `docker.io/library/alpine:3.20`, `ghcr.io/owner/image@sha256:...`, their web pages,
//! public images only, through the anonymous pull token the registry's `Bearer` challenge names.
//!
//! The image index is read, the build for this machine (or `DownloadOptions::platform`) picked,
//! and its config and layers downloaded as files of their own, each checked against its digest,
//! from wherever the registry redirects (a CDN, which never gets the token). An expired token is
//! fetched again and the blob resumed. The result is an OCI image layout folder (`oci-layout`,
//! `index.json`, `blobs/sha256/...`), which `skopeo` and `podman` take as it is and `docker load`
//! as a tar of it.
//!
//! Ollama's models come from its registry the same way (`ollama.com/library/llama3.2:3b`,
//! `ollama.com/<user>/<model>`, `ollama://<model>[:tag]`): the model's layers are files of their
//! own, named so that they can be used as they are (`llama3.2-3b.gguf`, `template.txt`,
//! `params.json`, `license.txt`), unless the save folder is an Ollama models folder (it holds
//! `blobs` and `manifests`), which then takes them as Ollama keeps them (`blobs/sha256-<hex>`,
//! `manifests/registry.ollama.ai/library/<model>/<tag>`), ready for Ollama to run.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use reqwest::header::{ACCEPT, AUTHORIZATION, WWW_AUTHENTICATE};
use reqwest::StatusCode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{DownloadEngine, DownloadOptions, EngineSnapshot, CANCELLED};

const SCHEME: &str = "docker";
/// Every kind of manifest and index the registries hand out.
const MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, \
                         application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
/// Largest manifest or token answer read.
const MAX_MANIFEST: usize = 4 << 20;
/// Most layers of one image.
const MAX_LAYERS: usize = 1000;
/// Fresh tokens a blob may need while it downloads.
const TOKEN_TRIES: u32 = 3;
/// Ollama's registry, and the start of the media types of its models' layers.
const OLLAMA: &str = "registry.ollama.ai";
const OLLAMA_LAYER: &str = "application/vnd.ollama.image.";
/// First path parts of ollama.com that are its own pages, not users.
const OLLAMA_PAGES: &[&str] = &["blog", "docs", "download", "models", "public", "search", "settings", "signin", "signup", "turbo", "api"];

/// An image: its registry (`host[:port]`), repository, and tag or digest.
#[derive(Debug, Clone, PartialEq)]
struct Image {
    registry: String,
    repo: String,
    tag: String,
    digest: Option<String>,
}

impl Image {
    /// The `docker://` link the engine downloads it by.
    fn link(&self) -> Option<Url> {
        let at = match &self.digest {
            Some(digest) => format!("@{digest}"),
            None => format!(":{}", self.tag),
        };
        Url::parse(&format!("{SCHEME}://{}/{}{at}", self.registry, self.repo)).ok()
    }

    /// Where its registry's API is: Docker Hub's own host, plain HTTP for one on this computer
    /// (as Docker does), else HTTPS.
    fn api(&self) -> String {
        let host = self.registry.rsplit_once(':').map_or(self.registry.as_str(), |(host, _)| host);
        match self.registry.as_str() {
            "docker.io" => "https://registry-1.docker.io".to_string(),
            _ if host == "localhost" || host == "127.0.0.1" || host == "[::1]" => format!("http://{}", self.registry),
            _ => format!("https://{}", self.registry),
        }
    }

    /// Its tag, or its digest when it was named by one.
    fn reference(&self) -> &str {
        self.digest.as_deref().unwrap_or(&self.tag)
    }

    /// Its full name, as `docker load` names what it loads.
    fn name(&self) -> String {
        format!("{}/{}:{}", self.registry, self.repo, self.tag)
    }

    /// The folder it downloads into: `alpine_3.20`, or `alpine_sha256-<12 hex>`.
    fn folder(&self) -> String {
        let last = self.repo.rsplit('/').next().unwrap_or(&self.repo);
        let what = match &self.digest {
            Some(digest) => digest.replace(':', "-").chars().take(19).collect(),
            None => self.tag.clone(),
        };
        crate::engine::sanitize_component(&format!("{last}_{what}"))
    }
}

/// The `docker://` link of the image `token` names: a reference on Docker Hub, ghcr.io or Quay
/// (`docker.io/library/alpine:3.20`, `ghcr.io/owner/name@sha256:...`), any registry's with
/// `docker://`, or an image's web page on Docker Hub, GitHub or Quay. None for anything else.
pub fn reference(token: &str) -> Option<Url> {
    let token = token.trim();
    if let Some(model) = token.strip_prefix("ollama://") {
        return ollama(model.trim_end_matches('/'))?.link();
    }
    let image = match token.strip_prefix("docker://") {
        Some(rest) => {
            let (registry, path) = rest.split_once('/')?;
            parse(registry, path)?
        }
        None if token.starts_with("http://") || token.starts_with("https://") => page(&Url::parse(token).ok()?)?,
        None => {
            let (registry, path) = token.split_once('/')?;
            let known = ["docker.io", "index.docker.io", "registry-1.docker.io", "ghcr.io", "quay.io", OLLAMA];
            known.contains(&registry.to_ascii_lowercase().as_str()).then(|| parse(registry, path))??
        }
    };
    image.link()
}

/// Whether `url` is an image's `docker://` link (see [`reference`]).
pub(crate) fn handles(url: &Url) -> bool {
    url.scheme() == SCHEME
}

/// The image `name[:tag][@digest]` on `registry`. Docker Hub's official images are under
/// `library/`.
fn parse(registry: &str, path: &str) -> Option<Image> {
    let registry = match registry.to_ascii_lowercase().as_str() {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => "docker.io".to_string(),
        other => other.to_string(),
    };
    let (rest, digest) = match path.split_once('@') {
        Some((rest, digest)) => (rest, Some(digest.to_ascii_lowercase())),
        None => (path, None),
    };
    if digest.as_deref().is_some_and(|d| d.strip_prefix("sha256:").is_none_or(|hex| hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()))) {
        return None;
    }
    let (repo, tag) = match rest.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => (repo, tag),
        _ => (rest, "latest"),
    };
    let tag_ok = !tag.is_empty() && tag.len() <= 128 && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) && !tag.starts_with(['.', '-']);
    let part_ok = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    if !tag_ok || !repo.split('/').all(part_ok) {
        return None;
    }
    let repo = match (registry.as_str(), repo.contains('/')) {
        ("docker.io", false) => format!("library/{repo}"),
        _ => repo.to_string(),
    };
    Some(Image { registry, repo, tag: tag.to_string(), digest })
}

/// The image of a web page: Docker Hub's `/_/name` and `/r/owner/name`, GitHub's package pages
/// (`/owner/repo/pkgs/container/name`, `/users/owner/packages/container/package/name`) and
/// Quay's `/repository/org/name`, with `?tag=` when the page names one.
fn page(url: &Url) -> Option<Image> {
    let parts: Vec<String> = url.path_segments()?.filter(|s| !s.is_empty()).map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let tag = url.query_pairs().find(|(k, _)| k == "tag").map(|(_, v)| v.into_owned()).unwrap_or_else(|| "latest".to_string());
    let host = url.host_str()?.trim_start_matches("www.");
    let (registry, repo) = match (host, parts.iter().map(String::as_str).collect::<Vec<_>>().as_slice()) {
        ("hub.docker.com", ["_", name, ..]) => ("docker.io", format!("library/{name}")),
        ("hub.docker.com", ["r", owner, name, ..]) => ("docker.io", format!("{owner}/{name}")),
        ("github.com", [owner, _, "pkgs", "container", name, ..]) => ("ghcr.io", format!("{owner}/{name}")),
        ("github.com", ["users" | "orgs", owner, "packages", "container", "package", name, ..]) => ("ghcr.io", format!("{owner}/{name}")),
        ("quay.io", ["repository", org, name, ..]) => ("quay.io", format!("{org}/{name}")),
        ("ollama.com", [user, model, ..]) if !OLLAMA_PAGES.contains(user) => return ollama(&format!("{user}/{model}")),
        _ => return None,
    };
    parse(registry, &format!("{}:{tag}", repo.to_ascii_lowercase()))
}

/// The Ollama model `[user/]model[:tag]` (`library/` is Ollama's own models); a tag keeps its
/// case (`q4_K_M`).
fn ollama(model: &str) -> Option<Image> {
    let (name, tag) = model.rsplit_once(':').map_or((model, None), |(name, tag)| (name, Some(tag)));
    let name = name.to_ascii_lowercase();
    let name = if name.contains('/') { name } else { format!("library/{name}") };
    parse(OLLAMA, &tag.map_or(name.clone(), |tag| format!("{name}:{tag}")))
}

/// The build of `index` for `platform` (`os/arch[/variant]`, else this machine's Linux build):
/// its descriptor. An index without one says which builds it has.
fn choose(index: &Value, platform: Option<&str>) -> Result<Value, String> {
    let (os, arch, variant) = match platform.map(|p| p.split('/').collect::<Vec<_>>()) {
        Some(parts) if (2..=3).contains(&parts.len()) => (parts[0].to_string(), parts[1].to_string(), parts.get(2).map(|v| v.to_string())),
        Some(_) => return Err(format!("{}: a platform is os/arch or os/arch/variant, such as linux/arm64", platform.unwrap_or_default())),
        None => ("linux".to_string(), this_arch().to_string(), None),
    };
    let builds = index["manifests"].as_array().map(Vec::as_slice).unwrap_or_default();
    let of = |m: &Value, key: &str| m["platform"][key].as_str().unwrap_or_default().to_string();
    let found = builds.iter().find(|m| of(m, "os") == os && of(m, "architecture") == arch && variant.as_ref().is_none_or(|v| of(m, "variant") == *v));
    if let Some(build) = found {
        return Ok(build.clone());
    }
    let mut has: Vec<String> = builds
        .iter()
        .filter(|m| of(m, "os") != "unknown")
        .map(|m| [of(m, "os"), of(m, "architecture"), of(m, "variant")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/"))
        .collect();
    has.dedup();
    Err(format!("the image has no {os}/{arch}{} build; it has {} (choose one with --platform)", variant.map(|v| format!("/{v}")).unwrap_or_default(), has.join(", ")))
}

/// This machine's CPU as image platforms name it.
fn this_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "powerpc64" => "ppc64le",
        other => other,
    }
}

/// A registry asked as an anonymous puller: the token its challenge names, fetched once and again
/// when it expires.
struct Registry<'a> {
    http: reqwest::Client,
    image: &'a Image,
    token: Option<String>,
    /// The last `Bearer` challenge: realm, service and scope.
    challenge: Option<(String, Option<String>, String)>,
}

impl Registry<'_> {
    /// The manifest or index `reference` names: its bytes and media type, checked against its
    /// digest (the one asked for, or the one the registry gives).
    async fn manifest(&mut self, reference: &str) -> Result<(Vec<u8>, String), String> {
        let url = format!("{}/v2/{}/manifests/{reference}", self.image.api(), self.image.repo);
        let answer = self.get(&url).await?;
        match answer.status() {
            StatusCode::NOT_FOUND => return Err(format!("{} has no {}", self.image.name().rsplit_once(':').map_or("", |(n, _)| n), reference)),
            status if !status.is_success() => return Err(format!("the registry answered HTTP {} for the image's manifest", status.as_u16())),
            _ => {}
        }
        let named = answer.headers().get("docker-content-digest").and_then(|d| d.to_str().ok()).map(str::to_ascii_lowercase);
        let media = answer.headers().get(reqwest::header::CONTENT_TYPE).and_then(|t| t.to_str().ok()).unwrap_or_default().split(';').next().unwrap_or_default().trim().to_string();
        let bytes = read_capped(answer).await?;
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        let wanted = if reference.starts_with("sha256:") { Some(reference.to_ascii_lowercase()) } else { named.filter(|d| d.starts_with("sha256:")) };
        if wanted.is_some_and(|w| w != digest) {
            return Err(format!("the image's manifest does not match its digest ({digest})"));
        }
        Ok((bytes, media))
    }

    /// `url` asked with the token, fetching one first when the registry's challenge asks for it.
    async fn get(&mut self, url: &str) -> Result<reqwest::Response, String> {
        let unreachable = |e: reqwest::Error| format!("Cannot reach the registry: {}", e.without_url());
        for fresh in [false, true] {
            let mut request = self.http.get(url).header(ACCEPT, MANIFESTS);
            if let Some(token) = &self.token {
                request = request.header(AUTHORIZATION, sensitive(&format!("Bearer {token}"))?);
            }
            let answer = request.send().await.map_err(unreachable)?;
            if answer.status() != StatusCode::UNAUTHORIZED || fresh {
                return Ok(answer);
            }
            let challenge = answer.headers().get(WWW_AUTHENTICATE).and_then(|c| c.to_str().ok()).and_then(|c| bearer(c, &self.image.repo));
            self.challenge = Some(challenge.ok_or("the registry wants a sign-in, and only public images can be downloaded")?);
            self.refresh().await?;
        }
        unreachable!("the second try returns")
    }

    /// A new anonymous token from the last challenge's realm.
    async fn refresh(&mut self) -> Result<(), String> {
        let (realm, service, scope) = self.challenge.clone().ok_or("the registry named no way to get a token")?;
        let mut url = Url::parse(&realm).map_err(|_| "the registry named no usable token address".to_string())?;
        if !matches!(url.scheme(), "https" | "http") {
            return Err("the registry named no usable token address".to_string());
        }
        url.query_pairs_mut().extend_pairs(service.map(|s| ("service", s))).append_pair("scope", &scope);
        let answer = self.http.get(url).send().await.map_err(|e| format!("Cannot get a token from the registry: {}", e.without_url()))?;
        if !answer.status().is_success() {
            return Err(format!("the registry gave no token (HTTP {}): only public images can be downloaded", answer.status().as_u16()));
        }
        let body: Value = serde_json::from_slice(&read_capped(answer).await?).map_err(|_| "the registry's token answer cannot be read".to_string())?;
        let token = body["token"].as_str().or(body["access_token"].as_str()).filter(|t| !t.is_empty());
        self.token = Some(token.ok_or("the registry's token answer holds no token")?.to_string());
        Ok(())
    }
}

/// The realm, service and scope of a `Bearer` challenge; the scope is a pull of `repo` when the
/// challenge names none.
fn bearer(challenge: &str, repo: &str) -> Option<(String, Option<String>, String)> {
    let params = challenge.trim().strip_prefix("Bearer ").or_else(|| challenge.trim().strip_prefix("bearer "))?;
    let mut found = std::collections::HashMap::new();
    let mut rest = params;
    while let Some((key, after)) = rest.split_once('=') {
        let key = key.trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let (value, next) = match after.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
            None => after.split_once(',').unwrap_or((after, "")),
        };
        found.insert(key, value.to_string());
        rest = next;
    }
    let scope = found.remove("scope").unwrap_or_else(|| format!("repository:{repo}:pull"));
    Some((found.remove("realm")?, found.remove("service"), scope))
}

fn sensitive(value: &str) -> Result<reqwest::header::HeaderValue, String> {
    let mut value = reqwest::header::HeaderValue::from_str(value).map_err(|_| "the registry's token is not a usable header".to_string())?;
    value.set_sensitive(true);
    Ok(value)
}

async fn read_capped(answer: reqwest::Response) -> Result<Vec<u8>, String> {
    if answer.content_length().is_some_and(|n| n as usize > MAX_MANIFEST) {
        return Err("the registry's answer is too large".to_string());
    }
    let bytes = answer.bytes().await.map_err(|e| format!("Cannot read the registry's answer: {}", e.without_url()))?;
    if bytes.len() > MAX_MANIFEST {
        return Err("the registry's answer is too large".to_string());
    }
    Ok(bytes.to_vec())
}

/// Downloads the image `source` names into its OCI layout folder (see the module docs): the
/// folder `options.output_path` names, else one named after the image in `save_dir`. Whether
/// anything was downloaded, for the engine (see `DownloadEngine::unless_rerun`).
///
/// A boxed Send future of a declared type: the engine's run leads here again (for each blob).
pub(crate) fn run<'a>(
    source: &'a Url,
    options: &'a DownloadOptions,
    save_dir: &'a Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Pin<Box<dyn Future<Output = Result<(PathBuf, bool), String>> + Send + 'a>> {
    Box::pin(download(source, options, save_dir, snapshots, cancel))
}

async fn download(
    source: &Url,
    options: &DownloadOptions,
    save_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let path = percent_encoding::percent_decode_str(source.path().trim_start_matches('/')).decode_utf8_lossy().into_owned();
    let host = match source.port() {
        Some(port) => format!("{}:{port}", source.host_str().unwrap_or_default()),
        None => source.host_str().unwrap_or_default().to_string(),
    };
    let image = parse(&host, &path).ok_or_else(|| format!("{source} names no container image"))?;
    let http = crate::ingest::descriptor_client(options.proxy.as_deref())?;
    let mut registry = Registry { http, image: &image, token: None, challenge: None };
    let (mut manifest, mut media) = registry.manifest(image.reference()).await?;
    let mut doc: Value = serde_json::from_slice(&manifest).map_err(|_| "the image's manifest cannot be read".to_string())?;
    let mut platform = Value::Null;
    if doc["manifests"].is_array() {
        let build = choose(&doc, options.platform.as_deref())?;
        let digest = build["digest"].as_str().ok_or("the image's index names a build without its digest")?.to_string();
        platform = build["platform"].clone();
        (manifest, media) = registry.manifest(&digest).await?;
        doc = serde_json::from_slice(&manifest).map_err(|_| "the image's manifest cannot be read".to_string())?;
    }
    if media.is_empty() || media == "application/json" {
        media = doc["mediaType"].as_str().unwrap_or("application/vnd.oci.image.manifest.v1+json").to_string();
    }
    let layers = doc["layers"].as_array().ok_or("the image's manifest lists no layers")?;
    if layers.len() > MAX_LAYERS {
        return Err("the image has too many layers".to_string());
    }
    let blobs: Vec<(String, u64, &str)> = std::iter::once(&doc["config"])
        .chain(layers)
        .map(|blob| {
            let digest = blob["digest"].as_str().filter(|d| d.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())));
            Some((digest?.to_ascii_lowercase(), blob["size"].as_u64()?, blob["mediaType"].as_str().unwrap_or_default()))
        })
        .collect::<Option<_>>()
        .ok_or("the image's manifest names a blob without a SHA-256 digest and size")?;

    // An Ollama model goes into the Ollama models folder picked as the save folder, else into
    // files of its own (see the module docs).
    let model = blobs.iter().any(|(_, _, media)| media.starts_with(OLLAMA_LAYER));
    let models = Some(save_dir).filter(|dir| model && dir.join("blobs").is_dir() && dir.join("manifests").is_dir());
    let folder = match (&options.output_path, models) {
        (_, Some(models)) => models.to_path_buf(),
        (Some(path), None) if !crate::engine::is_dir_target(path) => path.clone(),
        _ => save_dir.join(image.folder()),
    };
    let named = model_files(&image, &blobs);
    let total = blobs.iter().map(|(_, size, _)| size).sum::<u64>();
    let (mut done, mut fresh) = (0, false);
    for (n, (digest, size, _)) in blobs.iter().enumerate() {
        let hex = &digest["sha256:".len()..];
        let path = match (models, model) {
            (Some(_), _) => folder.join("blobs").join(format!("sha256-{hex}")),
            (None, true) => folder.join(&named[n]),
            (None, false) => folder.join("blobs").join("sha256").join(hex),
        };
        if !holds(&path, hex).await {
            fetch_blob(&mut registry, digest, &path, options, crate::p2p::Progress { total, before: done, target: folder.clone(), torrent: false }, &snapshots, &cancel).await?;
            fresh = true;
        }
        done += size;
    }
    if models.is_some() {
        let mut at = folder.join("manifests").join(crate::engine::sanitize_component(&image.registry));
        image.repo.split('/').chain([image.tag.as_str()]).for_each(|part| at.push(crate::engine::sanitize_component(part)));
        let failed = |e: std::io::Error| format!("Cannot write the model's manifest {}: {e}", at.display());
        std::fs::create_dir_all(at.parent().unwrap_or(&folder)).map_err(failed)?;
        std::fs::write(&at, &manifest).map_err(failed)?;
    } else if !model {
        write_layout(&folder, &image, &manifest, &media, &platform)?;
    }
    Ok((folder, fresh))
}

/// The file names an Ollama model's `blobs` (its config, then its layers) take outside an Ollama
/// models folder: the model as `<name>-<tag>.gguf` (a projector or adapter beside it), its
/// template, system prompt and licences as text, its parameters and messages as JSON, each name
/// numbered when it comes again.
fn model_files(image: &Image, blobs: &[(String, u64, &str)]) -> Vec<String> {
    let last = image.repo.rsplit('/').next().unwrap_or(&image.repo);
    let base = crate::engine::sanitize_component(&format!("{last}-{}", image.tag));
    let mut seen = std::collections::HashMap::new();
    blobs
        .iter()
        .enumerate()
        .map(|(n, (_, _, media))| {
            let (stem, ext) = match media.strip_prefix(OLLAMA_LAYER) {
                _ if n == 0 => ("config".to_string(), "json"),
                Some("model") => (base.clone(), "gguf"),
                Some(kind @ ("projector" | "adapter")) => (format!("{base}-{kind}"), "gguf"),
                Some(kind @ ("params" | "messages")) => (kind.to_string(), "json"),
                Some(kind) => (crate::engine::sanitize_component(kind), "txt"),
                None => ("layer".to_string(), "bin"),
            };
            let count = seen.entry((stem.clone(), ext)).and_modify(|c| *c += 1).or_insert(1);
            match *count {
                1 => format!("{stem}.{ext}"),
                c => format!("{stem}-{c}.{ext}"),
            }
        })
        .collect()
}

/// Downloads the blob `digest` into `path`, checked against it, from wherever the registry sends
/// it; the token goes to the registry alone (`DownloadOptions::secret_headers`), and is fetched
/// again when it expired meanwhile.
async fn fetch_blob(
    registry: &mut Registry<'_>,
    digest: &str,
    path: &Path,
    options: &DownloadOptions,
    progress: crate::p2p::Progress,
    snapshots: &Option<broadcast::Sender<EngineSnapshot>>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let url = Url::parse(&format!("{}/v2/{}/blobs/{digest}", registry.image.api(), registry.image.repo)).map_err(|e| e.to_string())?;
    let mut tries = 0;
    loop {
        let blob = DownloadOptions {
            output_path: Some(path.to_path_buf()),
            expected_checksum: Some(digest.to_string()),
            secret_headers: registry.token.iter().map(|t| ("Authorization".to_string(), format!("Bearer {t}"))).collect(),
            ..crate::p2p::network_only(options)
        };
        let shown = crate::p2p::Progress { target: progress.target.clone(), ..progress };
        let result = crate::p2p::run_nested(DownloadEngine::new(vec![url.clone()], blob), snapshots, shown, cancel).await;
        match result {
            _ if cancel.is_cancelled() => return Err(CANCELLED.to_string()),
            Ok(_) => return Ok(()),
            Err(e) if e.contains("401") && tries < TOKEN_TRIES && registry.challenge.is_some() => {
                tries += 1;
                tracing::info!("The registry's token expired; getting a new one");
                registry.refresh().await?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Whether `path` holds the blob whose SHA-256 is `hex` already.
async fn holds(path: &Path, hex: &str) -> bool {
    let (path, hex) = (path.to_path_buf(), hex.to_string());
    tokio::task::spawn_blocking(move || {
        let Ok(mut file) = std::fs::File::open(&path) else { return false };
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher).is_ok() && format!("{:x}", hasher.finalize()) == hex
    })
    .await
    .unwrap_or(false)
}

/// Writes the layout's `oci-layout`, the manifest's blob and `index.json`, naming the image.
fn write_layout(folder: &Path, image: &Image, manifest: &[u8], media: &str, platform: &Value) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("Cannot write the image's files in {}: {e}", folder.display());
    let digest = format!("{:x}", Sha256::digest(manifest));
    let blobs = folder.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).map_err(failed)?;
    std::fs::write(blobs.join(&digest), manifest).map_err(failed)?;
    std::fs::write(folder.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).map_err(failed)?;
    let mut entry = json!({
        "mediaType": media,
        "digest": format!("sha256:{digest}"),
        "size": manifest.len(),
        "annotations": {"org.opencontainers.image.ref.name": image.tag, "io.containerd.image.name": image.name()},
    });
    if !platform.is_null() {
        entry["platform"] = platform.clone();
    }
    let index = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [entry]});
    std::fs::write(folder.join("index.json"), serde_json::to_vec_pretty(&index).unwrap_or_default()).map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn references_and_pages_name_images() {
        let link = |t: &str| reference(t).map(|u| u.to_string());
        assert_eq!(link("docker.io/library/alpine:3.20").as_deref(), Some("docker://docker.io/library/alpine:3.20"));
        assert_eq!(link("docker.io/alpine").as_deref(), Some("docker://docker.io/library/alpine:latest"));
        assert_eq!(link("ghcr.io/owner/app").as_deref(), Some("docker://ghcr.io/owner/app:latest"));
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(link(&format!("quay.io/org/img@{digest}")), Some(format!("docker://quay.io/org/img@{digest}")));
        assert_eq!(link("docker://registry.example:5000/team/tool:v1").as_deref(), Some("docker://registry.example:5000/team/tool:v1"));
        assert_eq!(link("https://hub.docker.com/_/nginx").as_deref(), Some("docker://docker.io/library/nginx:latest"));
        assert_eq!(link("https://hub.docker.com/r/grafana/grafana/tags").as_deref(), Some("docker://docker.io/grafana/grafana:latest"));
        assert_eq!(link("https://github.com/Owner/repo/pkgs/container/App?tag=1.2").as_deref(), Some("docker://ghcr.io/owner/app:1.2"));
        assert_eq!(link("https://quay.io/repository/coreos/etcd?tab=tags").as_deref(), Some("docker://quay.io/coreos/etcd:latest"));
        for not in ["example.com/a/b", "docker.io/Bad/Name", "docker.io/a:b:c", "ghcr.io/a@sha256:12", "https://hub.docker.com/search", "alpine"] {
            assert_eq!(link(not), None, "{not}");
        }
        assert_eq!(link("ollama://llama3.2").as_deref(), Some("docker://registry.ollama.ai/library/llama3.2:latest"));
        assert_eq!(link("ollama://Llama3.2:3b-instruct-q4_K_M").as_deref(), Some("docker://registry.ollama.ai/library/llama3.2:3b-instruct-q4_K_M"));
        assert_eq!(link("https://ollama.com/library/qwen3:8b").as_deref(), Some("docker://registry.ollama.ai/library/qwen3:8b"));
        assert_eq!(link("https://ollama.com/library/llama3.2/tags").as_deref(), Some("docker://registry.ollama.ai/library/llama3.2:latest"));
        assert_eq!(link("https://ollama.com/huihui_ai/deepseek-r1-abliterated:14b").as_deref(), Some("docker://registry.ollama.ai/huihui_ai/deepseek-r1-abliterated:14b"));
        assert_eq!(link("registry.ollama.ai/library/smollm:135m").as_deref(), Some("docker://registry.ollama.ai/library/smollm:135m"));
        for not in ["https://ollama.com/search?q=llama", "https://ollama.com/download/windows", "https://ollama.com/library", "ollama://"] {
            assert_eq!(link(not), None, "{not}");
        }
        let image = parse("docker.io", "library/alpine:3.20").unwrap();
        assert_eq!((image.api(), image.folder()), ("https://registry-1.docker.io".to_string(), "alpine_3.20".to_string()));
        assert_eq!(parse("127.0.0.1:5000", "a/b").unwrap().api(), "http://127.0.0.1:5000");
    }

    #[test]
    fn challenges_and_platforms_are_read() {
        let found = bearer(r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:a/b:pull""#, "a/b");
        assert_eq!(found, Some(("https://auth.example/token".into(), Some("registry.example".into()), "repository:a/b:pull".into())));
        assert_eq!(bearer(r#"Bearer realm="https://t.example/""#, "x/y").unwrap().2, "repository:x/y:pull");
        assert_eq!(bearer("Basic realm=x", "x/y"), None);
        let index = json!({"manifests": [
            {"digest": "sha256:1", "platform": {"os": "linux", "architecture": "amd64"}},
            {"digest": "sha256:2", "platform": {"os": "linux", "architecture": "arm64", "variant": "v8"}},
            {"digest": "sha256:3", "platform": {"os": "unknown", "architecture": "unknown"}},
        ]});
        assert_eq!(choose(&index, Some("linux/arm64")).unwrap()["digest"], "sha256:2");
        assert_eq!(choose(&index, Some("linux/arm64/v8")).unwrap()["digest"], "sha256:2");
        let none = choose(&index, Some("windows/amd64")).unwrap_err();
        assert!(none.contains("no windows/amd64 build; it has linux/amd64, linux/arm64/v8"), "{none}");
        assert!(choose(&index, Some("linux")).is_err());
    }

    /// What the canned registry saw.
    #[derive(Default)]
    struct Seen {
        tokens: AtomicUsize,
        /// Blob requests with the first token, which expires after one.
        first_used: AtomicUsize,
        /// Requests the CDN got with an Authorization header.
        cdn_auth: AtomicUsize,
    }

    /// A registry on 127.0.0.1 whose blobs redirect to a CDN at `localhost`: anonymous tokens
    /// from its realm, the first of which expires after one blob request; `bad` is a blob whose
    /// bytes do not match its digest.
    async fn registry(blobs: Vec<(String, Vec<u8>)>, manifests: Vec<(String, (String, Vec<u8>))>, seen: Arc<Seen>) -> String {
        let cdn = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cdn_port = cdn.local_addr().unwrap().port();
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = api.local_addr().unwrap();
        let blobs = Arc::new(blobs);
        let (b, s) = (blobs.clone(), seen.clone());
        tokio::spawn(serve(cdn, move |head| {
            if head.to_ascii_lowercase().contains("\nauthorization:") {
                s.cdn_auth.fetch_add(1, Ordering::SeqCst);
            }
            let target = head.split(' ').nth(1).unwrap_or_default().to_string();
            match b.iter().find(|(d, _)| target == format!("/blob/{d}")) {
                Some((_, bytes)) => ranged(&head, bytes),
                None => reply("404 Not Found", "", b""),
            }
        }));
        let manifests = Arc::new(Mutex::new(manifests));
        tokio::spawn(serve(api, move |head| {
            let target = head.split(' ').nth(1).unwrap_or_default().to_string();
            if let Some(query) = target.strip_prefix("/token?") {
                assert!(query.contains("service=test") && query.contains("scope=repository%3Alib%2Fapp%3Apull"), "{query}");
                let n = seen.tokens.fetch_add(1, Ordering::SeqCst) + 1;
                return reply("200 OK", "Content-Type: application/json\r\n", format!(r#"{{"token":"t{n}","expires_in":300}}"#).as_bytes());
            }
            let auth = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("authorization: bearer ").map(|t| t.trim().to_string()));
            let blob = target.strip_prefix("/v2/lib/app/blobs/sha256:");
            let valid = match auth.as_deref() {
                Some("t1") if blob.is_some() => seen.first_used.fetch_add(1, Ordering::SeqCst) < 1,
                Some(t) => t.starts_with('t'),
                None => false,
            };
            if !valid {
                let challenge = format!("WWW-Authenticate: Bearer realm=\"http://{addr}/token\",service=\"test\",scope=\"repository:lib/app:pull\"\r\n");
                return reply("401 Unauthorized", &challenge, b"{}");
            }
            if let Some(hex) = blob {
                return reply("307 Temporary Redirect", &format!("Location: http://localhost:{cdn_port}/blob/sha256:{hex}\r\n"), b"");
            }
            let wanted = target.strip_prefix("/v2/lib/app/manifests/").unwrap_or_default();
            match manifests.lock().unwrap().iter().find(|(r, _)| r == wanted) {
                Some((_, (media, bytes))) => reply("200 OK", &format!("Content-Type: {media}\r\nDocker-Content-Digest: sha256:{}\r\n", hex(bytes)), bytes),
                None => reply("404 Not Found", "", b""),
            }
        }));
        format!("127.0.0.1:{}", addr.port())
    }

    fn reply(status: &str, extra: &str, body: &[u8]) -> Vec<u8> {
        [format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n", body.len()).as_bytes(), body].concat()
    }

    fn ranged(head: &str, bytes: &[u8]) -> Vec<u8> {
        let range = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("range: bytes=").map(str::to_string));
        let Some((start, end)) = range.and_then(|r| {
            let (a, b) = r.trim().split_once('-')?;
            let start = a.parse::<usize>().ok()?;
            Some((start, b.parse::<usize>().map_or(bytes.len() - 1, |e| e.min(bytes.len() - 1))))
        }) else {
            return reply("200 OK", "Accept-Ranges: bytes\r\n", bytes);
        };
        let extra = format!("Accept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{}\r\n", bytes.len());
        reply("206 Partial Content", &extra, &bytes[start..=end])
    }

    async fn serve(listener: tokio::net::TcpListener, answer: impl Fn(String) -> Vec<u8> + Send + Sync + 'static) {
        let answer = Arc::new(answer);
        while let Ok((mut socket, _)) = listener.accept().await {
            let answer = answer.clone();
            tokio::spawn(async move {
                let (mut head, mut buf) = (Vec::new(), [0u8; 4096]);
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = socket.write_all(&answer(String::from_utf8_lossy(&head).into_owned())).await;
            });
        }
    }

    /// The arm64 build of a two-platform image: its index read with an anonymous token, its
    /// config and layers checked against their digests from the CDN (which never sees the token),
    /// a new token fetched when the first expires, and the folder an OCI layout of that build. A
    /// layer whose bytes are not its digest's fails the download.
    #[tokio::test]
    async fn an_image_downloads_into_an_oci_layout() {
        let config = br#"{"architecture":"arm64","os":"linux"}"#.to_vec();
        let layer = crate::netsim::file(300_000);
        let wrong = b"not what the digest says".to_vec();
        let blob = |bytes: &[u8]| json!({"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": format!("sha256:{}", hex(bytes)), "size": bytes.len()});
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": format!("sha256:{}", hex(&config)), "size": config.len()},
            "layers": [blob(&layer)],
        }))
        .unwrap();
        let broken = serde_json::to_vec(&json!({"schemaVersion": 2, "config": blob(&config), "layers": [{"digest": format!("sha256:{}", "0".repeat(64)), "size": wrong.len()}]})).unwrap();
        let other = b"{}".to_vec();
        let index = serde_json::to_vec(&json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": format!("sha256:{}", hex(&other)), "size": other.len(), "platform": {"os": "linux", "architecture": "amd64"}},
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": format!("sha256:{}", hex(&manifest)), "size": manifest.len(), "platform": {"os": "linux", "architecture": "arm64"}},
        ]}))
        .unwrap();
        let oci = |media: &str, bytes: &[u8]| (media.to_string(), bytes.to_vec());
        let manifests = vec![
            ("1.0".to_string(), oci("application/vnd.oci.image.index.v1+json", &index)),
            (format!("sha256:{}", hex(&manifest)), oci("application/vnd.oci.image.manifest.v1+json", &manifest)),
            ("bad".to_string(), oci("application/vnd.oci.image.manifest.v1+json", &broken)),
        ];
        let blobs = vec![(format!("sha256:{}", hex(&config)), config.clone()), (format!("sha256:{}", hex(&layer)), layer.clone()), (format!("sha256:{}", "0".repeat(64)), wrong)];
        let seen = Arc::new(Seen::default());
        let host = registry(blobs, manifests, seen.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let options = DownloadOptions { platform: Some("linux/arm64".into()), num_connections: 2, max_retries: 1, ..Default::default() };

        let source = reference(&format!("docker://{host}/lib/app:1.0")).unwrap();
        let (folder, fresh) = run(&source, &options, dir.path(), None, CancellationToken::new()).await.unwrap();
        assert!(fresh);
        assert_eq!(folder, dir.path().join("app_1.0"));
        let at = |hex: &str| folder.join("blobs").join("sha256").join(hex);
        assert_eq!(std::fs::read(at(&self::hex(&config))).unwrap(), config);
        assert_eq!(std::fs::read(at(&self::hex(&layer))).unwrap(), layer);
        assert_eq!(std::fs::read(at(&self::hex(&manifest))).unwrap(), manifest);
        assert_eq!(std::fs::read_to_string(folder.join("oci-layout")).unwrap(), r#"{"imageLayoutVersion":"1.0.0"}"#);
        let written: Value = serde_json::from_slice(&std::fs::read(folder.join("index.json")).unwrap()).unwrap();
        let entry = &written["manifests"][0];
        assert_eq!(entry["digest"], format!("sha256:{}", self::hex(&manifest)));
        assert_eq!((entry["size"].as_u64(), &entry["platform"]["architecture"]), (Some(manifest.len() as u64), &json!("arm64")));
        assert_eq!(entry["annotations"]["io.containerd.image.name"], format!("{host}/lib/app:1.0"));
        assert_eq!(seen.tokens.load(Ordering::SeqCst), 2, "a new token once the first expired");
        assert_eq!(seen.cdn_auth.load(Ordering::SeqCst), 0, "the token never goes to the CDN");

        let (_, again) = run(&source, &options, dir.path(), None, CancellationToken::new()).await.unwrap();
        assert!(!again, "blobs already there are kept");
        let refused = run(&reference(&format!("docker://{host}/lib/app:bad")).unwrap(), &options, dir.path(), None, CancellationToken::new()).await.unwrap_err();
        assert!(refused.to_ascii_lowercase().contains("checksum") || refused.contains("sha256"), "{refused}");
        let missing = run(&reference(&format!("docker://{host}/lib/app:2.0")).unwrap(), &options, dir.path(), None, CancellationToken::new()).await.unwrap_err();
        assert!(missing.contains("has no 2.0"), "{missing}");
    }

    /// An Ollama model's layers become files named for use (the model a .gguf, its template and
    /// licences text, its parameters JSON), or, in an Ollama models folder, its blobs and manifest
    /// where Ollama keeps them; each checked against its digest.
    #[tokio::test]
    async fn an_ollama_model_downloads_as_files_or_into_a_models_folder() {
        let (config, gguf) = (br#"{"model_format":"gguf"}"#.to_vec(), [b"GGUF".to_vec(), crate::netsim::file(50_000)].concat());
        let (template, license, terms, params) = (b"{{ .Prompt }}".to_vec(), b"LICENSE A".to_vec(), b"POLICY B".to_vec(), br#"{"stop":["<|end|>"]}"#.to_vec());
        let layer = |media: &str, bytes: &[u8]| json!({"mediaType": format!("application/vnd.ollama.image.{media}"), "digest": format!("sha256:{}", hex(bytes)), "size": bytes.len()});
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2, "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "digest": format!("sha256:{}", hex(&config)), "size": config.len()},
            "layers": [layer("model", &gguf), layer("template", &template), layer("license", &license), layer("license", &terms), layer("params", &params)],
        }))
        .unwrap();
        let all = [&config, &gguf, &template, &license, &terms, &params];
        let blobs = all.iter().map(|b| (format!("sha256:{}", hex(b)), b.to_vec())).collect();
        let manifests = vec![("1.0".to_string(), ("application/vnd.docker.distribution.manifest.v2+json".to_string(), manifest.clone()))];
        let host = registry(blobs, manifests, Arc::new(Seen::default())).await;
        let source = reference(&format!("docker://{host}/lib/app:1.0")).unwrap();
        let options = DownloadOptions { num_connections: 2, max_retries: 1, ..Default::default() };

        let dir = tempfile::tempdir().unwrap();
        let (folder, fresh) = run(&source, &options, dir.path(), None, CancellationToken::new()).await.unwrap();
        assert!(fresh && folder == dir.path().join("app_1.0"));
        let names = ["config.json", "app-1.0.gguf", "template.txt", "license.txt", "license-2.txt", "params.json"];
        for (name, bytes) in names.iter().zip(all) {
            assert_eq!(&std::fs::read(folder.join(name)).unwrap(), bytes, "{name}");
        }
        assert!(!folder.join("index.json").exists() && !folder.join("blobs").exists());

        let models = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(models.path().join("blobs")).unwrap();
        std::fs::create_dir_all(models.path().join("manifests")).unwrap();
        let (folder, _) = run(&source, &options, models.path(), None, CancellationToken::new()).await.unwrap();
        assert_eq!(folder, models.path());
        for bytes in all {
            assert_eq!(&std::fs::read(folder.join("blobs").join(format!("sha256-{}", hex(bytes)))).unwrap(), bytes);
        }
        let kept = folder.join("manifests").join(crate::engine::sanitize_component(&host)).join("lib").join("app").join("1.0");
        assert_eq!(std::fs::read(kept).unwrap(), manifest);
    }
}
