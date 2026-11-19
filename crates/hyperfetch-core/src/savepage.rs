//! Saves a web page as one self-contained HTML file (`--save-page URL`), which opens offline as it
//! looked: its images, style sheets (with what they import and use: images, fonts) and iframes are
//! put into it as `data:` URIs, its other links made full, its `<base>` left out. Its scripts are
//! left out unless kept (then put in too), since the page shows what it served, not what a script
//! would build; `srcset` is emptied so that the image put in shows. The page's bytes are kept as
//! served (a page not in UTF-8 stays in its encoding), and a comment first says where and when it
//! was saved from, with the SHA-256 of the page as served.
//!
//! As polite as `--mirror` (see `crate::sitemirror`, whose HTML and CSS reading it shares): each
//! site's robots.txt is honoured, with its Crawl-delay, one request at a time; what robots.txt
//! forbids, what fails and what would pass the size cap stays a link. Every answer can also go
//! into a WARC file (see `crate::warc`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use sha2::{Digest, Sha256};
use url::Url;

use crate::sitemirror::{attr_span, css_links, html_links, page_base, rewrite, robots_of, robots_path, Robots};

/// How a page is saved.
#[derive(Debug, Clone)]
pub struct SaveOptions {
    /// Keep the page's scripts (put in too); by default they are left out.
    pub keep_scripts: bool,
    /// Most bytes fetched in all: the page and what it uses.
    pub max_bytes: u64,
    /// A WARC file each answer is also written to, as received.
    pub warc: Option<PathBuf>,
}

impl Default for SaveOptions {
    fn default() -> Self {
        Self { keep_scripts: false, max_bytes: 64 << 20, warc: None }
    }
}

/// What saving a page did.
#[derive(Debug)]
pub struct Saved {
    pub file: PathBuf,
    /// The saved file's size.
    pub bytes: u64,
    /// How many things the page uses were put in; those left as links, and why.
    pub inlined: usize,
    pub left: Vec<(String, String)>,
}

/// Iframes are put in this many deep; deeper ones stay links.
const MAX_FRAMES: usize = 2;
/// Style sheets that import style sheets are followed this many deep.
const MAX_IMPORTS: usize = 4;

/// `bytes` as text, losing none: as UTF-8 when they are, else a char for each byte (see `bytes_of`).
fn text_of(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_string(), true),
        Err(_) => (bytes.iter().map(|&b| b as char).collect(), false),
    }
}

/// The bytes of `text` from [`text_of`]: what was added to it is ASCII, so each char of a page not
/// in UTF-8 is its byte again.
fn bytes_of(text: String, utf8: bool) -> Vec<u8> {
    if utf8 {
        text.into_bytes()
    } else {
        text.chars().map(|c| c as u32 as u8).collect()
    }
}

/// `body` as a `data:` URI of the media type `mime` (with its charset, if it names one).
fn data_uri(mime: &str, body: &[u8]) -> String {
    let mime: String = mime.chars().filter(|c| c.is_ascii_alphanumeric() || "/+.;=_-".contains(*c)).collect();
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(body))
}

fn is_html(mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    mime.contains("text/html") || mime.contains("xhtml")
}

/// Where each `<name ...>` start tag of the lower-case HTML `lower` is: its `<`, and past its `>`.
fn tags_at(lower: &str, name: &str) -> Vec<(usize, usize)> {
    let open = format!("<{name}");
    let (mut found, mut from) = (Vec::new(), 0);
    while let Some(at) = lower[from..].find(&open).map(|i| from + i) {
        from = at + open.len();
        if !lower[from..].starts_with(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/') {
            continue;
        }
        let Some(gt) = lower[from..].find('>') else { break };
        from += gt + 1;
        found.push((at, from));
    }
    found
}

/// Where the values of the `style` attributes of the HTML `html` (`lower` in lower case) are.
fn style_attrs(html: &str, lower: &str) -> Vec<(usize, usize)> {
    let (mut spans, mut from) = (Vec::new(), 0);
    while let Some(at) = lower[from..].find("style=").map(|i| from + i) {
        from = at + 6;
        // An attribute of a start tag: after a space, and after the tag's `<` with no `>` between.
        let Some(lt) = lower[..at].rfind('<').filter(|&lt| at > 0 && lower.as_bytes()[at - 1].is_ascii_whitespace() && !lower[lt..at].contains('>')) else { continue };
        if let Some((start, end)) = attr_span(&html[lt..], "style").filter(|&(start, _)| lt + start == from || lt + start == from + 1) {
            spans.push((lt + start, lt + end));
            from = lt + end;
        }
    }
    spans
}

/// The name of the tag whose start tag holds the offset `at` of `lower`.
fn tag_name(lower: &str, at: usize) -> &str {
    let start = lower[..at].rfind('<').map_or(at, |lt| lt + 1);
    lower[start..].split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or_default()
}

/// `text` with each (start, end, value) edit made, in order; one inside an earlier one is dropped.
fn splice(text: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    edits.sort_by_key(|&(start, end, _)| (start, std::cmp::Reverse(end)));
    let (mut out, mut at) = (String::with_capacity(text.len()), 0);
    for (start, end, value) in edits {
        if start < at {
            continue;
        }
        out.push_str(&text[at..start]);
        out.push_str(&value);
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// `value` as an HTML attribute's, quoted with `"` or `'`.
fn attribute(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// The saved file's name: the page's title, else its host and path.
fn file_name(html: &str, url: &Url) -> PathBuf {
    let lower = html.to_ascii_lowercase();
    let title = tags_at(&lower, "title").first().and_then(|&(_, start)| Some(crate::folders::unescape(&html[start..start + lower[start..].find("</title")?])));
    let title = title.map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|t| !t.is_empty());
    let name = title.unwrap_or_else(|| format!("{}{}", url.host_str().unwrap_or("page"), url.path().trim_end_matches('/')).replace('/', " "));
    let name = format!("{}.html", crate::ingest::truncate_chars(&name, 120));
    crate::ingest::clean_path([name.as_str()]).unwrap_or_else(|_| PathBuf::from("page.html"))
}

struct Saver<'a> {
    http: &'a reqwest::Client,
    options: &'a SaveOptions,
    /// Each site's robots.txt, by origin.
    robots: HashMap<String, Result<Robots, String>>,
    /// The `data:` URI of each link fetched (none: it stays a link).
    inlined: HashMap<String, Option<String>>,
    fetched: u64,
    warc: Option<crate::warc::Writer>,
    /// The first WARC write that failed.
    broken: Option<String>,
    left: Vec<(String, String)>,
}

impl Saver<'_> {
    /// The answer to a GET of `url` (where it landed, its media type, its body), when robots.txt
    /// allows it and it fits under the size cap.
    async fn fetch(&mut self, url: &Url) -> Result<(Url, String, Vec<u8>), String> {
        let origin = url.origin().ascii_serialization();
        if !self.robots.contains_key(&origin) {
            let robots = robots_of(self.http, url, Duration::ZERO).await;
            self.robots.insert(origin.clone(), robots);
        }
        let gap = match &self.robots[&origin] {
            Ok(robots) if robots.allows(&robots_path(url)) => robots.delay.unwrap_or_default(),
            Ok(_) => return Err("the site's robots.txt asks robots not to fetch it".to_string()),
            Err(e) => return Err(e.clone()),
        };
        let mut answer = crate::polite::get(self.http, url, gap).await?;
        if !answer.status().is_success() {
            return Err(format!("HTTP {}", answer.status().as_u16()));
        }
        let (landed, head, date) = (answer.url().clone(), crate::warc::head_of(&answer), SystemTime::now());
        let mime = answer.headers().get(reqwest::header::CONTENT_TYPE).and_then(|t| t.to_str().ok()).unwrap_or("application/octet-stream").to_string();
        let (cap, left) = (self.options.max_bytes, self.options.max_bytes.saturating_sub(self.fetched));
        let too_big = move || format!("the size cap ({cap} bytes) was reached");
        if answer.content_length().is_some_and(|n| n > left) {
            return Err(too_big());
        }
        let mut body = Vec::new();
        while let Some(chunk) = answer.chunk().await.map_err(|e| format!("Cannot reach {}: {}", url.host_str().unwrap_or_default(), e.without_url()))? {
            body.extend_from_slice(&chunk);
            if body.len() as u64 > left {
                return Err(too_big());
            }
        }
        self.fetched += body.len() as u64;
        if let Some(warc) = &mut self.warc {
            if let Err(e) = warc.response(&landed, date, &head, &mut std::io::Cursor::new(&body)) {
                self.broken.get_or_insert(e);
            }
        }
        Ok((landed, mime, body))
    }

    /// `url` as a `data:` URI (a style sheet with what it uses put in too), or none to leave it a link.
    async fn inline(&mut self, url: &Url, imports: usize) -> Option<String> {
        if let Some(done) = self.inlined.get(url.as_str()) {
            return done.clone();
        }
        let uri = match self.fetch(url).await {
            Ok((landed, mime, body)) => {
                let body = match mime.to_ascii_lowercase().starts_with("text/css") && imports < MAX_IMPORTS {
                    true => {
                        let (text, utf8) = text_of(&body);
                        bytes_of(Box::pin(self.css(&text, &landed, imports + 1)).await, utf8)
                    }
                    false => body,
                };
                Some(data_uri(&mime, &body))
            }
            Err(why) => {
                self.left.push((url.to_string(), why));
                None
            }
        };
        self.inlined.insert(url.to_string(), uri.clone());
        uri
    }

    /// The style sheet `css` (from `base`) with what it uses put in.
    async fn css(&mut self, css: &str, base: &Url, imports: usize) -> String {
        let links = css_links(css, base);
        let mut found = HashMap::new();
        for link in &links {
            if let Some(uri) = self.inline(&link.url, imports).await {
                found.insert(link.url.to_string(), uri);
            }
        }
        rewrite(css, &links, false, |link| found.get(link.url.as_str()).cloned())
    }

    /// The page at `url` for an iframe (`frames` deep), as a `data:` URI; none leaves it a link.
    async fn frame(&mut self, url: &Url, frames: usize) -> Option<String> {
        match self.fetch(url).await {
            Ok((landed, mime, body)) if is_html(&mime) => {
                let (text, utf8) = text_of(&body);
                let html = Box::pin(self.page(&text, &landed, frames)).await;
                Some(data_uri(&mime, &bytes_of(html, utf8)))
            }
            Ok((_, mime, body)) => Some(data_uri(&mime, &body)),
            Err(why) => {
                self.left.push((url.to_string(), why));
                None
            }
        }
    }

    /// The HTML `html` (from `base`, `frames` iframes deep) saved as the module says.
    async fn page(&mut self, html: &str, base: &Url, frames: usize) -> String {
        let lower = html.to_ascii_lowercase();
        let base = page_base(html, &lower, base);
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        for (start, end) in tags_at(&lower, "base") {
            edits.push((start, end, String::new()));
        }
        // What looks like a link in a script is text of the script: a script left out goes whole,
        // the text of one kept stays as it is.
        let mut untouched = Vec::new();
        for (start, end) in tags_at(&lower, "script") {
            let close = lower[end..].find("</script").map_or(end, |at| end + at);
            let after = lower[close..].find('>').map_or(close, |gt| close + gt + 1);
            if self.options.keep_scripts {
                untouched.push((end, close));
            } else {
                edits.push((start, after, String::new()));
                untouched.push((start, after));
            }
        }
        for name in ["img", "source"] {
            for (start, end) in tags_at(&lower, name) {
                if let Some((from, to)) = attr_span(&html[start..end], "srcset") {
                    edits.push((start + from, start + to, String::new()));
                }
            }
        }
        for link in html_links(html, &base) {
            if untouched.iter().any(|&(start, end)| start <= link.span.0 && link.span.1 <= end) {
                continue;
            }
            let value = match (link.page, tag_name(&lower, link.span.0)) {
                (true, "iframe" | "frame") if frames < MAX_FRAMES => self.frame(&link.url, frames + 1).await,
                (true, _) => None,
                (false, _) => self.inline(&link.url, 0).await,
            };
            edits.push((link.span.0, link.span.1, attribute(&value.unwrap_or_else(|| link.url.to_string()))));
        }
        for (_, start) in tags_at(&lower, "style") {
            let end = lower[start..].find("</style").map_or(lower.len(), |len| start + len);
            let css = self.css(&html[start..end], &base, 0).await;
            edits.push((start, end, css));
        }
        for (start, end) in style_attrs(html, &lower) {
            let css = self.css(&crate::folders::unescape(&html[start..end]), &base, 0).await;
            edits.push((start, end, attribute(&css)));
        }
        splice(html, edits)
    }
}

/// Saves the page at `url` into the folder `dir` as one HTML file (see the module), named after its
/// title. `Err` when the page cannot be fetched or is no page, or the file cannot be written.
pub async fn save_page(http: &reqwest::Client, url: &Url, dir: &Path, options: &SaveOptions) -> Result<Saved, String> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("{url} is not a web page's link"));
    }
    let mut url = url.clone();
    url.set_fragment(None);
    let warc = options.warc.as_deref().map(crate::warc::Writer::open).transpose()?;
    let mut saver = Saver { http, options, robots: HashMap::new(), inlined: HashMap::new(), fetched: 0, warc, broken: None, left: Vec::new() };
    let (landed, mime, body) = saver.fetch(&url).await?;
    if !is_html(&mime) {
        return Err(format!("{url} is not a web page (it is {mime})"));
    }
    let (text, utf8) = text_of(&body);
    // A byte order mark stays first.
    let (bom, text) = match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest.to_string()),
        None => ("", text),
    };
    let html = saver.page(&text, &landed, 0).await;
    let (when, _) = crate::warc::dates(SystemTime::now());
    let comment = format!(
        "<!-- Saved by Endo's Unified Downloader from {} on {when}; SHA-256 of the page as served: {:x} -->\n",
        landed.as_str().replace("--", "%2D%2D"),
        Sha256::digest(&body)
    );
    let mut out = format!("{bom}{comment}").into_bytes();
    out.extend(bytes_of(html, utf8));

    let file = dir.join(file_name(&text, &landed));
    let part = file.with_extension("html.part");
    std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&part, &out))
        .and_then(|()| std::fs::rename(&part, &file))
        .map_err(|e| format!("Cannot write {}: {e}", file.display()))?;
    if let Some(e) = saver.broken {
        return Err(e);
    }
    if let Some(warc) = saver.warc.take() {
        warc.finish()?;
    }
    let inlined = saver.inlined.values().filter(|uri| uri.is_some()).count();
    Ok(Saved { file, bytes: out.len() as u64, inlined, left: saver.left })
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::hls::tests::{serve, Reply};

    const PAGE: &str = r#"<!doctype html><html><head><title>A &amp; B</title><base href="/p/"><link rel="stylesheet" href="s.css">
<style>h1 { background: url('bg.png') }</style><script src="app.js"></script><script>document.write("<img src=never.png>")</script></head>
<body><h1 style="background: url(&quot;i.png&quot;)">Hi</h1><img src="i.png" srcset="i.png 1x, big.png 2x"><img src="/private/x.png">
<a href="other.html?a=1&amp;b=2">next</a><iframe src="frame.html"></iframe></body></html>"#;

    fn site(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        let typed = |kind: &str, body: &[u8]| (200, format!("Content-Type: {kind}\r\n"), body.to_vec());
        match path {
            "/robots.txt" => (200, String::new(), b"User-agent: *\nDisallow: /private/\n".to_vec()),
            "/page" => typed("text/html; charset=utf-8", PAGE.as_bytes()),
            "/latin" => typed("text/html; charset=iso-8859-1", b"<title>caf\xe9</title><p>\xe9t\xe9</p><img src=p/i.png>"),
            "/p/s.css" => typed("text/css", b"@import 'more.css'; body { background: url(bg.png) }"),
            "/p/more.css" => typed("text/css", b"p { font: url(/f.woff2) }"),
            "/p/bg.png" => typed("image/png", b"BG"),
            "/p/i.png" => typed("image/png", b"IMG"),
            "/f.woff2" => typed("font/woff2", b"FONT"),
            "/p/frame.html" => typed("text/html", b"<img src=i.png>"),
            "/p/app.js" => typed("text/javascript", b"alert(1)"),
            "/private/x.png" => typed("image/png", b"NO"),
            _ => (404, String::new(), Vec::new()),
        }
    }

    fn b64(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    /// Every part of the data: URI `uri` after `base64,` decoded.
    fn decoded(html: &str, after: &str) -> String {
        let start = html.find(after).unwrap_or_else(|| panic!("{after} in {html}")) + after.len();
        let end = start + html[start..].find(|c: char| !(c.is_ascii_alphanumeric() || "+/=".contains(c))).unwrap_or(html.len() - start);
        String::from_utf8(base64::engine::general_purpose::STANDARD.decode(&html[start..end]).unwrap()).unwrap()
    }

    /// What the page uses is put in (style sheets with their imports and images, `<style>` and
    /// `style` attributes, an iframe), its scripts, `<base>` and `srcset` left out, its links made
    /// full; robots.txt keeps a file out; a WARC gets each answer.
    #[tokio::test]
    async fn a_page_is_saved_as_one_file_that_needs_nothing_else() {
        let (addr, hits) = serve(site).await;
        let dir = tempfile::tempdir().unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let warc = dir.path().join("page.warc.gz");
        let options = SaveOptions { warc: Some(warc.clone()), ..SaveOptions::default() };
        let saved = save_page(&http, &Url::parse(&format!("http://{addr}/page#top")).unwrap(), dir.path(), &options).await.unwrap();
        assert_eq!(saved.file, dir.path().join("A & B.html"));
        let html = std::fs::read_to_string(&saved.file).unwrap();
        let hash = format!("{:x}", Sha256::digest(PAGE.as_bytes()));
        assert!(html.starts_with(&format!("<!-- Saved by Endo's Unified Downloader from http://{addr}/page on ")) && html.contains(&format!("SHA-256 of the page as served: {hash} -->\n<!doctype html>")), "{html}");
        assert!(!html.contains("<script") && !html.contains("<base") && html.contains(r#"srcset="""#), "{html}");
        let img = format!("data:image/png;base64,{}", b64("IMG"));
        assert!(html.contains(&format!(r#"<img src="{img}""#)) && html.contains(&format!("url(&quot;{img}&quot;)")), "{html}");
        assert!(html.contains(&format!("h1 {{ background: url('data:image/png;base64,{}') }}", b64("BG"))), "{html}");
        assert!(html.contains(&format!(r#"<img src="http://{addr}/private/x.png">"#)) && html.contains(&format!(r#"<a href="http://{addr}/p/other.html?a=1&amp;b=2">"#)), "{html}");
        let css = decoded(&html, r#"<link rel="stylesheet" href="data:text/css;base64,"#);
        assert!(css.starts_with("@import 'data:text/css;base64,") && css.ends_with(&format!("body {{ background: url(data:image/png;base64,{}) }}", b64("BG"))), "{css}");
        assert_eq!(decoded(&css, "@import 'data:text/css;base64,"), format!("p {{ font: url(data:font/woff2;base64,{}) }}", b64("FONT")));
        assert_eq!(decoded(&html, r#"<iframe src="data:text/html;base64,"#), format!("<img src={img}>"));
        assert_eq!(saved.inlined, 5, "s.css, more.css, the font, bg.png and i.png: each fetched once");
        assert_eq!(saved.left.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(), [format!("http://{addr}/private/x.png")]);
        let hits = hits.lock().clone();
        assert!(["/p/app.js", "/never.png", "/p/never.png", "/private/x.png", "/p/big.png"].iter().all(|p| !hits.contains_key(*p)), "{hits:?}");
        assert_eq!((hits["/robots.txt"], hits["/p/i.png"], hits["/p/bg.png"]), (1, 1, 1));

        let mut all = Vec::new();
        flate2::read::MultiGzDecoder::new(std::fs::File::open(&warc).unwrap()).read_to_end(&mut all).unwrap();
        let records = crate::warc::tests::records(&all);
        let page_url = format!("http://{addr}/page");
        let page = records.iter().find(|r| r.field("WARC-Type") == Some("response") && r.field("WARC-Target-URI") == Some(page_url.as_str())).unwrap();
        assert!(page.block.ends_with(PAGE.as_bytes()), "the page as served");
        assert_eq!(records.iter().filter(|r| r.field("WARC-Type") == Some("response")).count(), 7, "the page, what it uses and the frame");

        // Kept, scripts are put in too.
        let kept = SaveOptions { keep_scripts: true, ..SaveOptions::default() };
        let saved = save_page(&http, &Url::parse(&format!("http://{addr}/page")).unwrap(), dir.path(), &kept).await.unwrap();
        let html = std::fs::read_to_string(&saved.file).unwrap();
        assert!(html.contains(&format!(r#"<script src="data:text/javascript;base64,{}"></script>"#, b64("alert(1)"))) && html.contains("document.write"), "{html}");
    }

    /// A page not in UTF-8 keeps its bytes; the size cap leaves what passes it a link; a link that
    /// is no page is an error.
    #[tokio::test]
    async fn encodings_are_kept_and_the_size_cap_holds() {
        let (addr, _) = serve(site).await;
        let dir = tempfile::tempdir().unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let saved = save_page(&http, &Url::parse(&format!("http://{addr}/latin")).unwrap(), dir.path(), &SaveOptions::default()).await.unwrap();
        let bytes = std::fs::read(&saved.file).unwrap();
        let body = &bytes[bytes.iter().position(|&b| b == b'\n').unwrap() + 1..];
        assert_eq!(body, format!("<title>caf\u{e9}</title><p>\u{e9}t\u{e9}</p><img src=data:image/png;base64,{}>", b64("IMG")).chars().map(|c| c as u8).collect::<Vec<_>>());

        let small = SaveOptions { max_bytes: PAGE.len() as u64 + 1, ..SaveOptions::default() };
        let saved = save_page(&http, &Url::parse(&format!("http://{addr}/page")).unwrap(), dir.path(), &small).await.unwrap();
        assert_eq!(saved.inlined, 0);
        assert!(saved.left.iter().any(|(_, why)| why.contains("size cap")), "{:?}", saved.left);
        let refused = save_page(&http, &Url::parse(&format!("http://{addr}/p/i.png")).unwrap(), dir.path(), &SaveOptions::default()).await.unwrap_err();
        assert!(refused.contains("is not a web page"), "{refused}");
    }
}
