//! Open-access papers: a DOI (`10.1371/journal.pone.0000308`, `doi:...`, `doi.org/...`,
//! `dx.doi.org/...`) or a publisher's page whose link holds one, PubMed Central articles and
//! bioRxiv or medRxiv preprints, each to a legal open copy. A DOI goes to Unpaywall, with the
//! email address the user sets for it (its terms ask for one; none is ever made up or taken from
//! elsewhere), whose best open copy is downloaded, or the arXiv preprint, PubMed Central article or
//! Zenodo record it names; without an email, or when Unpaywall knows no PDF, the DOI's own
//! metadata (content negotiation at doi.org) gives a PDF link under an open licence, if it has one.
//!
//! Nothing behind a paywall is fetched: without an open copy, the answer says so and gives the
//! paper's landing page.

use std::time::Duration;

use percent_encoding::percent_decode_str;
use reqwest::header::ACCEPT;
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use crate::ingest::{clean_path, truncate_chars, ListOptions, Task};
use crate::repos::url_at;

const UNPAYWALL: &str = "https://api.unpaywall.org/v2";
const DOI_ORG: &str = "https://doi.org";
/// PubMed Central's open-access articles, in its cloud service's public bucket.
const PMC_BUCKET: &str = "https://pmc-oa-opendata.s3.amazonaws.com";
const BIORXIV_API: &str = "https://api.biorxiv.org";
/// Between two requests to one of these services.
const GAP: Duration = Duration::from_millis(200);
/// Path parts after which a publisher's link names the paper's DOI (`/doi/10...`, `/article/10...`).
const DOI_AFTER: &[&str] = &["doi", "abs", "full", "pdf", "epdf", "epub", "reader", "article", "chapter", "book"];
const NO_EMAIL: &str = "Unpaywall, which finds a paper's legal open copy, asks for an email address: set yours in Settings (Unpaywall email), or with --unpaywall-email";

/// What a paper link is.
#[derive(Debug, PartialEq)]
enum Paper {
    Doi(String),
    /// PMC{digits}
    Pmc(String),
    /// A bioRxiv or medRxiv preprint: its server, DOI and version when the link names one.
    Preprint { server: &'static str, doi: String, version: Option<u32> },
}

/// Whether `text` is a DOI: `10.`, a registrant of 4 to 9 digits (dots between parts allowed),
/// `/` and a suffix with no spaces.
fn is_doi(text: &str) -> bool {
    let Some((prefix, suffix)) = text.strip_prefix("10.").and_then(|rest| rest.split_once('/')) else { return false };
    let digits = prefix.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    digits && (4..=20).contains(&prefix.len()) && !suffix.is_empty() && !suffix.contains(char::is_whitespace)
}

/// The doi.org link of a bare DOI the user typed (`10.xxxx/...` or `doi:10.xxxx/...`).
pub fn doi_link(token: &str) -> Option<Url> {
    let token = token.trim();
    let doi = token.get(..4).filter(|p| p.eq_ignore_ascii_case("doi:")).map_or(token, |_| token[4..].trim());
    is_doi(doi).then(|| url_at(DOI_ORG, doi.split('/')).ok()).flatten()
}

fn paper_of(url: &Url) -> Option<Paper> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let decoded: Vec<String> = url.path_segments()?.map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let segs: Vec<&str> = decoded.iter().map(String::as_str).filter(|s| !s.is_empty()).collect();
    let host = url.host_str()?.trim_end_matches('.');
    match (host.strip_prefix("www.").unwrap_or(host), &segs[..]) {
        ("doi.org" | "dx.doi.org", _) => {
            let doi = segs.join("/");
            is_doi(&doi).then_some(Paper::Doi(doi))
        }
        ("ncbi.nlm.nih.gov", ["pmc", "articles", id, ..]) | ("pmc.ncbi.nlm.nih.gov", ["articles", id, ..]) => {
            let digits = id.strip_prefix("PMC").or_else(|| id.strip_prefix("pmc"))?;
            digits.bytes().all(|b| b.is_ascii_digit()).then(|| Paper::Pmc(digits.to_string())).filter(|_| !digits.is_empty())
        }
        (site @ ("biorxiv.org" | "medrxiv.org"), ["content", "10.1101", id, ..]) => {
            // 2020.03.19.999086v2.full.pdf: the preprint, its version, any ending.
            let id = id.split(".full").next().unwrap_or_default().trim_end_matches(".pdf");
            let (id, version) = match id.rsplit_once('v') {
                Some((id, v)) if v.bytes().all(|b| b.is_ascii_digit()) && !v.is_empty() => (id, v.parse().ok()),
                _ => (id, None),
            };
            let server = if site == "biorxiv.org" { "biorxiv" } else { "medrxiv" };
            Some(Paper::Preprint { server, doi: format!("10.1101/{id}"), version })
        }
        _ if url.path().to_ascii_lowercase().ends_with(".pdf") => None,
        _ => {
            let after = segs.windows(2).position(|w| DOI_AFTER.contains(&w[0].to_ascii_lowercase().as_str()) && w[1].starts_with("10."));
            let in_query = || url.query_pairs().filter(|(k, _)| k == "doi" || k == "id").map(|(_, v)| v.into_owned()).find(|v| is_doi(v));
            let doi = after.map(|at| segs[at + 1..].join("/")).or_else(in_query);
            doi.filter(|d| is_doi(d)).map(Paper::Doi)
        }
    }
}

/// Whether `url` is a DOI, PubMed Central or preprint link [`list`] reads.
pub fn lists(url: &Url) -> bool {
    paper_of(url).is_some()
}

/// The open copy of the paper `url` names (see the module docs).
pub async fn list(http: &Client, url: &Url, options: &ListOptions) -> Result<Vec<Task>, String> {
    let email = options.keys.unpaywall_email.as_deref().map(str::trim).filter(|e| e.contains('@'));
    match paper_of(url).ok_or_else(|| format!("{url} names no paper"))? {
        Paper::Doi(doi) => resolve(http, &Services::default(), &doi, email, options).await,
        Paper::Pmc(id) => pmc(http, PMC_BUCKET, &id).await,
        Paper::Preprint { server, doi, version } => preprint(http, BIORXIV_API, server, &doi, version).await,
    }
}

/// Where the services are asked (canned servers in tests).
struct Services {
    unpaywall: String,
    doi: String,
    pmc: String,
}

impl Default for Services {
    fn default() -> Self {
        Services { unpaywall: UNPAYWALL.into(), doi: DOI_ORG.into(), pmc: PMC_BUCKET.into() }
    }
}

/// What `site` answers at `url` (asked with `accept`), paced; None for 404 or 410.
async fn get(http: &Client, url: &Url, site: &str, accept: &str) -> Result<Option<Vec<u8>>, String> {
    let answer = crate::polite::send(url, GAP, || http.get(url.clone()).header(ACCEPT, accept)).await?;
    match answer.status() {
        StatusCode::NOT_FOUND | StatusCode::GONE => Ok(None),
        StatusCode::UNPROCESSABLE_ENTITY if site == "Unpaywall" => Err("Unpaywall refused the email address set for it: check it in Settings".to_string()),
        status if !status.is_success() => Err(crate::polite::refused(site, status)),
        _ => crate::archives::body(answer, site).await.map(Some),
    }
}

async fn json<T: DeserializeOwned>(http: &Client, url: &Url, site: &str, accept: &str) -> Result<Option<T>, String> {
    match get(http, url, site, accept).await? {
        Some(body) => serde_json::from_slice(&body).map(Some).map_err(|_| format!("{site} answered with something the app cannot read")),
        None => Ok(None),
    }
}

/// The paper's file name: its title, else its DOI.
fn named(title: Option<&str>, doi: &str) -> Option<std::path::PathBuf> {
    let title = title.map(|t| truncate_chars(t.split_whitespace().collect::<Vec<_>>().join(" ").as_str(), 120)).filter(|t| !t.is_empty());
    let name = format!("{}.pdf", title.unwrap_or_else(|| doi.replace('/', "_")));
    clean_path([name.as_str()]).ok().or_else(|| clean_path(["paper.pdf"]).ok())
}

#[derive(Deserialize)]
struct Unpaywall {
    title: Option<String>,
    #[serde(default)]
    is_oa: bool,
    best_oa_location: Option<Location>,
    #[serde(default)]
    oa_locations: Vec<Location>,
}

#[derive(Deserialize)]
struct Location {
    url: Option<String>,
    url_for_pdf: Option<String>,
}

#[derive(Deserialize)]
struct Csl {
    title: Option<String>,
    #[serde(default)]
    link: Vec<CslLink>,
    #[serde(default)]
    license: Vec<CslLicense>,
}

#[derive(Deserialize)]
struct CslLink {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "content-type")]
    content_type: Option<String>,
}

#[derive(Deserialize)]
struct CslLicense {
    #[serde(rename = "URL")]
    url: String,
}

/// The open copy of `doi`: an arXiv DOI's preprint; Unpaywall's best PDF, or the arXiv, PubMed
/// Central or Zenodo copy it names; else a PDF the DOI's metadata links under a Creative Commons
/// licence. An error with the landing page when there is none.
async fn resolve(http: &Client, at: &Services, doi: &str, email: Option<&str>, options: &ListOptions) -> Result<Vec<Task>, String> {
    if let Some(id) = doi.get(..15).filter(|p| p.eq_ignore_ascii_case("10.48550/arxiv.")).map(|_| &doi[15..]) {
        return elsewhere(http, &format!("https://arxiv.org/abs/{id}"), options).await.unwrap_or_else(|| Err(format!("arXiv has no paper {id}")));
    }
    let landing = format!("https://doi.org/{doi}");
    let mut page_only = None;
    if let Some(email) = email {
        let mut url = url_at(&at.unpaywall, doi.split('/'))?;
        url.query_pairs_mut().append_pair("email", email);
        if let Some(found) = json::<Unpaywall>(http, &url, "Unpaywall", "application/json").await?.filter(|f| f.is_oa) {
            let locations: Vec<&Location> = found.best_oa_location.iter().chain(&found.oa_locations).collect();
            if let Some(pdf) = locations.iter().find_map(|l| Url::parse(l.url_for_pdf.as_deref()?).ok()) {
                return Ok(vec![Task { urls: vec![pdf], name: named(found.title.as_deref(), doi), from_document: true, ..Task::default() }]);
            }
            // A repository's page: arXiv, PubMed Central and Zenodo give their files.
            for page in locations.iter().filter_map(|l| l.url.as_deref()) {
                if let Some(Paper::Pmc(id)) = Url::parse(page).ok().as_ref().and_then(paper_of) {
                    return pmc(http, &at.pmc, &id).await;
                }
                if let Some(listed) = elsewhere(http, page, options).await {
                    return listed;
                }
            }
            page_only = locations.iter().find_map(|l| l.url.clone());
        }
    }
    let url = url_at(&at.doi, doi.split('/'))?;
    let csl = json::<Csl>(http, &url, "doi.org", "application/vnd.citationstyles.csl+json").await?.ok_or_else(|| format!("doi.org knows no DOI {doi}"))?;
    let open = csl.license.iter().any(|l| l.url.contains("creativecommons.org"));
    let pdf = csl.link.iter().filter(|_| open).find(|l| l.content_type.as_deref() == Some("application/pdf")).and_then(|l| Url::parse(&l.url).ok());
    if let Some(pdf) = pdf.filter(|u| matches!(u.scheme(), "http" | "https")) {
        return Ok(vec![Task { urls: vec![pdf], name: named(csl.title.as_deref(), doi), from_document: true, ..Task::default() }]);
    }
    Err(match (email, page_only) {
        (_, Some(page)) => format!("The paper's open copy is a web page, not a file: open {page}"),
        (Some(_), None) => format!("No legal open copy of this paper is known: its page is {landing}"),
        (None, None) => format!("{NO_EMAIL}. Its page is {landing}"),
    })
}

/// The files of an arXiv paper or Zenodo record at `page`, as their own links list them.
async fn elsewhere(http: &Client, page: &str, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    let url = Url::parse(page).ok()?;
    let host = url.host_str()?.trim_start_matches("www.");
    if !matches!(host, "arxiv.org" | "zenodo.org") {
        return None;
    }
    crate::archives::list(http, &url, options).await
}

/// PubMed Central's article PMC`id`, as its cloud service's open-access bucket holds it: the PDF
/// of its latest version, with its size and MD5.
async fn pmc(http: &Client, bucket: &str, id: &str) -> Result<Vec<Task>, String> {
    let mut url = Url::parse(bucket).map_err(|e| e.to_string())?;
    url.query_pairs_mut().append_pair("list-type", "2").append_pair("prefix", &format!("PMC{id}.")).append_pair("max-keys", "1000");
    let page = format!("https://pmc.ncbi.nlm.nih.gov/articles/PMC{id}/");
    let missing = || format!("PubMed Central's open-access set has no PDF of PMC{id}: its page is {page}");
    let xml = get(http, &url, "PubMed Central", "application/xml").await?.ok_or_else(missing)?;
    let (records, _) = crate::cloud::xml_records(&String::from_utf8_lossy(&xml), b"Contents");
    let version = |key: &str| key.strip_prefix(&format!("PMC{id}."))?.split_once('/')?.0.parse::<u32>().ok();
    let pdf = records.iter().filter_map(|r| Some((r.get("Key")?, r))).filter(|(key, _)| key.ends_with(".pdf")).max_by_key(|(key, _)| version(key));
    let (key, record) = pdf.ok_or_else(missing)?;
    let etag = record.get("ETag").map(|e| e.trim_matches('"').to_ascii_lowercase()).unwrap_or_default();
    Ok(vec![Task {
        urls: vec![url_at(bucket, key.split('/'))?],
        name: clean_path([format!("PMC{id}.pdf").as_str()]).ok(),
        size: record.get("Size").and_then(|s| s.parse().ok()),
        // A one-part upload's ETag is its MD5; a multipart one's ("...-2") is not.
        checksum: (etag.len() == 32 && etag.bytes().all(|b| b.is_ascii_hexdigit())).then(|| format!("md5:{etag}")),
        from_document: true,
        ..Task::default()
    }])
}

#[derive(Deserialize)]
struct Details {
    #[serde(default)]
    collection: Vec<Detail>,
}

#[derive(Deserialize)]
struct Detail {
    title: Option<String>,
    version: String,
}

/// The PDF of a bioRxiv or medRxiv preprint at `version`, else its latest one, as `server`'s API
/// lists its versions.
async fn preprint(http: &Client, api: &str, server: &str, doi: &str, version: Option<u32>) -> Result<Vec<Task>, String> {
    let url = url_at(api, ["details", server].into_iter().chain(doi.split('/')))?;
    let details: Details = json(http, &url, server, "application/json").await?.ok_or_else(|| format!("{server} has no preprint {doi}"))?;
    let chosen = match version {
        Some(v) => details.collection.iter().find(|d| d.version == v.to_string()),
        None => details.collection.iter().max_by_key(|d| d.version.parse::<u32>().unwrap_or(0)),
    };
    let chosen = chosen.ok_or_else(|| format!("{server} has no such version of preprint {doi}"))?;
    let pdf = Url::parse(&format!("https://www.{server}.org/content/{doi}v{}.full.pdf", chosen.version)).map_err(|e| e.to_string())?;
    Ok(vec![Task { urls: vec![pdf], name: named(chosen.title.as_deref(), doi), from_document: true, ..Task::default() }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{serve, Reply};

    fn http() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    fn reply(body: &str) -> Reply {
        (200, "Content-Type: application/json\r\n".into(), body.as_bytes().to_vec())
    }

    fn paper(link: &str) -> Option<Paper> {
        paper_of(&Url::parse(link).unwrap())
    }

    #[test]
    fn paper_links_are_told_by_their_shape() {
        let doi = |d: &str| Some(Paper::Doi(d.into()));
        assert_eq!(doi_link(" doi:10.1371/journal.pone.0000308 ").unwrap().as_str(), "https://doi.org/10.1371/journal.pone.0000308");
        assert_eq!(doi_link("10.1000.10/a<b>#c").unwrap().as_str(), "https://doi.org/10.1000.10/a%3Cb%3E%23c");
        for not in ["10.12/x", "10.abcd/x", "10.1234/", "11.1234/x", "10.1234/a b", "https://doi.org/10.1234/x"] {
            assert!(doi_link(not).is_none(), "{not}");
        }
        assert_eq!(paper("https://doi.org/10.1000.10/a%3Cb%3E%23c"), doi("10.1000.10/a<b>#c"));
        assert_eq!(paper("http://dx.doi.org/10.1038/nature12373"), doi("10.1038/nature12373"));
        assert_eq!(paper("https://onlinelibrary.wiley.com/doi/full/10.1002/anie.201915678"), doi("10.1002/anie.201915678"));
        assert_eq!(paper("https://link.springer.com/article/10.1007/s00145-023-09467-1"), doi("10.1007/s00145-023-09467-1"));
        assert_eq!(paper("https://journals.plos.org/plosone/article?id=x&doi=10.1371/journal.pone.0000308"), doi("10.1371/journal.pone.0000308"));
        assert_eq!(paper("https://www.ncbi.nlm.nih.gov/pmc/articles/PMC5334499/"), Some(Paper::Pmc("5334499".into())));
        assert_eq!(paper("https://pmc.ncbi.nlm.nih.gov/articles/PMC5334499"), Some(Paper::Pmc("5334499".into())));
        let pre = |server, version| Some(Paper::Preprint { server, doi: "10.1101/2020.03.19.999086".into(), version });
        assert_eq!(paper("https://www.biorxiv.org/content/10.1101/2020.03.19.999086v2.full.pdf"), pre("biorxiv", Some(2)));
        assert_eq!(paper("https://www.medrxiv.org/content/10.1101/2020.03.19.999086"), pre("medrxiv", None));
        for not in ["https://doi.org/", "https://example.com/doi/10.1234/x/paper.pdf", "https://example.com/files/10.1234/x", "https://www.ncbi.nlm.nih.gov/pmc/articles/123/"] {
            assert_eq!(paper(not), None, "{not}");
        }
    }

    /// Unpaywall's best PDF is the download, named by the title; without an email the DOI's own
    /// metadata gives a PDF only under an open licence, and otherwise the answer asks for the email
    /// and names the landing page; a paper with no open copy says so.
    #[tokio::test]
    async fn dois_resolve_to_their_legal_open_copy() {
        let (addr, hits) = serve(|path, _| match path {
            "/v2/10.1/open?email=me%40example.org" => reply(
                r#"{"title":"An  Open\nPaper","is_oa":true,"best_oa_location":{"url":"https://repo.example/landing","url_for_pdf":null},
                    "oa_locations":[{"url":"https://repo.example/landing","url_for_pdf":null},{"url":"https://journal.example/x","url_for_pdf":"https://journal.example/x.pdf"}]}"#,
            ),
            "/v2/10.1/closed?email=me%40example.org" => reply(r#"{"title":"Closed","is_oa":false,"best_oa_location":null,"oa_locations":[]}"#),
            "/v2/10.1/page?email=me%40example.org" => reply(r#"{"is_oa":true,"best_oa_location":{"url":"https://repo.example/page","url_for_pdf":null}}"#),
            "/doi/10.1/closed" | "/doi/10.1/page" => reply(r#"{"title":"Closed","link":[{"URL":"https://pub.example/x.pdf","content-type":"application/pdf"}],"license":[{"URL":"https://pub.example/terms"}]}"#),
            "/doi/10.1/cc" => reply(
                r#"{"title":"Libre","link":[{"URL":"https://pub.example/x.xml","content-type":"text/xml"},{"URL":"https://pub.example/libre.pdf","content-type":"application/pdf"}],
                    "license":[{"URL":"http://creativecommons.org/licenses/by/4.0/"}]}"#,
            ),
            _ => (404, String::new(), b"{}".to_vec()),
        })
        .await;
        let base = format!("http://{addr}");
        let at = Services { unpaywall: format!("{base}/v2"), doi: format!("{base}/doi"), pmc: format!("{base}/pmc") };
        let options = ListOptions::default();
        let email = Some("me@example.org");
        let [open] = &resolve(&http(), &at, "10.1/open", email, &options).await.unwrap()[..] else { panic!("one PDF") };
        assert_eq!((open.urls[0].as_str(), open.name.clone().unwrap()), ("https://journal.example/x.pdf", "An Open Paper.pdf".into()));
        let closed = resolve(&http(), &at, "10.1/closed", email, &options).await.unwrap_err();
        assert_eq!(closed, "No legal open copy of this paper is known: its page is https://doi.org/10.1/closed");
        let page = resolve(&http(), &at, "10.1/page", email, &options).await.unwrap_err();
        assert_eq!(page, "The paper's open copy is a web page, not a file: open https://repo.example/page");
        let [libre] = &resolve(&http(), &at, "10.1/cc", None, &options).await.unwrap()[..] else { panic!("one PDF") };
        assert_eq!((libre.urls[0].as_str(), libre.name.clone().unwrap()), ("https://pub.example/libre.pdf", "Libre.pdf".into()));
        let asked = resolve(&http(), &at, "10.1/closed", None, &options).await.unwrap_err();
        assert!(asked.starts_with(NO_EMAIL) && asked.ends_with("https://doi.org/10.1/closed"), "{asked}");
        assert!(hits.lock().keys().all(|p| !p.starts_with("/v2/") || p.contains("email=me%40example.org")), "Unpaywall is asked only with the email set");
        assert_eq!(resolve(&http(), &at, "10.1/gone", None, &options).await.unwrap_err(), "doi.org knows no DOI 10.1/gone");
    }

    /// A PubMed Central article is its latest version's PDF in the open-access bucket, with its size
    /// and MD5; one not there says so. A preprint is its version's PDF, the latest without one.
    #[tokio::test]
    async fn pmc_articles_and_preprints_are_their_pdfs() {
        let (addr, _) = serve(|path, _| match path {
            "/pmc?list-type=2&prefix=PMC5334499.&max-keys=1000" => (
                200,
                "Content-Type: application/xml\r\n".into(),
                br#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult><Contents><Key>PMC5334499.1/PMC5334499.1.pdf</Key><ETag>&quot;9c91f19803174073235fe352906e2ab0&quot;</ETag><Size>1006134</Size></Contents>
                <Contents><Key>PMC5334499.2/PMC5334499.2.pdf</Key><ETag>&quot;0a3d8b5b0fb4a2a3b3c3d3e3f3a3b3c3-2&quot;</ETag><Size>1006200</Size></Contents>
                <Contents><Key>PMC5334499.2/PMC5334499.2.xml</Key><Size>73647</Size></Contents></ListBucketResult>"#
                    .to_vec(),
            ),
            "/pmc?list-type=2&prefix=PMC1.&max-keys=1000" => (200, String::new(), b"<ListBucketResult></ListBucketResult>".to_vec()),
            "/details/biorxiv/10.1101/2020.03.19.999086" => reply(r#"{"collection":[{"title":"Strains","version":"1"},{"title":"Strains, revised","version":"2"}]}"#),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let base = format!("http://{addr}");
        let [pdf] = &pmc(&http(), &format!("{base}/pmc"), "5334499").await.unwrap()[..] else { panic!("one PDF") };
        assert_eq!(pdf.urls[0].as_str(), format!("{base}/pmc/PMC5334499.2/PMC5334499.2.pdf"));
        assert_eq!((pdf.size, pdf.checksum.as_deref(), pdf.name.clone().unwrap()), (Some(1006200), None, "PMC5334499.pdf".into()));
        let none = pmc(&http(), &format!("{base}/pmc"), "1").await.unwrap_err();
        assert_eq!(none, "PubMed Central's open-access set has no PDF of PMC1: its page is https://pmc.ncbi.nlm.nih.gov/articles/PMC1/");
        let latest = preprint(&http(), &base, "biorxiv", "10.1101/2020.03.19.999086", None).await.unwrap();
        assert_eq!(latest[0].urls[0].as_str(), "https://www.biorxiv.org/content/10.1101/2020.03.19.999086v2.full.pdf");
        assert_eq!(latest[0].name.clone().unwrap(), std::path::PathBuf::from("Strains, revised.pdf"));
        let first = preprint(&http(), &base, "biorxiv", "10.1101/2020.03.19.999086", Some(1)).await.unwrap();
        assert!(first[0].urls[0].as_str().ends_with("999086v1.full.pdf"));
        assert!(preprint(&http(), &base, "medrxiv", "10.1101/none", None).await.unwrap_err().contains("medrxiv has no preprint"));
    }
}
