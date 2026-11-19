//! OpenStreetMap data: Geofabrik's extracts (`download.geofabrik.de`), a file with the MD5
//! Geofabrik publishes beside it, a region's page with its sub-regions' files from Geofabrik's
//! index (`index-v1-nogeom.json`); and the planet files of `planet.openstreetmap.org`, from the
//! mirrors their torrent lists as web seeds, with the MD5 published beside them, or from the torrent
//! and its mirrors at once when the swarm is allowed (see `ListOptions::swarm_with_http`).

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use url::Url;

use crate::ingest::{clean_path, ListOptions, Task};

const GEOFABRIK_INDEX: &str = "https://download.geofabrik.de/index-v1-nogeom.json";
/// Between two requests to one of these servers.
const GAP: Duration = Duration::from_millis(300);
/// How long a file's MD5 is waited for before the file downloads unchecked.
const MD5_WAIT: Duration = Duration::from_secs(15);
/// Most files of one region's page listed (Europe has some 70 countries).
const MAX_REGIONS: usize = 100;
/// The kinds of file these servers publish an `.md5` of.
const DATA: &[&str] = &[".osm.pbf", ".osm.bz2", ".shp.zip", ".osh.pbf"];

/// What an OpenStreetMap link is, from its shape.
#[derive(Debug, PartialEq)]
enum Osm {
    /// A file of Geofabrik's.
    File(Url),
    /// A region's page of Geofabrik's (`europe/germany.html`).
    Region(Url),
    /// A planet file (`pbf/planet-latest.osm.pbf`).
    Planet(Url),
}

fn osm_of(url: &Url) -> Option<Osm> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let path = url.path();
    let data = DATA.iter().any(|ext| path.ends_with(ext));
    match url.host_str()?.trim_end_matches('.').to_ascii_lowercase().as_str() {
        "download.geofabrik.de" if data => Some(Osm::File(url.clone())),
        "download.geofabrik.de" if path.ends_with(".html") && path != "/index.html" => Some(Osm::Region(url.clone())),
        "planet.openstreetmap.org" | "planet.osm.org" if data => Some(Osm::Planet(url.clone())),
        _ => None,
    }
}

/// Whether `url` is an OpenStreetMap link [`list`] reads.
pub fn lists(url: &Url) -> bool {
    osm_of(url).is_some()
}

/// The files of the OpenStreetMap link `url` (see the module docs).
pub async fn list(http: &Client, url: &Url, options: &ListOptions) -> Result<Vec<Task>, String> {
    match osm_of(url).ok_or_else(|| format!("{url} is no OpenStreetMap data"))? {
        Osm::File(file) => Ok(vec![with_md5(http, file, "Geofabrik").await?]),
        Osm::Region(page) => region(http, &Url::parse(GEOFABRIK_INDEX).map_err(|e| e.to_string())?, &page, options).await,
        Osm::Planet(file) => planet(http, file, options).await,
    }
}

/// The task of `file`, named as it is, with the MD5 published beside it (`<file>.md5`, as
/// `md5sum` writes it) when there is one. An MD5 that cannot be had now leaves the file unchecked,
/// as a link to it downloads anyway.
async fn with_md5(http: &Client, file: Url, site: &str) -> Result<Task, String> {
    let name = file.path_segments().and_then(|mut s| s.next_back()).map(|n| percent_encoding::percent_decode_str(n).decode_utf8_lossy().into_owned()).unwrap_or_default();
    // A `-latest` link redirects to the dated file, beside which its MD5 is: Geofabrik's answer
    // for `-latest...md5` is a 404 it takes half a minute over.
    let dated = match tokio::time::timeout(MD5_WAIT, crate::polite::send(&file, GAP, || http.head(file.clone()))).await {
        Ok(Ok(answer)) if answer.status().is_success() => answer.url().clone(),
        _ => file.clone(),
    };
    let sum = Url::parse(&format!("{dated}.md5")).map_err(|e| e.to_string())?;
    let asked = tokio::time::timeout(MD5_WAIT, crate::polite::get(http, &sum, GAP)).await;
    let checksum = match asked.unwrap_or_else(|_| Err(format!("{site} did not answer for {sum} in time"))) {
        Ok(answer) if answer.status().is_success() => {
            let body = crate::archives::body(answer, site).await.unwrap_or_default();
            String::from_utf8_lossy(&body).split_whitespace().next().and_then(|hex| crate::repos::checksum("md5", hex))
        }
        Ok(answer) if answer.status() == StatusCode::NOT_FOUND => None,
        Ok(answer) => {
            tracing::info!("{site} answered HTTP {} for {sum}: {name} downloads unchecked", answer.status().as_u16());
            None
        }
        Err(e) => {
            tracing::info!("{e}: {name} downloads unchecked");
            None
        }
    };
    Ok(Task { urls: vec![file], name: Some(clean_path([name.as_str()])?), checksum, ..Task::default() })
}

#[derive(Deserialize)]
struct Index {
    features: Vec<Feature>,
}

#[derive(Deserialize)]
struct Feature {
    properties: Region,
}

#[derive(Deserialize)]
struct Region {
    id: String,
    parent: Option<String>,
    #[serde(default)]
    urls: Urls,
}

#[derive(Default, Deserialize)]
struct Urls {
    pbf: Option<Url>,
}

/// The region of Geofabrik's page `page` (as `index` lists it): its `.osm.pbf` and those of its
/// sub-regions, each with its MD5.
async fn region(http: &Client, index: &Url, page: &Url, options: &ListOptions) -> Result<Vec<Task>, String> {
    let answer = crate::polite::get(http, index, GAP).await?;
    if !answer.status().is_success() {
        return Err(crate::polite::refused("Geofabrik", answer.status()));
    }
    let found: Index = serde_json::from_slice(&crate::archives::body(answer, "Geofabrik").await?).map_err(|_| "Geofabrik's index cannot be read".to_string())?;
    let pbf = format!("{}-latest.osm.pbf", page.path().trim_end_matches(".html"));
    let regions: Vec<Region> = found.features.into_iter().map(|f| f.properties).collect();
    let this = regions.iter().find(|r| r.urls.pbf.as_ref().is_some_and(|u| u.path() == pbf)).ok_or_else(|| format!("Geofabrik's index has no region of {page}"))?;
    let under: Vec<&Region> = regions.iter().filter(|r| r.parent.as_deref() == Some(this.id.as_str())).collect();
    if under.len() > MAX_REGIONS {
        options.note(format!("Geofabrik: the first {MAX_REGIONS} of {} sub-regions are listed", under.len()));
    }
    let mut tasks = Vec::new();
    for region in std::iter::once(this).chain(under.into_iter().take(MAX_REGIONS)) {
        if let Some(file) = region.urls.pbf.clone() {
            // Links its index gives, not the user: the user's headers, cookies and referer stay home.
            tasks.push(Task { from_document: true, ..with_md5(http, file, "Geofabrik").await? });
        }
    }
    Ok(tasks)
}

/// A planet file: the dated file its torrent (`<file>.torrent`) holds, from the torrent's web
/// seeds (the planet's mirrors) with the MD5 published beside it, or from the torrent, its web
/// seeds and the swarm at once when the swarm is allowed. Without a torrent, the file with its MD5.
async fn planet(http: &Client, file: Url, options: &ListOptions) -> Result<Vec<Task>, String> {
    let torrent = Url::parse(&format!("{file}.torrent")).map_err(|e| e.to_string())?;
    let answer = crate::polite::get(http, &torrent, GAP).await?;
    let at = answer.url().clone();
    let info = match answer.status().is_success() {
        true => crate::torrent::parse_torrent_bytes(&crate::archives::body(answer, "OpenStreetMap").await?).ok().filter(|i| i.files.len() == 1),
        false => None,
    };
    let Some(info) = info else { return Ok(vec![with_md5(http, file, "OpenStreetMap").await?]) };
    // The dated file the torrent holds, beside the link (planet-latest names the newest).
    let dated = file.join(&info.name).map_err(|e| e.to_string())?;
    let mut mirrors: Vec<Url> = info.web_seeds.iter().map(|seed| if seed.path().ends_with('/') { seed.join(&info.name).unwrap_or_else(|_| seed.clone()) } else { seed.clone() }).collect();
    if mirrors.is_empty() {
        mirrors.push(dated.clone());
    }
    let size = Some(info.total_length);
    // The mirrors are the torrent's, hosts the user never named: their headers stay home.
    if options.swarm_with_http() && info.total_length >= crate::p2p::SWARM_MIN {
        options.note("Downloading from the planet's torrent swarm and its mirrors at once".to_string());
        return Ok(vec![Task { urls: [vec![at], mirrors].concat(), name: Some(clean_path([info.name.as_str()])?), size, p2p: true, from_document: true, ..Task::default() }]);
    }
    let task = with_md5(http, dated, "OpenStreetMap").await?;
    Ok(vec![Task { urls: mirrors, size, from_document: true, ..task }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{ok, serve};
    use std::sync::OnceLock;

    #[test]
    fn osm_links_are_told_by_their_shape() {
        let of = |s: &str| osm_of(&Url::parse(s).unwrap());
        let url = |s: &str| Url::parse(s).unwrap();
        let file = "https://download.geofabrik.de/europe/germany/bremen-latest.osm.pbf";
        assert_eq!(of(file), Some(Osm::File(url(file))));
        assert_eq!(of("https://download.geofabrik.de/europe/germany.html"), Some(Osm::Region(url("https://download.geofabrik.de/europe/germany.html"))));
        let planet = "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf";
        assert_eq!(of(planet), Some(Osm::Planet(url(planet))));
        for not in ["https://download.geofabrik.de/index.html", "https://download.geofabrik.de/europe/germany-updates/state.txt", "https://planet.openstreetmap.org/replication/minute/state.txt", "https://example.com/a.osm.pbf"] {
            assert_eq!(of(not), None, "{not}");
        }
    }

    /// A Geofabrik file with its MD5, a region's page with its sub-regions', and a planet file
    /// from its torrent's mirrors with the dated file's MD5.
    #[tokio::test]
    async fn extracts_and_planets_come_with_their_md5_and_mirrors() {
        static BASE: OnceLock<String> = OnceLock::new();
        let (addr, _) = serve(|path, _| {
            let base = BASE.get().unwrap();
            let seed = format!("{base}/mirror/planet-261109.osm.pbf");
            match path {
                "/index.json" => ok(format!(r#"{{"type":"FeatureCollection","features":[
                    {{"properties":{{"id":"europe","urls":{{"pbf":"{base}/europe-latest.osm.pbf"}}}}}},
                    {{"properties":{{"id":"germany","parent":"europe","urls":{{"pbf":"{base}/europe/germany-latest.osm.pbf"}}}}}},
                    {{"properties":{{"id":"bremen","parent":"germany","urls":{{"pbf":"{base}/europe/germany/bremen-latest.osm.pbf"}}}}}},
                    {{"properties":{{"id":"berlin","parent":"germany","urls":{{"pbf":"{base}/europe/germany/berlin-latest.osm.pbf"}}}}}},
                    {{"properties":{{"id":"hamburg","parent":"germany","urls":{{"pbf":"{base}/europe/germany/hamburg-latest.osm.pbf"}}}}}}]}}"#)),
                "/europe/germany/hamburg-latest.osm.pbf.md5" => (500, String::new(), Vec::new()),
                "/europe/monaco-latest.osm.pbf" => (307, format!("Location: {base}/europe/monaco-261118.osm.pbf\r\n"), Vec::new()),
                "/europe/monaco-261118.osm.pbf" => ok(""),
                "/europe/monaco-261118.osm.pbf.md5" => ok("524e54114b05f99865e8dfb37a576d35  monaco-261118.osm.pbf\n"),
                "/europe/germany-latest.osm.pbf.md5" => ok("04a445a266cc62177eb37c3bc489268a  germany-latest.osm.pbf\n"),
                "/europe/germany/bremen-latest.osm.pbf.md5" => ok("14a445a266cc62177eb37c3bc489268a  bremen-latest.osm.pbf\n"),
                "/pbf/planet-latest.osm.pbf.torrent" => (302, format!("Location: {base}/pbf/planet-261109.osm.pbf.torrent\r\n"), Vec::new()),
                "/pbf/planet-261109.osm.pbf.torrent" => ok(format!(
                    "d8:url-listl{}:{seed}{}:{base}/other/e4:infod6:lengthi300e4:name21:planet-261109.osm.pbf12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee",
                    seed.len(),
                    base.len() + "/other/".len()
                )),
                "/pbf/planet-261109.osm.pbf.md5" => ok("62cc0443e92674d338d0a48668efd9b1  planet-261109.osm.pbf\n"),
                _ => (404, String::new(), Vec::new()),
            }
        })
        .await;
        let base = format!("http://{addr}");
        BASE.set(base.clone()).unwrap();
        let (http, options) = (Client::new(), ListOptions::default());
        let at = |p: &str| Url::parse(&format!("{base}{p}")).unwrap();

        let tasks = region(&http, &at("/index.json"), &at("/europe/germany.html"), &options).await.unwrap();
        let got: Vec<_> = tasks.iter().map(|t| (t.urls[0].path().to_string(), t.checksum.clone())).collect();
        assert_eq!(got, [
            ("/europe/germany-latest.osm.pbf".to_string(), Some("md5:04a445a266cc62177eb37c3bc489268a".to_string())),
            ("/europe/germany/bremen-latest.osm.pbf".to_string(), Some("md5:14a445a266cc62177eb37c3bc489268a".to_string())),
            ("/europe/germany/berlin-latest.osm.pbf".to_string(), None),
            // Its MD5 answered 500: the file downloads unchecked rather than not at all.
            ("/europe/germany/hamburg-latest.osm.pbf".to_string(), None),
        ]);
        assert!(tasks.iter().all(|t| t.from_document), "the user's headers go to no listed link");

        // A -latest link's MD5 is the one beside the dated file it redirects to.
        let latest = with_md5(&http, at("/europe/monaco-latest.osm.pbf"), "Geofabrik").await.unwrap();
        assert_eq!((latest.urls[0].path(), latest.checksum.as_deref()), ("/europe/monaco-latest.osm.pbf", Some("md5:524e54114b05f99865e8dfb37a576d35")));

        let planet = planet(&http, at("/pbf/planet-latest.osm.pbf"), &options).await.unwrap();
        let task = &planet[0];
        assert!(task.from_document, "the mirrors are the torrent's");
        assert_eq!(task.urls.iter().map(Url::as_str).collect::<Vec<_>>(), [format!("{base}/mirror/planet-261109.osm.pbf"), format!("{base}/other/planet-261109.osm.pbf")]);
        assert_eq!((task.name.as_deref(), task.size, task.p2p), (Some(std::path::Path::new("planet-261109.osm.pbf")), Some(300), false));
        assert_eq!(task.checksum.as_deref(), Some("md5:62cc0443e92674d338d0a48668efd9b1"));
    }
}
