//! Kaggle datasets and competitions through Kaggle's public API (`www.kaggle.com/api/v1`), as
//! its own `kaggle` command reaches them: a dataset's page downloads its files as the one archive
//! Kaggle makes of them (a version's when the link names one), a competition's its data, signed in
//! with the user's `kaggle.json` (the file Settings or `--kaggle-json` points at, else
//! `~/.kaggle/kaggle.json`). A public dataset needs no sign-in; a competition does, and one the
//! user has not joined, or whose rules they have not accepted, is an error that says so. Kaggle
//! publishes no checksum of the archive; its storage sends an MD5 header the engine can check
//! (`DownloadOptions::remote_checksum`).

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use reqwest::header::{HeaderValue, AUTHORIZATION, LOCATION};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use url::Url;

use crate::ingest::{clean_path, ListOptions, Task};

const API: &str = "https://www.kaggle.com/api/v1";
const GAP: Duration = Duration::from_millis(500);

/// What a Kaggle link is, from its shape.
#[derive(Debug, PartialEq)]
enum Kaggle {
    /// `kaggle.com/datasets/<owner>/<dataset>[/versions/<n>]`.
    Dataset { owner: String, slug: String, version: Option<u32> },
    /// `kaggle.com/competitions/<name>` or `kaggle.com/c/<name>`.
    Competition(String),
}

fn kaggle_of(url: &Url) -> Option<Kaggle> {
    if !matches!(url.scheme(), "http" | "https") || !matches!(url.host_str()?.trim_end_matches('.'), "kaggle.com" | "www.kaggle.com") {
        return None;
    }
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let owned = |s: &str| s.to_string();
    match &segs[..] {
        ["datasets", owner, slug, rest @ ..] => {
            let version = match rest {
                ["versions", n, ..] => Some(n.parse().ok()?),
                _ => None,
            };
            Some(Kaggle::Dataset { owner: owned(owner), slug: owned(slug), version })
        }
        ["competitions" | "c", name, ..] => Some(Kaggle::Competition(owned(name))),
        _ => None,
    }
}

/// Whether `url` is a Kaggle page [`list`] reads.
pub fn lists(url: &Url) -> bool {
    kaggle_of(url).is_some()
}

/// The archive of the dataset or competition `url` names (see the module docs).
pub async fn list(_http: &Client, url: &Url, options: &ListOptions) -> Result<Vec<Task>, String> {
    let kaggle = kaggle_of(url).ok_or_else(|| format!("{url} is not a Kaggle dataset or competition"))?;
    let auth = credentials(options.keys.kaggle.as_deref())?;
    Ok(vec![archive(&crate::itch::unredirected(options)?, API, &kaggle, auth).await?])
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    key: String,
}

/// The `Authorization` header of the user's `kaggle.json` at `path`, else at its usual place;
/// None when there is none there. A file the user named that cannot be read is an error.
fn credentials(path: Option<&str>) -> Result<Option<HeaderValue>, String> {
    let named = path.map(str::trim).filter(|p| !p.is_empty()).map(PathBuf::from);
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    let Some(file) = named.clone().or_else(|| home.map(|h| PathBuf::from(h).join(".kaggle").join("kaggle.json"))) else { return Ok(None) };
    let text = match std::fs::read(&file) {
        Ok(text) => text,
        Err(_) if named.is_none() => return Ok(None),
        Err(e) => return Err(format!("Cannot read your kaggle.json {}: {e}", file.display())),
    };
    let found: Credentials = serde_json::from_slice(&text).map_err(|_| format!("{} is not a kaggle.json (username and key)", file.display()))?;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", found.username, found.key));
    let mut value = HeaderValue::from_str(&format!("Basic {basic}")).map_err(|_| format!("{} holds a key that cannot be sent", file.display()))?;
    value.set_sensitive(true);
    Ok(Some(value))
}

/// The task of `kaggle`'s archive: the link of Kaggle's storage its download endpoint redirects
/// to (signed, valid for days), which never gets the credentials.
async fn archive(http: &Client, api: &str, kaggle: &Kaggle, auth: Option<HeaderValue>) -> Result<Task, String> {
    let (path, name, page) = match kaggle {
        Kaggle::Dataset { owner, slug, version } => {
            let version = version.map(|v| format!("?datasetVersionNumber={v}")).unwrap_or_default();
            (format!("datasets/download/{owner}/{slug}{version}"), format!("{slug}.zip"), format!("https://www.kaggle.com/datasets/{owner}/{slug}"))
        }
        Kaggle::Competition(name) => {
            if auth.is_none() {
                return Err("Kaggle's competitions need your kaggle.json (from the Settings page of your Kaggle account): set its place in Settings, or with --kaggle-json".to_string());
            }
            (format!("competitions/data/download-all/{name}"), format!("{name}.zip"), format!("https://www.kaggle.com/competitions/{name}"))
        }
    };
    let url = Url::parse(&format!("{api}/{path}")).map_err(|e| e.to_string())?;
    let answer = crate::polite::send(&url, GAP, || {
        let request = http.get(url.clone());
        match &auth {
            Some(auth) => request.header(AUTHORIZATION, auth.clone()),
            None => request,
        }
    })
    .await?;
    let status = answer.status();
    if status.is_redirection() {
        let link = answer.headers().get(LOCATION).and_then(|l| l.to_str().ok()).and_then(|l| url.join(l).ok()).ok_or("Kaggle sent the download nowhere")?;
        return Ok(Task { urls: vec![link], name: Some(clean_path([name.as_str()])?), from_document: true, ..Task::default() });
    }
    let said = crate::archives::body(answer, "Kaggle").await.ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).and_then(|v| v["message"].as_str().map(str::to_string));
    Err(match (status, kaggle) {
        (StatusCode::FORBIDDEN, Kaggle::Competition(_)) => format!("Join this competition and accept its rules on Kaggle first ({page}/rules), then add it again"),
        (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN, _) if auth.is_none() => format!("{page} needs your kaggle.json: set its place in Settings, or with --kaggle-json"),
        (StatusCode::UNAUTHORIZED, _) => "Kaggle refused the key of your kaggle.json: make a new one on Kaggle's Settings page".to_string(),
        (StatusCode::NOT_FOUND, _) => format!("Kaggle has no {page}"),
        _ => said.map(|m| format!("Kaggle: {m}")).unwrap_or_else(|| crate::polite::refused("Kaggle", status)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::serve;

    #[test]
    fn kaggle_links_and_credentials_are_read() {
        let of = |s: &str| kaggle_of(&Url::parse(s).unwrap());
        assert_eq!(of("https://www.kaggle.com/datasets/uciml/iris"), Some(Kaggle::Dataset { owner: "uciml".into(), slug: "iris".into(), version: None }));
        assert_eq!(of("https://www.kaggle.com/datasets/uciml/iris/versions/2"), Some(Kaggle::Dataset { owner: "uciml".into(), slug: "iris".into(), version: Some(2) }));
        assert_eq!(of("https://www.kaggle.com/c/titanic/data"), Some(Kaggle::Competition("titanic".into())));
        assert_eq!(of("https://www.kaggle.com/competitions/titanic"), Some(Kaggle::Competition("titanic".into())));
        for not in ["https://www.kaggle.com/datasets", "https://www.kaggle.com/code/x/y", "https://example.com/datasets/a/b"] {
            assert_eq!(of(not), None, "{not}");
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("kaggle.json");
        std::fs::write(&file, r#"{"username":"me","key":"k"}"#).unwrap();
        let auth = credentials(file.to_str()).unwrap().unwrap();
        assert_eq!(auth.to_str().unwrap(), "Basic bWU6aw==");
        assert!(credentials(dir.path().join("none.json").to_str()).unwrap_err().contains("Cannot read"));
    }

    /// A dataset's archive is where Kaggle redirects, without credentials when there are none; a
    /// competition needs them, and one not joined says so.
    #[tokio::test]
    async fn datasets_and_competitions_are_their_archives() {
        let (addr, _) = serve(|path, _| match path {
            "/api/datasets/download/uciml/iris?datasetVersionNumber=2" => (302, "Location: https://storage.example/iris.zip?sig=1\r\n".into(), Vec::new()),
            "/private/competitions/data/download-all/titanic" => (302, "Location: https://storage.example/titanic.zip\r\n".into(), Vec::new()),
            _ => (403, String::new(), br#"{"code":403,"message":"forbidden"}"#.to_vec()),
        })
        .await;
        let http = Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let dataset = Kaggle::Dataset { owner: "uciml".into(), slug: "iris".into(), version: Some(2) };
        let task = archive(&http, &format!("http://{addr}/api"), &dataset, None).await.unwrap();
        assert_eq!((task.urls[0].as_str(), task.name.as_deref()), ("https://storage.example/iris.zip?sig=1", Some(std::path::Path::new("iris.zip"))));

        let titanic = Kaggle::Competition("titanic".into());
        assert!(archive(&http, &format!("http://{addr}/private"), &titanic, None).await.unwrap_err().contains("need your kaggle.json"));
        // The canned server's /private/ takes this header alone.
        let auth = Some(HeaderValue::from_static("Bearer secret"));
        let task = archive(&http, &format!("http://{addr}/private"), &titanic, auth).await.unwrap();
        assert_eq!(task.urls[0].as_str(), "https://storage.example/titanic.zip");
        let other = Kaggle::Competition("closed".into());
        let joined = archive(&http, &format!("http://{addr}/private"), &other, Some(HeaderValue::from_static("Bearer secret"))).await.unwrap_err();
        assert!(joined.contains("accept its rules") && joined.contains("/competitions/closed/rules"), "{joined}");
    }
}
