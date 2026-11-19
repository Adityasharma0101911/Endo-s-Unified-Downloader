//! Package registries beside those of `repos` (PyPI, npm, crates.io): Maven Central, NuGet, Go
//! modules, Homebrew bottles, Flathub, conda (anaconda.org), RubyGems, Packagist and F-Droid. A
//! package's page, or Maven coordinates as typed (`group:artifact[:version]`), becomes its files,
//! each with the checksum its registry publishes, from the registry's own hosts.

use std::collections::HashMap;
use std::time::Duration;

use percent_encoding::percent_decode_str;
use reqwest::{Client, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::ingest::{clean_path, ListOptions, Task};
use crate::repos::{checksum, url_at};

const MAVEN: &str = "https://repo1.maven.org/maven2";
const NUGET: &str = "https://api.nuget.org";
const GO_PROXY: &str = "https://proxy.golang.org";
const GO_SUMS: &str = "https://sum.golang.org";
const BREW: &str = "https://formulae.brew.sh/api";
const FLATHUB: &str = "https://flathub.org/api/v2";
const FLATHUB_REFS: &str = "https://dl.flathub.org/repo/appstream";
const ANACONDA: &str = "https://api.anaconda.org";
const CONDA_FILES: &str = "https://conda.anaconda.org";
const RUBYGEMS: &str = "https://rubygems.org";
const PACKAGIST: &str = "https://repo.packagist.org";
const FDROID: &str = "https://f-droid.org/repo";
/// Between two requests to one registry.
const GAP: Duration = Duration::from_millis(200);
/// The largest answer read: F-Droid's whole index is some 75 MB.
const MAX_ANSWER: usize = 256 << 20;
/// Most files of one conda version listed.
const MAX_BUILDS: usize = 50;
/// macOS releases Homebrew names its bottles by, newest first.
const MACOS: &[&str] = &["golden_gate", "tahoe", "sequoia", "sonoma", "ventura", "monterey", "big_sur", "catalina"];
/// First path parts of these registries' sites that are their own pages.
const SITE_PAGES: &[&str] = &["about", "account", "api", "docs", "explore", "help", "login", "organizations", "search", "settings", "signup", "std", "user", "users"];

/// What a package link is.
#[derive(Debug, PartialEq)]
enum Pkg {
    Maven { group: String, artifact: String, version: Option<String> },
    NuGet { id: String, version: Option<String> },
    Go { module: String, version: Option<String> },
    Brew { formula: String },
    Flathub { app: String },
    Conda { channel: String, name: String },
    Gem { name: String, version: Option<String> },
    Packagist { name: String, version: Option<String> },
    FDroid { id: String },
}

/// The link of Maven coordinates as typed, `group:artifact[:version]` (a group with a dot, or a
/// version), on Maven Central's site.
pub fn coordinates_link(token: &str) -> Option<Url> {
    let parts: Vec<&str> = token.trim().split(':').collect();
    let name = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    let (group, artifact, version) = match parts[..] {
        [group, artifact] if group.contains('.') => (group, artifact, None),
        [group, artifact, version] if version.starts_with(|c: char| c.is_ascii_digit()) => (group, artifact, Some(version)),
        _ => return None,
    };
    let ok = name(group) && name(artifact) && artifact.starts_with(|c: char| c.is_ascii_alphabetic()) && version.is_none_or(|v| name(v) || v.contains('+'));
    ok.then(|| url_at("https://central.sonatype.com/artifact", [group, artifact].into_iter().chain(version)).ok()).flatten()
}

fn pkg_of(url: &Url) -> Option<Pkg> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let decoded: Vec<String> = url.path_segments()?.filter(|s| !s.is_empty()).map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let segs: Vec<&str> = decoded.iter().map(String::as_str).collect();
    let host = url.host_str()?.trim_end_matches('.');
    let owned = |s: &str| s.to_string();
    let version = |v: Option<&&str>| v.map(|v| v.to_string());
    match (host.strip_prefix("www.").unwrap_or(host), &segs[..]) {
        ("central.sonatype.com" | "search.maven.org" | "mvnrepository.com", ["artifact", group, artifact, rest @ ..]) => {
            Some(Pkg::Maven { group: owned(group), artifact: owned(artifact), version: version(rest.first()) })
        }
        ("nuget.org", ["packages", id, rest @ ..]) => Some(Pkg::NuGet { id: owned(id), version: version(rest.first()) }),
        ("pkg.go.dev", [first, ..]) if first.contains('.') => {
            let path = segs.join("/");
            let (module, version) = match path.split_once('@') {
                Some((module, version)) => (module.to_string(), Some(version.split('/').next().unwrap_or(version).to_string())),
                None => (path, None),
            };
            Some(Pkg::Go { module, version })
        }
        ("formulae.brew.sh", ["formula", name]) => Some(Pkg::Brew { formula: owned(name) }),
        ("flathub.org", ["apps", app] | [_, "apps", app]) if app.contains('.') => Some(Pkg::Flathub { app: owned(app) }),
        ("anaconda.org", [channel, name]) if !SITE_PAGES.contains(channel) => Some(Pkg::Conda { channel: owned(channel), name: owned(name) }),
        ("rubygems.org", ["gems", name] | ["gems", name, "versions"]) => Some(Pkg::Gem { name: owned(name), version: None }),
        ("rubygems.org", ["gems", name, "versions", version]) => Some(Pkg::Gem { name: owned(name), version: Some(owned(version)) }),
        ("packagist.org", ["packages", vendor, name, ..]) => {
            Some(Pkg::Packagist { name: format!("{vendor}/{name}"), version: url.fragment().filter(|f| !f.is_empty()).map(str::to_string) })
        }
        ("f-droid.org", ["packages", id, ..] | [_, "packages", id, ..]) if id.contains('.') => Some(Pkg::FDroid { id: owned(id) }),
        _ => None,
    }
}

/// Whether `url` is a package page [`list`] reads.
pub fn lists(url: &Url) -> bool {
    pkg_of(url).is_some()
}

/// The files of the package `url` names (see the module docs); `options.formats` picks a
/// Homebrew bottle's platform (`arm64_sequoia`, `x86_64_linux`) and a conda build's (`linux-64`,
/// `py312`).
pub async fn list(http: &Client, url: &Url, options: &ListOptions) -> Result<Vec<Task>, String> {
    let picks: Vec<String> = options.formats.iter().map(|f| f.trim().to_ascii_lowercase()).filter(|f| !f.is_empty()).collect();
    match pkg_of(url).ok_or_else(|| format!("{url} is not a package page"))? {
        Pkg::Maven { group, artifact, version } => maven(http, MAVEN, &group, &artifact, version.as_deref()).await,
        Pkg::NuGet { id, version } => nuget(http, NUGET, &id, version.as_deref()).await,
        Pkg::Go { module, version } => go(http, GO_PROXY, GO_SUMS, &module, version.as_deref(), options).await,
        Pkg::Brew { formula } => brew(http, BREW, &formula, &picks).await,
        Pkg::Flathub { app } => flathub(http, FLATHUB, &app, options).await,
        Pkg::Conda { channel, name } => conda(http, ANACONDA, CONDA_FILES, &channel, &name, &picks).await,
        Pkg::Gem { name, version } => gem(http, RUBYGEMS, &name, version.as_deref()).await,
        Pkg::Packagist { name, version } => packagist(http, PACKAGIST, &name, version.as_deref()).await,
        Pkg::FDroid { id } => fdroid(http, FDROID, &id, options).await,
    }
}

/// The body of `answer`, at most [`MAX_ANSWER`] bytes.
async fn capped(mut answer: Response, site: &str) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = answer.chunk().await.map_err(|e| format!("Cannot reach {site}: {e}"))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_ANSWER {
            return Err(format!("{site} answered with more than the app reads"));
        }
    }
    Ok(body)
}

/// What `site` answers at `url`, paced; None for 404 or 410.
async fn get(http: &Client, url: &Url, site: &str) -> Result<Option<Vec<u8>>, String> {
    let answer = crate::polite::get(http, url, GAP).await?;
    match answer.status() {
        StatusCode::NOT_FOUND | StatusCode::GONE => Ok(None),
        status if !status.is_success() => Err(crate::polite::refused(site, status)),
        _ => capped(answer, site).await.map(Some),
    }
}

async fn json<T: DeserializeOwned>(http: &Client, url: &Url, site: &str) -> Result<Option<T>, String> {
    match get(http, url, site).await? {
        // NuGet's registration of SemVer 2 packages comes gzipped whatever is asked.
        Some(body) if body.starts_with(&[0x1f, 0x8b]) => {
            let mut plain = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(flate2::read::GzDecoder::new(&body[..]), MAX_ANSWER as u64), &mut plain).map_err(|_| format!("{site} answered with something the app cannot read"))?;
            serde_json::from_slice(&plain).map(Some).map_err(|_| format!("{site} answered with something the app cannot read"))
        }
        Some(body) => serde_json::from_slice(&body).map(Some).map_err(|_| format!("{site} answered with something the app cannot read")),
        None => Ok(None),
    }
}

/// A task of one file from a registry's host.
fn file(url: Url, name: &str, size: Option<u64>, checksum: Option<String>) -> Result<Task, String> {
    Ok(Task { urls: vec![url], name: Some(clean_path([name])?), size, checksum, from_document: true, ..Task::default() })
}

/// The text of the first `<tag>` of `xml`.
fn tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    Some(xml[start..start + xml[start..].find("</")?].trim()).filter(|t| !t.is_empty())
}

/// Maven Central's `group:artifact` at `version`, its latest release without one: its jar, sources
/// and javadoc jars and POM, those it has, each with the SHA-1 published beside it.
async fn maven(http: &Client, repo: &str, group: &str, artifact: &str, version: Option<&str>) -> Result<Vec<Task>, String> {
    let base: Vec<&str> = group.split('.').chain([artifact]).collect();
    let version = match version {
        Some(version) => version.to_string(),
        None => {
            let meta = url_at(repo, base.iter().copied().chain(["maven-metadata.xml"]))?;
            let xml = get(http, &meta, "Maven Central").await?.ok_or_else(|| format!("Maven Central has no {group}:{artifact}"))?;
            let xml = String::from_utf8_lossy(&xml);
            tag(&xml, "release").or_else(|| tag(&xml, "latest")).ok_or_else(|| format!("Maven Central names no release of {group}:{artifact}"))?.to_string()
        }
    };
    let mut tasks = Vec::new();
    for suffix in [".jar", "-sources.jar", "-javadoc.jar", ".pom"] {
        let name = format!("{artifact}-{version}{suffix}");
        let url = url_at(repo, base.iter().copied().chain([version.as_str(), name.as_str()]))?;
        let sum = url_at(repo, base.iter().copied().chain([version.as_str(), format!("{name}.sha1").as_str()]))?;
        // The checksum beside a file says it is there.
        let Some(sha1) = get(http, &sum, "Maven Central").await? else { continue };
        let hex = String::from_utf8_lossy(&sha1).split_whitespace().next().map(str::to_string);
        tasks.push(file(url, &name, None, hex.and_then(|h| checksum("sha1", &h)))?);
    }
    if tasks.is_empty() {
        return Err(format!("Maven Central has no version {version} of {group}:{artifact}"));
    }
    Ok(tasks)
}

#[derive(Deserialize)]
struct Versions {
    versions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    catalog_entry: Url,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogLeaf {
    package_hash: Option<String>,
    package_hash_algorithm: Option<String>,
    package_size: Option<u64>,
}

/// NuGet's package `id` at `version`, its latest stable one without one: its .nupkg from the flat
/// container, with the size and SHA-512 its catalog entry gives.
async fn nuget(http: &Client, api: &str, id: &str, version: Option<&str>) -> Result<Vec<Task>, String> {
    use base64::Engine as _;
    let lower = id.to_ascii_lowercase();
    let version = match version {
        Some(version) => version.to_ascii_lowercase(),
        None => {
            let index = url_at(api, ["v3-flatcontainer", lower.as_str(), "index.json"])?;
            let found: Versions = json(http, &index, "NuGet").await?.ok_or_else(|| format!("NuGet has no package {id}"))?;
            let stable = found.versions.iter().rev().find(|v| !v.contains('-')).or(found.versions.last());
            stable.ok_or_else(|| format!("NuGet lists no version of {id}"))?.to_ascii_lowercase()
        }
    };
    let leaf = format!("{version}.json");
    let mut registration = None;
    for hive in ["registration5-semver1", "registration5-gz-semver2"] {
        registration = json::<Registration>(http, &url_at(api, ["v3", hive, lower.as_str(), leaf.as_str()])?, "NuGet").await?;
        if registration.is_some() {
            break;
        }
    }
    let registration = registration.ok_or_else(|| format!("NuGet has no version {version} of {id}"))?;
    let catalog: CatalogLeaf = json(http, &registration.catalog_entry, "NuGet").await?.ok_or_else(|| format!("NuGet has no catalog entry of {id} {version}"))?;
    let sha512 = catalog.package_hash.filter(|_| catalog.package_hash_algorithm.as_deref().is_some_and(|a| a.eq_ignore_ascii_case("sha512")));
    let sha512 = sha512.and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok()).filter(|d| d.len() == 64);
    let name = format!("{lower}.{version}.nupkg");
    let url = url_at(api, ["v3-flatcontainer", lower.as_str(), version.as_str(), name.as_str()])?;
    Ok(vec![file(url, &name, catalog.package_size, sha512.map(|d| crate::digest::checksum("sha512", &d)))?])
}

#[derive(Deserialize)]
struct GoLatest {
    #[serde(rename = "Version")]
    version: String,
}

/// A module path as Go's proxy protocol writes it: each capital letter as `!` and the letter.
fn go_escape(module: &str) -> String {
    module.chars().flat_map(|c| if c.is_ascii_uppercase() { vec!['!', c.to_ascii_lowercase()] } else { vec![c] }).collect()
}

/// The Go module `module` (or the module a package path of it is in) at `version`, its latest
/// without one, from Go's module proxy: its source zip and go.mod. sum.golang.org's `h1:` hash is
/// of the files inside the zip, not of the zip, so the app does not check it: it says it, for the
/// `go` command to check when the module is used.
async fn go(http: &Client, proxy: &str, sums: &str, module: &str, version: Option<&str>, options: &ListOptions) -> Result<Vec<Task>, String> {
    let mut path = module.to_string();
    let found = loop {
        let escaped = go_escape(&path);
        let at = match version {
            Some(version) => Some(version.to_string()),
            None => json::<GoLatest>(http, &url_at(proxy, escaped.split('/').chain(["@latest"]))?, "Go's module proxy").await?.map(|l| l.version),
        };
        let exists = match &at {
            Some(v) if version.is_some() => get(http, &url_at(proxy, escaped.split('/').chain(["@v", format!("{v}.info").as_str()]))?, "Go's module proxy").await?.is_some(),
            Some(_) => true,
            None => false,
        };
        match (exists, path.rsplit_once('/')) {
            (true, _) => break (path, at.unwrap_or_default()),
            // A package's path: its module is a shorter one.
            (false, Some((parent, _))) if parent.contains('/') => path = parent.to_string(),
            _ => return Err(format!("Go's module proxy has no module {module}{}", version.map(|v| format!(" at {v}")).unwrap_or_default())),
        }
    };
    let (module, version) = found;
    let escaped = go_escape(&module);
    let last = module.rsplit('/').next().unwrap_or(&module);
    let at = |ext: &str| url_at(proxy, escaped.split('/').chain(["@v", format!("{version}.{ext}").as_str()]));
    let tasks = vec![file(at("zip")?, &format!("{last}@{version}.zip"), None, None)?, file(at("mod")?, &format!("{last}@{version}.mod"), None, None)?];
    let tail = format!("{escaped}@{version}");
    let lookup = url_at(sums, std::iter::once("lookup").chain(tail.split('/')))?;
    if let Some(lines) = get(http, &lookup, "sum.golang.org").await.ok().flatten() {
        let lines = String::from_utf8_lossy(&lines);
        let sums: Vec<&str> = lines.lines().filter(|l| l.starts_with(&format!("{module} {version}"))).collect();
        if !sums.is_empty() {
            options.note(format!("Go checks a module by the files inside its zip, not by the zip, so the app does not; its go.sum lines, from sum.golang.org: {}", sums.join("; ")));
        }
    }
    Ok(tasks)
}

#[derive(Deserialize)]
struct Formula {
    name: String,
    versions: FormulaVersions,
    #[serde(default)]
    revision: u32,
    bottle: Option<Bottles>,
}

#[derive(Deserialize)]
struct FormulaVersions {
    stable: String,
}

#[derive(Deserialize)]
struct Bottles {
    stable: Option<BottleSet>,
}

#[derive(Deserialize)]
struct BottleSet {
    #[serde(default)]
    rebuild: u32,
    files: HashMap<String, BottleFile>,
}

#[derive(Deserialize)]
struct BottleFile {
    url: String,
    sha256: String,
}

/// The bottle tag this machine runs: the newest macOS's for its CPU, Linux's for its CPU.
fn this_bottle(tags: &[&String]) -> Option<String> {
    let arm = std::env::consts::ARCH == "aarch64";
    match std::env::consts::OS {
        "linux" => Some(if arm { "arm64_linux" } else { "x86_64_linux" }.to_string()),
        "macos" => MACOS.iter().map(|os| if arm { format!("arm64_{os}") } else { os.to_string() }).find(|tag| tags.iter().any(|t| t.as_str() == tag)),
        _ => None,
    }
}

/// Homebrew's bottle of `formula` for the platform `picks` names (else this machine's), from its
/// GitHub Packages registry through the anonymous token flow of a container image, checked
/// against the SHA-256 Homebrew publishes, named as `brew` names it.
async fn brew(http: &Client, api: &str, formula: &str, picks: &[String]) -> Result<Vec<Task>, String> {
    let url = url_at(api, ["formula", format!("{formula}.json").as_str()])?;
    let found: Formula = json(http, &url, "Homebrew").await?.ok_or_else(|| format!("Homebrew has no formula {formula}"))?;
    let set = found.bottle.and_then(|b| b.stable).ok_or_else(|| format!("Homebrew builds no bottle of {formula}"))?;
    let mut tags: Vec<&String> = set.files.keys().collect();
    tags.sort();
    let tag = picks.iter().find(|p| set.files.contains_key(p.as_str())).cloned().or_else(|| this_bottle(&tags)).filter(|t| set.files.contains_key(t));
    let all = tags.iter().map(|t| t.as_str()).collect::<Vec<_>>().join(", ");
    let tag = tag.ok_or_else(|| format!("Homebrew has no bottle of {formula} for this computer: pick one with --formats ({all})"))?;
    let bottle = &set.files[&tag];
    let repo = bottle.url.strip_prefix("https://ghcr.io/v2/").and_then(|rest| rest.split_once("/blobs/")).map(|(repo, _)| repo);
    let repo = repo.ok_or_else(|| format!("Homebrew's bottle of {formula} is not on GitHub Packages"))?;
    let version = match found.revision {
        0 => found.versions.stable.clone(),
        n => format!("{}_{n}", found.versions.stable),
    };
    let rebuild = if set.rebuild > 0 { format!(".{}", set.rebuild) } else { String::new() };
    let name = format!("{}--{version}.{tag}.bottle{rebuild}.tar.gz", found.name);
    let digest = format!("sha256:{}", bottle.sha256.to_ascii_lowercase());
    let link = crate::oci::blob_link("ghcr.io", repo, &digest, &name).ok_or_else(|| format!("Homebrew's bottle of {formula} has no usable address"))?;
    Ok(vec![Task { urls: vec![link], name: Some(clean_path([name.as_str()])?), ..Task::default() }])
}

#[derive(Deserialize)]
struct Appstream {
    name: Option<String>,
}

/// Flathub's app `app`: its .flatpakref, which `flatpak install` takes. Flathub publishes apps
/// as an OSTree repository, not as single-file bundles, so there is no bundle to download.
async fn flathub(http: &Client, api: &str, app: &str, options: &ListOptions) -> Result<Vec<Task>, String> {
    let found: Appstream = json(http, &url_at(api, ["appstream", app])?, "Flathub").await?.ok_or_else(|| format!("Flathub has no app {app}"))?;
    options.note(format!(
        "Flathub publishes {} as an OSTree repository, not as a single-file bundle: the .flatpakref installs it (flatpak install {app}.flatpakref)",
        found.name.as_deref().unwrap_or(app)
    ));
    let name = format!("{app}.flatpakref");
    Ok(vec![file(url_at(FLATHUB_REFS, [name.as_str()])?, &name, None, None)?])
}

#[derive(Deserialize)]
struct CondaFile {
    basename: String,
    version: String,
    sha256: Option<String>,
    size: Option<u64>,
    #[serde(default)]
    upload_time: String,
    attrs: CondaAttrs,
}

#[derive(Deserialize)]
struct CondaAttrs {
    subdir: String,
    #[serde(default)]
    build: String,
}

/// conda's platform folder for this machine.
fn this_subdir() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "aarch64") => "win-arm64",
        ("windows", _) => "win-64",
        ("macos", "aarch64") => "osx-arm64",
        ("macos", _) => "osx-64",
        ("linux", "aarch64") => "linux-aarch64",
        _ => "linux-64",
    }
}

/// The files of the conda package `name` on `channel` for the platform `picks` names (else this
/// machine's, and `noarch`): every build of its newest version there, or those whose build
/// matches the other picks (`py312`), with the size and SHA-256 anaconda.org gives.
async fn conda(http: &Client, api: &str, files: &str, channel: &str, name: &str, picks: &[String]) -> Result<Vec<Task>, String> {
    let url = url_at(api, ["package", channel, name, "files"])?;
    let listed: Vec<CondaFile> = json(http, &url, "anaconda.org").await?.ok_or_else(|| format!("anaconda.org has no package {channel}/{name}"))?;
    let subdirs: Vec<&str> = listed.iter().map(|f| f.attrs.subdir.as_str()).collect();
    let platform = picks.iter().map(String::as_str).find(|p| subdirs.contains(p)).unwrap_or(this_subdir());
    // Python builds (py312) a pick may name; other picks are for other registries.
    let builds: Vec<&String> = picks.iter().filter(|p| p.starts_with("py")).collect();
    let here: Vec<&CondaFile> = listed.iter().filter(|f| f.attrs.subdir == platform || f.attrs.subdir == "noarch").collect();
    let newest = here.iter().max_by(|a, b| a.upload_time.cmp(&b.upload_time)).map(|f| f.version.clone());
    let newest = newest.ok_or_else(|| format!("anaconda.org has no file of {channel}/{name} for {platform}"))?;
    let chosen: Vec<&&CondaFile> = here.iter().filter(|f| f.version == newest && builds.iter().all(|b| f.attrs.build.to_ascii_lowercase().contains(b.as_str()))).take(MAX_BUILDS).collect();
    if chosen.is_empty() {
        return Err(format!("anaconda.org has no build of {channel}/{name} {newest} for {platform} that matches {}", picks.join(", ")));
    }
    chosen
        .into_iter()
        .map(|f| {
            let last = f.basename.rsplit('/').next().unwrap_or(&f.basename);
            let url = url_at(files, [channel].into_iter().chain(f.basename.split('/')))?;
            file(url, last, f.size, f.sha256.as_deref().and_then(|h| checksum("sha256", h)))
        })
        .collect()
}

#[derive(Deserialize)]
struct Gem {
    version: String,
    sha: Option<String>,
}

/// RubyGems' gem `name` at `version`, its latest without one, with its SHA-256.
async fn gem(http: &Client, base: &str, name: &str, version: Option<&str>) -> Result<Vec<Task>, String> {
    let url = match version {
        Some(version) => url_at(base, ["api", "v2", "rubygems", name, "versions", format!("{version}.json").as_str()])?,
        None => url_at(base, ["api", "v1", "gems", format!("{name}.json").as_str()])?,
    };
    let found: Gem = json(http, &url, "RubyGems").await?.ok_or_else(|| format!("RubyGems has no gem {name}{}", version.map(|v| format!(" {v}")).unwrap_or_default()))?;
    let file_name = format!("{name}-{}.gem", found.version);
    Ok(vec![file(url_at(base, ["gems", file_name.as_str()])?, &file_name, None, found.sha.as_deref().and_then(|h| checksum("sha256", h)))?])
}

#[derive(Deserialize)]
struct PackagistAnswer {
    packages: HashMap<String, Vec<PackagistVersion>>,
}

#[derive(Deserialize)]
struct PackagistVersion {
    version: String,
    dist: Option<PackagistDist>,
}

#[derive(Deserialize)]
struct PackagistDist {
    url: Url,
    shasum: Option<String>,
}

/// Packagist's `vendor/package` at `version`, its newest stable one without one: its dist zip,
/// with its SHA-1 when Packagist has one (it has none for a GitHub zipball).
async fn packagist(http: &Client, repo: &str, name: &str, version: Option<&str>) -> Result<Vec<Task>, String> {
    let (vendor, package) = name.split_once('/').ok_or_else(|| format!("{name} is no vendor/package"))?;
    let url = url_at(repo, ["p2", vendor, format!("{package}.json").as_str()])?;
    let found: PackagistAnswer = json(http, &url, "Packagist").await?.ok_or_else(|| format!("Packagist has no package {name}"))?;
    let versions = found.packages.get(name).map(Vec::as_slice).unwrap_or_default();
    let chosen = match version {
        Some(wanted) => versions.iter().find(|v| v.version == wanted || v.version.strip_prefix('v') == Some(wanted)),
        None => versions.iter().find(|v| !v.version.contains('-')).or(versions.first()),
    };
    let chosen = chosen.ok_or_else(|| format!("Packagist has no {} of {name}", version.unwrap_or("version")))?;
    let dist = chosen.dist.as_ref().ok_or_else(|| format!("Packagist names no file of {name} {}", chosen.version))?;
    let shasum = dist.shasum.as_deref().filter(|s| !s.is_empty()).and_then(|h| checksum("sha1", h));
    Ok(vec![file(dist.url.clone(), &format!("{vendor}-{package}-{}.zip", chosen.version), None, shasum)?])
}

#[derive(Deserialize)]
struct Entry {
    index: EntryFile,
}

#[derive(Deserialize)]
struct EntryFile {
    name: String,
    sha256: String,
    size: u64,
}

#[derive(Deserialize)]
struct IndexV2 {
    #[serde(default)]
    repo: FRepo,
    #[serde(default)]
    packages: HashMap<String, FPackage>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct FRepo {
    mirrors: Vec<FMirror>,
}

#[derive(Deserialize)]
struct FMirror {
    url: String,
}

#[derive(Deserialize)]
struct FPackage {
    #[serde(default)]
    versions: HashMap<String, FVersion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FVersion {
    file: EntryFileLite,
    manifest: FManifest,
    #[serde(default)]
    release_channels: Vec<String>,
}

#[derive(Deserialize)]
struct EntryFileLite {
    name: String,
    sha256: Option<String>,
    size: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FManifest {
    version_code: u64,
    version_name: Option<String>,
    signer: Option<FSigner>,
}

#[derive(Deserialize)]
struct FSigner {
    #[serde(default)]
    sha256: Vec<String>,
}

/// F-Droid's app `id`: the APK of its newest stable version as the repository's index (v2) lists
/// it, with its size and SHA-256, from the repository and the mirrors the index names. The index
/// is checked against the SHA-256 the repository's entry.json gives it (fetched over HTTPS; the
/// signature of entry.jar is not checked); the signing certificate the index gives the APK is
/// told, not checked against the APK.
async fn fdroid(http: &Client, repo: &str, id: &str, options: &ListOptions) -> Result<Vec<Task>, String> {
    let entry: Entry = json(http, &url_at(repo, ["entry.json"])?, "F-Droid").await?.ok_or("F-Droid's repository has no entry.json")?;
    if entry.index.size > MAX_ANSWER as u64 {
        return Err("F-Droid's index is larger than the app reads".to_string());
    }
    // The index is read once while entry.json gives it the same SHA-256, not again for each app.
    // shortcut: the last one read stays in memory until another replaces it; drop it after a while
    // if that ever matters.
    static READ: std::sync::LazyLock<tokio::sync::Mutex<Option<(String, std::sync::Arc<IndexV2>)>>> = std::sync::LazyLock::new(Default::default);
    let mut read = READ.lock().await;
    let sha256 = entry.index.sha256.to_ascii_lowercase();
    let index = match &*read {
        Some((had, index)) if *had == sha256 => std::sync::Arc::clone(index),
        _ => {
            let index_url = url_at(repo, entry.index.name.split('/').filter(|s| !s.is_empty()))?;
            let bytes = get(http, &index_url, "F-Droid").await?.ok_or("F-Droid's repository has no index")?;
            if sha256 != format!("{:x}", Sha256::digest(&bytes)) {
                return Err("F-Droid's index does not match the SHA-256 its entry.json gives".to_string());
            }
            let index = std::sync::Arc::new(serde_json::from_slice::<IndexV2>(&bytes).map_err(|_| "F-Droid's index cannot be read".to_string())?);
            *read = Some((sha256, std::sync::Arc::clone(&index)));
            index
        }
    };
    drop(read);
    let package = index.packages.get(id).ok_or_else(|| format!("F-Droid has no app {id}"))?;
    let newest = |stable: bool| package.versions.values().filter(|v| !stable || v.release_channels.is_empty()).max_by_key(|v| v.manifest.version_code);
    let version = newest(true).or_else(|| newest(false)).ok_or_else(|| format!("F-Droid lists no version of {id}"))?;
    let path: Vec<&str> = version.file.name.split('/').filter(|s| !s.is_empty()).collect();
    let mut urls = vec![url_at(repo, path.iter().copied())?];
    for mirror in index.repo.mirrors.iter().filter_map(|m| url_at(m.url.trim_end_matches('/'), path.iter().copied()).ok()) {
        if mirror.scheme() == "https" && !urls.contains(&mirror) {
            urls.push(mirror);
        }
    }
    if let Some(signer) = version.manifest.signer.as_ref().and_then(|s| s.sha256.first()) {
        options.note(format!("F-Droid's index says {id} {} is signed by the certificate with SHA-256 {signer}", version.manifest.version_name.as_deref().unwrap_or("")));
    }
    let name = path.last().copied().unwrap_or("app.apk");
    let checksum = version.file.sha256.as_deref().and_then(|h| checksum("sha256", h));
    Ok(vec![Task { urls, name: Some(clean_path([name])?), size: version.file.size, checksum, from_document: true, ..Task::default() }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{serve, Reply};
    use std::path::PathBuf;

    fn http() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    fn ok(body: &[u8]) -> Reply {
        (200, String::new(), body.to_vec())
    }

    fn missing() -> Reply {
        (404, String::new(), Vec::new())
    }

    fn pkg(link: &str) -> Option<Pkg> {
        pkg_of(&Url::parse(link).unwrap())
    }

    fn name(task: &Task) -> PathBuf {
        task.name.clone().unwrap()
    }

    #[test]
    fn package_links_are_told_by_their_shape() {
        assert_eq!(coordinates_link("org.slf4j:slf4j-api").unwrap().as_str(), "https://central.sonatype.com/artifact/org.slf4j/slf4j-api");
        assert_eq!(coordinates_link(" junit:junit:4.13.2 ").unwrap().as_str(), "https://central.sonatype.com/artifact/junit/junit/4.13.2");
        for not in ["localhost:8080", "example.com:8080", "C:\\x", "a:b:c:d", "user:pass", "org.x:art:latest"] {
            assert!(coordinates_link(not).is_none(), "{not}");
        }
        let maven = |v: Option<&str>| Some(Pkg::Maven { group: "org.slf4j".into(), artifact: "slf4j-api".into(), version: v.map(Into::into) });
        assert_eq!(pkg("https://central.sonatype.com/artifact/org.slf4j/slf4j-api/2.0.13"), maven(Some("2.0.13")));
        assert_eq!(pkg("https://search.maven.org/artifact/org.slf4j/slf4j-api/2.0.13/jar"), maven(Some("2.0.13")));
        assert_eq!(pkg("https://mvnrepository.com/artifact/org.slf4j/slf4j-api"), maven(None));
        assert_eq!(pkg("https://www.nuget.org/packages/Newtonsoft.Json/13.0.3"), Some(Pkg::NuGet { id: "Newtonsoft.Json".into(), version: Some("13.0.3".into()) }));
        assert_eq!(pkg("https://pkg.go.dev/golang.org/x/text@v0.14.0/language"), Some(Pkg::Go { module: "golang.org/x/text".into(), version: Some("v0.14.0".into()) }));
        assert_eq!(pkg("https://pkg.go.dev/github.com/BurntSushi/toml"), Some(Pkg::Go { module: "github.com/BurntSushi/toml".into(), version: None }));
        assert_eq!(pkg("https://formulae.brew.sh/formula/wget"), Some(Pkg::Brew { formula: "wget".into() }));
        assert_eq!(pkg("https://flathub.org/en/apps/org.gimp.GIMP"), Some(Pkg::Flathub { app: "org.gimp.GIMP".into() }));
        assert_eq!(pkg("https://anaconda.org/conda-forge/zstd"), Some(Pkg::Conda { channel: "conda-forge".into(), name: "zstd".into() }));
        assert_eq!(pkg("https://rubygems.org/gems/rake/versions/13.2.1"), Some(Pkg::Gem { name: "rake".into(), version: Some("13.2.1".into()) }));
        assert_eq!(pkg("https://packagist.org/packages/monolog/monolog#3.12.1"), Some(Pkg::Packagist { name: "monolog/monolog".into(), version: Some("3.12.1".into()) }));
        assert_eq!(pkg("https://f-droid.org/en/packages/org.fdroid.fdroid/"), Some(Pkg::FDroid { id: "org.fdroid.fdroid".into() }));
        for not in ["https://pkg.go.dev/std", "https://pkg.go.dev/search?q=x", "https://anaconda.org/search", "https://formulae.brew.sh/cask/firefox", "https://flathub.org/apps/search", "https://f-droid.org/en/packages/"] {
            assert_eq!(pkg(not), None, "{not}");
        }
        assert_eq!(go_escape("github.com/BurntSushi/toml"), "github.com/!burnt!sushi/toml");
    }

    /// Maven Central lists the files a version has, each with the SHA-1 beside it, the latest
    /// release without a version; NuGet's package has the SHA-512 of its catalog entry, from a
    /// gzipped registration too; RubyGems' gem its SHA-256; Packagist's newest stable dist.
    #[tokio::test]
    async fn registries_list_their_files_with_their_checksums() {
        let host = std::sync::Arc::new(std::sync::OnceLock::<String>::new());
        let named = std::sync::Arc::clone(&host);
        let (addr, _) = serve(move |path, _| {
            let at = named.get().cloned().unwrap_or_default();
            match path {
                "/maven2/org/x/art/maven-metadata.xml" => ok(b"<metadata><versioning><latest>2.0-rc1</latest><release>1.5</release></versioning></metadata>"),
                "/maven2/org/x/art/1.5/art-1.5.jar.sha1" => ok(b"A94A8FE5CCB19BA61C4C0873D391E987982FBBD3  art-1.5.jar\n"),
                "/maven2/org/x/art/1.5/art-1.5.pom.sha1" => ok(b"62cdb7020ff920e5aa642c3d4066950dd1f01f4d"),
                "/nuget/v3-flatcontainer/some.pkg/index.json" => ok(br#"{"versions":["1.0.0","2.0.0","3.0.0-beta"]}"#),
                "/nuget/v3/registration5-semver1/some.pkg/2.0.0.json" => ok(format!(r#"{{"catalogEntry":"http://{at}/catalog/2.json"}}"#).as_bytes()),
                "/nuget/v3/registration5-gz-semver2/some.pkg/3.0.0-beta.json" => {
                    use std::io::Write;
                    let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                    z.write_all(format!(r#"{{"catalogEntry":"http://{at}/catalog/3.json"}}"#).as_bytes()).unwrap();
                    ok(&z.finish().unwrap())
                }
                "/catalog/2.json" | "/catalog/3.json" => ok(
                    br#"{"packageHash":"mbJSvHfRxfX3tR/U6n1WU+mWHXswYc+SB/hkOpx8yZZe68hNZGfymJu0cjsaJEkVzCMqePiU6LdIyogqfIn7kg==","packageHashAlgorithm":"SHA512","packageSize":2441966}"#,
                ),
                "/gems/api/v1/gems/rake.json" => ok(br#"{"version":"13.4.2","sha":"cb825b2bd5f1f8e91ca37bddb4b9aaf345551b4731da62949be002fa89283701"}"#),
                "/packagist/p2/mono/log.json" => ok(
                    br#"{"minified":"composer/2.0","packages":{"mono/log":[{"version":"4.0.0-beta1","dist":{"url":"https://api.github.com/repos/m/l/zipball/b","type":"zip","shasum":""}},
                        {"version":"3.13.0","dist":{"url":"https://api.github.com/repos/m/l/zipball/a","type":"zip","shasum":"0123456789abcdef0123456789abcdef01234567"}}]}}"#,
                ),
                _ => missing(),
            }
        })
        .await;
        host.set(addr.to_string()).unwrap();
        let base = format!("http://{addr}");

        let tasks = maven(&http(), &format!("{base}/maven2"), "org.x", "art", None).await.unwrap();
        assert_eq!(tasks.iter().map(name).collect::<Vec<_>>(), [PathBuf::from("art-1.5.jar"), PathBuf::from("art-1.5.pom")]);
        assert_eq!(tasks[0].urls[0].as_str(), format!("{base}/maven2/org/x/art/1.5/art-1.5.jar"));
        assert_eq!(tasks[0].checksum.as_deref(), Some("sha1:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3"));
        assert_eq!(maven(&http(), &format!("{base}/maven2"), "org.x", "art", Some("9")).await.unwrap_err(), "Maven Central has no version 9 of org.x:art");

        let sha512 = {
            use base64::Engine as _;
            let digest = base64::engine::general_purpose::STANDARD.decode("mbJSvHfRxfX3tR/U6n1WU+mWHXswYc+SB/hkOpx8yZZe68hNZGfymJu0cjsaJEkVzCMqePiU6LdIyogqfIn7kg==").unwrap();
            crate::digest::checksum("sha512", &digest)
        };
        for (version, file) in [(None, "some.pkg.2.0.0.nupkg"), (Some("3.0.0-beta"), "some.pkg.3.0.0-beta.nupkg")] {
            let [task] = &nuget(&http(), &format!("{base}/nuget"), "Some.Pkg", version).await.unwrap()[..] else { panic!("one package") };
            assert_eq!((name(task), task.size), (PathBuf::from(file), Some(2441966)));
            assert_eq!(task.checksum.as_deref(), Some(sha512.as_str()));
        }

        let [rake] = &gem(&http(), &format!("{base}/gems"), "rake", None).await.unwrap()[..] else { panic!("one gem") };
        assert_eq!((rake.urls[0].as_str(), name(rake)), (format!("{base}/gems/gems/rake-13.4.2.gem").as_str(), PathBuf::from("rake-13.4.2.gem")));
        assert_eq!(rake.checksum.as_deref(), Some("sha256:cb825b2bd5f1f8e91ca37bddb4b9aaf345551b4731da62949be002fa89283701"));

        let [log] = &packagist(&http(), &format!("{base}/packagist"), "mono/log", None).await.unwrap()[..] else { panic!("one zip") };
        assert_eq!((log.urls[0].as_str(), name(log)), ("https://api.github.com/repos/m/l/zipball/a", PathBuf::from("mono-log-3.13.0.zip")));
        assert_eq!(log.checksum.as_deref(), Some("sha1:0123456789abcdef0123456789abcdef01234567"));
        let beta = packagist(&http(), &format!("{base}/packagist"), "mono/log", Some("4.0.0-beta1")).await.unwrap();
        assert_eq!((beta[0].urls[0].as_str(), beta[0].checksum.as_deref()), ("https://api.github.com/repos/m/l/zipball/b", None));
    }

    /// A Go package's path finds its module, whose zip and go.mod come with its go.sum lines told;
    /// a Homebrew bottle is the picked platform's blob through the registry's token flow; a Flathub
    /// app its .flatpakref; a conda package the newest version's builds the picks name; an F-Droid
    /// app its newest stable APK from the repository and the index's mirrors, the index checked.
    #[tokio::test]
    async fn modules_bottles_apps_and_apks_come_from_their_registries() {
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let index = format!(
            r#"{{"repo":{{"mirrors":[{{"url":"https://f-droid.org/repo"}},{{"url":"https://mirror.example/fdroid/repo/"}}]}},"packages":{{"org.x.app":{{"versions":{{
                "v3":{{"file":{{"name":"/org.x.app_3.apk","sha256":"{a}","size":300}},"manifest":{{"versionCode":3,"versionName":"3.0-beta"}},"releaseChannels":["Beta"]}},
                "v2":{{"file":{{"name":"/org.x.app_2.apk","sha256":"{b}","size":200}},"manifest":{{"versionCode":2,"versionName":"2.0","signer":{{"sha256":["c0ffee"]}}}}}}}}}}}}}}"#
        );
        let entry = format!(r#"{{"index":{{"name":"/index-v2.json","sha256":"{:x}","size":{}}}}}"#, Sha256::digest(index.as_bytes()), index.len());
        let bottle = |sha: &str| format!(r#"{{"cellar":":any","url":"https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:{sha}","sha256":"{sha}"}}"#);
        let formula = format!(r#"{{"name":"wget","versions":{{"stable":"1.25.0"}},"revision":2,"bottle":{{"stable":{{"rebuild":1,"files":{{"arm64_sequoia":{},"x86_64_linux":{}}}}}}}}}"#, bottle(&a), bottle(&b));
        let conda_file = |version: &str, subdir: &str, build: &str, uploaded: &str| {
            format!(r#"{{"basename":"{subdir}/zstd-{version}-{build}.conda","version":"{version}","sha256":"{a}","size":10,"upload_time":"{uploaded}","attrs":{{"subdir":"{subdir}","build":"{build}"}}}}"#)
        };
        let files = format!(
            "[{},{},{},{}]",
            conda_file("1.5.6", "linux-64", "py312_0", "2025-01-01"),
            conda_file("1.5.7", "linux-64", "py311_0", "2026-08-01"),
            conda_file("1.5.7", "linux-64", "py312_0", "2026-08-02"),
            conda_file("1.5.7", "osx-arm64", "py312_0", "2026-08-03")
        );
        let (addr, hits) = serve(move |path, _| match path {
            "/fdroid/entry.json" => ok(entry.as_bytes()),
            "/fdroid/index-v2.json" => ok(index.as_bytes()),
            "/go/github.com/!burnt!sushi/toml/@latest" => ok(br#"{"Version":"v1.4.0","Time":"2024-06-01T00:00:00Z"}"#),
            "/sum/lookup/github.com/!burnt!sushi/toml@v1.4.0" => {
                ok(b"123\ngithub.com/BurntSushi/toml v1.4.0 h1:abc=\ngithub.com/BurntSushi/toml v1.4.0/go.mod h1:def=\n\ngo.sum database tree\n")
            }
            "/brew/formula/wget.json" => ok(formula.as_bytes()),
            "/brew/formula/none.json" => ok(br#"{"name":"none","versions":{"stable":"1"},"bottle":{}}"#),
            "/flathub/appstream/org.gimp.GIMP" => ok(br#"{"name":"GIMP"}"#),
            "/conda/package/conda-forge/zstd/files" => ok(files.as_bytes()),
            _ => missing(),
        })
        .await;
        let base = format!("http://{addr}");
        let (tx, notes) = std::sync::mpsc::channel();
        let options = ListOptions { notes: Some(tx), ..ListOptions::default() };

        let module = go(&http(), &format!("{base}/go"), &format!("{base}/sum"), "github.com/BurntSushi/toml/internal", None, &options).await.unwrap();
        assert_eq!(module.iter().map(name).collect::<Vec<_>>(), [PathBuf::from("toml@v1.4.0.zip"), PathBuf::from("toml@v1.4.0.mod")]);
        assert_eq!(module[0].urls[0].as_str(), format!("{base}/go/github.com/!burnt!sushi/toml/@v/v1.4.0.zip"));
        assert!(notes.try_iter().any(|n| n.text.contains("github.com/BurntSushi/toml v1.4.0 h1:abc=")));
        assert!(go(&http(), &format!("{base}/go"), &format!("{base}/sum"), "example.com/none", None, &options).await.is_err());

        let [wget] = &brew(&http(), &format!("{base}/brew"), "wget", &["x86_64_linux".to_string()]).await.unwrap()[..] else { panic!("one bottle") };
        assert_eq!(name(wget), PathBuf::from("wget--1.25.0_2.x86_64_linux.bottle.1.tar.gz"));
        assert!(wget.urls[0].as_str().starts_with("docker://ghcr.io/homebrew/core/wget:latest?blob=sha256%3Abbbb"), "{}", wget.urls[0]);
        assert_eq!(brew(&http(), &format!("{base}/brew"), "none", &[]).await.unwrap_err(), "Homebrew builds no bottle of none");

        let [gimp] = &flathub(&http(), &format!("{base}/flathub"), "org.gimp.GIMP", &options).await.unwrap()[..] else { panic!("one ref") };
        assert_eq!(gimp.urls[0].as_str(), "https://dl.flathub.org/repo/appstream/org.gimp.GIMP.flatpakref");
        assert!(notes.try_iter().any(|n| n.text.contains("OSTree")));

        let picks = ["linux-64".to_string(), "py312".to_string()];
        let [zstd] = &conda(&http(), &format!("{base}/conda"), "https://conda.anaconda.org", "conda-forge", "zstd", &picks).await.unwrap()[..] else { panic!("one build") };
        assert_eq!(zstd.urls[0].as_str(), "https://conda.anaconda.org/conda-forge/linux-64/zstd-1.5.7-py312_0.conda");
        assert_eq!((name(zstd), zstd.size), (PathBuf::from("zstd-1.5.7-py312_0.conda"), Some(10)));

        let [apk] = &fdroid(&http(), &format!("{base}/fdroid"), "org.x.app", &options).await.unwrap()[..] else { panic!("one APK") };
        let urls: Vec<&str> = apk.urls.iter().map(Url::as_str).collect();
        let local = format!("{base}/fdroid/org.x.app_2.apk");
        assert_eq!(urls, [local.as_str(), "https://f-droid.org/repo/org.x.app_2.apk", "https://mirror.example/fdroid/repo/org.x.app_2.apk"]);
        assert_eq!((apk.size, apk.checksum.clone()), (Some(200), Some(format!("sha256:{b}"))));
        assert!(notes.try_iter().any(|n| n.text.contains("c0ffee")));
        let read = hits.lock().get("/fdroid/index-v2.json").copied();
        assert_eq!(fdroid(&http(), &format!("{base}/fdroid"), "org.none", &options).await.unwrap_err(), "F-Droid has no app org.none");
        assert_eq!(hits.lock().get("/fdroid/index-v2.json").copied(), read, "the ~75 MB index is read once, not for each app");
        assert!(hits.lock().get("/fdroid/entry.json").copied() >= Some(2), "its entry.json is, to see that it has not changed");
    }
}
