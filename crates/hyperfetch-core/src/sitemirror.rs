//! Copies a site to disk to browse offline (the CLI's `--mirror`): the pages under the link's
//! folder on its host, as many links deep as asked, and what they need (style sheets, scripts,
//! images) when asked, from that host only. Once the crawl ends, every saved page's links to a
//! saved file are rewritten relative to it, and its other links to their full URLs. No script
//! runs: a page that builds its content with one is saved as it came.
//!
//! It is polite: it honours the site's robots.txt (its rules for this app, else for every robot,
//! and the Crawl-delay it asks for), makes one request at a time with a delay between them, under
//! the host's shared rest (see `crate::polite`), and stops at a number of pages and a total size.
//! A manifest (`mirror-manifest.json`: each file's URL, path, size and SHA-256) is kept as it goes:
//! a run stopped early goes on where it stopped when started again, reading the pages it saved
//! from disk instead of asking for them again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::ingest::clean_path;
use crate::polite;

/// The manifest's name, in the folder the site is saved to.
pub const MANIFEST: &str = "mirror-manifest.json";
/// Largest page (HTML or CSS) read for its links.
const MAX_PAGE: u64 = 8 << 20;

/// How a site is mirrored.
#[derive(Debug, Clone)]
pub struct MirrorOptions {
    /// How many links away from the first page a page may be (0: that page alone).
    pub depth: usize,
    /// Also the style sheets, scripts, images and media the pages use.
    pub requisites: bool,
    /// Least time between two requests (a longer Crawl-delay in robots.txt wins).
    pub delay: Duration,
    /// Most pages saved; most bytes saved in all.
    pub max_pages: usize,
    pub max_bytes: u64,
    /// A WARC file each answer is also written to, as received (see `crate::warc`).
    pub warc: Option<PathBuf>,
}

impl Default for MirrorOptions {
    fn default() -> Self {
        Self { depth: 2, requisites: false, delay: Duration::from_secs(1), max_pages: 500, max_bytes: 1 << 30, warc: None }
    }
}

/// What a mirror did.
#[derive(Debug, Default)]
pub struct Mirrored {
    /// Pages and other files saved (those of an earlier run read again included), and their bytes.
    pub pages: usize,
    pub files: usize,
    pub bytes: u64,
    /// Links robots.txt keeps the app from.
    pub disallowed: usize,
    /// Links that could not be saved, and why.
    pub failed: Vec<(String, String)>,
    /// Why the crawl stopped before it saw everything it may (a cap), if it did.
    pub stopped: Option<String>,
}

/// One file of the manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Entry {
    url: String,
    /// Relative to the mirror's folder, with `/`.
    file: String,
    size: u64,
    sha256: String,
    /// Whether it is a page (HTML), read for its links.
    page: bool,
    /// Its links were rewritten: it no longer holds the site's own, so a later run asks for it again.
    rewritten: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    start: String,
    files: Vec<Entry>,
}

/// The rules robots.txt gives this app: (allow, path pattern) pairs, and its Crawl-delay.
#[derive(Debug, Default)]
pub(crate) struct Robots {
    rules: Vec<(bool, String)>,
    pub(crate) delay: Option<Duration>,
}

/// The robots.txt rules of `site`'s host (none when it has none). `Err` when the host cannot be
/// reached, or fails to answer for it.
pub(crate) async fn robots_of(http: &reqwest::Client, site: &Url, delay: Duration) -> Result<Robots, String> {
    let robots_url = site.join("/robots.txt").map_err(|e| e.to_string())?;
    match polite::get(http, &robots_url, delay).await {
        Ok(answer) if answer.status().is_success() => Ok(Robots::parse(&answer.text().await.unwrap_or_default(), crate::resolver::APP_USER_AGENT)),
        // No robots.txt: nothing is forbidden.
        Ok(answer) if answer.status().is_client_error() => Ok(Robots::default()),
        Ok(answer) => Err(format!("{} answered HTTP {} for its robots.txt: try again later", site.host_str().unwrap_or_default(), answer.status().as_u16())),
        Err(e) => Err(e),
    }
}

/// `url`'s path with its query, as robots.txt rules match it.
pub(crate) fn robots_path(url: &Url) -> String {
    match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    }
}

impl Robots {
    /// The rules of `text` for the agent `token` (this app's name), else for every robot (`*`).
    fn parse(text: &str, token: &str) -> Self {
        // Groups: their agents, rules and delay.
        let mut groups: Vec<(Vec<String>, Vec<(bool, String)>, Option<Duration>)> = Vec::new();
        let mut open = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            let Some((key, value)) = line.split_once(':') else { continue };
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            match key.as_str() {
                "user-agent" => {
                    if !open {
                        groups.push((Vec::new(), Vec::new(), None));
                        open = true;
                    }
                    if let Some(group) = groups.last_mut() {
                        group.0.push(value.to_ascii_lowercase());
                    }
                }
                "allow" | "disallow" => {
                    open = false;
                    // "Disallow:" with nothing allows everything.
                    if let Some(group) = groups.last_mut().filter(|_| !value.is_empty()) {
                        group.1.push((key == "allow", value.to_string()));
                    }
                }
                "crawl-delay" => {
                    open = false;
                    if let Some(group) = groups.last_mut() {
                        group.2 = value.parse::<f64>().ok().filter(|s| s.is_finite() && *s >= 0.0).map(|s| Duration::from_secs_f64(s.min(60.0)));
                    }
                }
                _ => {}
            }
        }
        let token = token.to_ascii_lowercase();
        let ours = groups.iter().find(|(agents, _, _)| agents.iter().any(|a| a != "*" && token.contains(a.as_str())));
        let group = ours.or_else(|| groups.iter().find(|(agents, _, _)| agents.iter().any(|a| a == "*")));
        group.map_or_else(Robots::default, |(_, rules, delay)| Robots { rules: rules.clone(), delay: *delay })
    }

    /// Whether `path` (with its query) may be fetched: the longest rule that matches it decides,
    /// Allow on a tie; none allows.
    pub(crate) fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, pattern) in &self.rules {
            if robots_match(pattern, path) {
                let better = best.is_none_or(|(len, was)| pattern.len() > len || (pattern.len() == len && *allow && !was));
                if better {
                    best = Some((pattern.len(), *allow));
                }
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

/// Whether a robots.txt `pattern` (`*` for any run of characters, `$` at the end for the path's
/// end) matches the start of `path`.
fn robots_match(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let parts: Vec<&str> = pattern.split('*').collect();
    if !path.starts_with(parts[0]) {
        return false;
    }
    let mut at = parts[0].len();
    for (i, part) in parts.iter().enumerate().skip(1) {
        let last = i == parts.len() - 1;
        if last && anchored {
            return path.len() >= at + part.len() && path.ends_with(part);
        }
        match path[at..].find(part) {
            Some(found) => at += found + part.len(),
            None => return false,
        }
    }
    !anchored || at == path.len()
}

/// Where the file of `url` goes, relative to the mirror's folder: `host/path`, `index.html` for a
/// folder, its query in its name, `.html` added to a page whose name does not say it is one.
fn local_path(url: &Url, page: bool) -> Option<PathBuf> {
    let mut parts: Vec<String> = url.path_segments()?.map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let mut name = parts.pop().filter(|n| !n.is_empty()).unwrap_or_else(|| "index.html".to_string());
    if let Some(query) = url.query() {
        name = format!("{name}?{query}");
    }
    let lower = name.to_ascii_lowercase();
    if page && !(lower.ends_with(".html") || lower.ends_with(".htm")) {
        name.push_str(".html");
    }
    parts.retain(|p| !p.is_empty());
    let host = match url.port() {
        Some(port) => format!("{}_{port}", url.host_str()?),
        None => url.host_str()?.to_string(),
    };
    let name = crate::ingest::truncate_chars(&name, 150);
    clean_path(std::iter::once(host.as_str()).chain(parts.iter().map(String::as_str)).chain([name.as_str()])).ok()
}

/// `target` relative to the folder of `from` (both relative to the mirror's folder), with `/`.
fn relative(from: &Path, target: &Path) -> String {
    let from: Vec<Component> = from.parent().map_or_else(Vec::new, |p| p.components().collect());
    let to: Vec<Component> = target.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), from.len() - common).collect();
    parts.extend(to[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    parts.iter().map(|p| percent_encoding::utf8_percent_encode(p, LINK_PART).to_string()).collect::<Vec<_>>().join("/")
}

/// What a part of a rewritten link must have encoded.
const LINK_PART: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'?').add(b'<').add(b'>').add(b'\'');

/// A link of a page: where its value is in the page's text, the URL it names, and whether it is a
/// page of its own (else something the page uses).
pub(crate) struct Link {
    pub(crate) span: (usize, usize),
    pub(crate) url: Url,
    pub(crate) page: bool,
}

/// Tags whose attribute names a page, and those whose attribute names something a page uses.
const PAGE_LINKS: &[(&str, &str)] = &[("a", "href"), ("area", "href"), ("iframe", "src"), ("frame", "src")];
const USED_LINKS: &[(&str, &str)] = &[("img", "src"), ("script", "src"), ("source", "src"), ("audio", "src"), ("video", "src"), ("video", "poster"), ("embed", "src"), ("input", "src"), ("track", "src")];

/// Where the value of `attr` is in the start tag `tag` (its source, starting at `<`).
pub(crate) fn attr_span(tag: &str, attr: &str) -> Option<(usize, usize)> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{attr}=");
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let start = from + found;
        from = start + needle.len();
        if start == 0 || !lower.as_bytes()[start - 1].is_ascii_whitespace() {
            continue;
        }
        let rest = &tag[from..];
        return match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].find(q).map(|len| (from + 1, from + 1 + len)),
            _ => Some((from, from + rest.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(rest.len()))),
        };
    }
    None
}

/// What the links of the HTML `html` (`lower` in lower case), fetched from `base`, are relative to:
/// its `<base href>`, else `base`.
pub(crate) fn page_base(html: &str, lower: &str, base: &Url) -> Url {
    crate::resolver::start_tags(html, lower, "base")
        .first()
        .and_then(|tag| crate::resolver::attr_value(tag, "href"))
        .and_then(|href| base.join(&href).ok())
        .unwrap_or_else(|| base.clone())
}

/// The links of the HTML `html`, resolved against `base` (or the page's `<base href>`).
pub(crate) fn html_links(html: &str, base: &Url) -> Vec<Link> {
    let lower = html.to_ascii_lowercase();
    let base = page_base(html, &lower, base);
    let mut links = Vec::new();
    let mut add = |tag: &str, attr: &str, page: bool| {
        let offset = tag.as_ptr() as usize - html.as_ptr() as usize;
        let Some((start, end)) = attr_span(tag, attr) else { return };
        let value = crate::folders::unescape(tag[start..end].trim());
        if let Ok(mut url) = base.join(&value) {
            url.set_fragment(None);
            if matches!(url.scheme(), "http" | "https") {
                links.push(Link { span: (offset + start, offset + end), url, page });
            }
        }
    };
    for (name, attr) in PAGE_LINKS {
        for tag in crate::resolver::start_tags(html, &lower, name) {
            add(tag, attr, true);
        }
    }
    for (name, attr) in USED_LINKS {
        for tag in crate::resolver::start_tags(html, &lower, name) {
            add(tag, attr, false);
        }
    }
    for tag in crate::resolver::start_tags(html, &lower, "link") {
        let rel = crate::resolver::attr_value(tag, "rel").unwrap_or_default().to_ascii_lowercase();
        if rel.split_whitespace().any(|r| matches!(r, "stylesheet" | "icon" | "preload" | "apple-touch-icon")) {
            add(tag, "href", false);
        }
    }
    links.sort_by_key(|l| l.span.0);
    links
}

/// The links of the style sheet `css` (`url(...)` and `@import "..."`), resolved against `base`.
pub(crate) fn css_links(css: &str, base: &Url) -> Vec<Link> {
    let mut links = Vec::new();
    let mut push = |start: usize, end: usize| {
        let value = css[start..end].trim();
        if value.is_empty() || value.starts_with("data:") {
            return;
        }
        if let Ok(mut url) = base.join(value) {
            url.set_fragment(None);
            if matches!(url.scheme(), "http" | "https") {
                links.push(Link { span: (start, end), url, page: false });
            }
        }
    };
    let lower = css.to_ascii_lowercase();
    let mut at = 0;
    while let Some(found) = lower[at..].find("url(") {
        let start = at + found + 4;
        let Some(len) = css[start..].find(')') else { break };
        let inner = &css[start..start + len];
        let trimmed = inner.trim();
        let quoted = trimmed.starts_with(['"', '\'']);
        let lead = inner.len() - inner.trim_start().len() + usize::from(quoted);
        let value_len = trimmed.len() - if quoted { 2.min(trimmed.len()) } else { 0 };
        push(start + lead, start + lead + value_len);
        at = start + len;
    }
    let mut at = 0;
    while let Some(found) = lower[at..].find("@import") {
        let start = at + found + 7;
        let rest = &css[start..];
        let skip = rest.len() - rest.trim_start().len();
        if let Some(q @ ('"' | '\'')) = rest[skip..].chars().next() {
            let from = start + skip + 1;
            if let Some(len) = css[from..].find(q) {
                push(from, from + len);
            }
        }
        at = start;
    }
    links.sort_by_key(|l| l.span.0);
    links
}

/// `text` with each link's value replaced by what `to` gives for it (none: kept), HTML-escaped
/// when `html`.
pub(crate) fn rewrite(text: &str, links: &[Link], html: bool, mut to: impl FnMut(&Link) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for link in links {
        if link.span.0 < at {
            continue;
        }
        let Some(value) = to(link) else { continue };
        out.push_str(&text[at..link.span.0]);
        if html {
            out.push_str(&value.replace('&', "&amp;").replace('"', "&quot;"));
        } else {
            out.push_str(&value);
        }
        at = link.span.1;
    }
    out.push_str(&text[at..]);
    out
}

/// A crawl in progress.
struct Crawl<'a> {
    http: &'a reqwest::Client,
    start: Url,
    /// Pages are under this path of the start's host.
    scope: String,
    dir: &'a Path,
    options: &'a MirrorOptions,
    robots: Robots,
    gap: Duration,
    manifest: Manifest,
    /// The manifest's entries by URL (the URL a request was redirected to included).
    known: HashMap<String, usize>,
    done: Mirrored,
    warc: Option<crate::warc::Writer>,
}

impl Crawl<'_> {
    fn in_scope(&self, url: &Url, page: bool) -> bool {
        url.origin() == self.start.origin() && (!page || url.path().starts_with(&self.scope))
    }

    fn save_manifest(&self) -> Result<(), String> {
        let text = serde_json::to_vec_pretty(&self.manifest).map_err(|e| e.to_string())?;
        let file = self.dir.join(MANIFEST);
        let part = self.dir.join(format!("{MANIFEST}.part"));
        std::fs::write(&part, text).and_then(|_| std::fs::rename(&part, &file)).map_err(|e| format!("Cannot write {}: {e}", file.display()))
    }

    fn record(&mut self, entry: Entry, also: Option<&Url>) {
        let at = match self.known.get(&entry.url) {
            Some(&at) => {
                self.manifest.files[at] = entry;
                at
            }
            None => {
                self.manifest.files.push(entry);
                self.manifest.files.len() - 1
            }
        };
        self.known.insert(self.manifest.files[at].url.clone(), at);
        if let Some(also) = also {
            self.known.entry(also.to_string()).or_insert(at);
        }
    }

    /// The text of the saved page `entry` read again, when it is the site's own (not rewritten).
    fn saved_text(&self, entry: &Entry) -> Option<String> {
        if entry.rewritten {
            return None;
        }
        let bytes = std::fs::read(self.dir.join(&entry.file)).ok()?;
        (bytes.len() as u64 == entry.size).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Fetches `url` and saves it; its text when it is a page or a style sheet to read for links,
    /// with the URL it came from (after redirects). None when it was not saved (told in `done`).
    async fn fetch(&mut self, url: &Url, page: bool) -> Result<Option<(String, Url, bool)>, String> {
        let answer = match polite::get(self.http, url, self.gap).await {
            Ok(answer) => answer,
            Err(e) => {
                self.done.failed.push((url.to_string(), e));
                return Ok(None);
            }
        };
        let landed = answer.url().clone();
        if !answer.status().is_success() {
            self.done.failed.push((url.to_string(), format!("HTTP {}", answer.status().as_u16())));
            return Ok(None);
        }
        if !self.in_scope(&landed, false) {
            self.done.failed.push((url.to_string(), format!("leads to another site ({landed})")));
            return Ok(None);
        }
        let kind = answer.headers().get(reqwest::header::CONTENT_TYPE).and_then(|t| t.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
        let is_page = kind.contains("text/html") || kind.contains("xhtml");
        let is_css = kind.contains("text/css");
        let left = self.options.max_bytes.saturating_sub(self.done.bytes);
        if answer.content_length().is_some_and(|n| n > left) {
            self.done.stopped = Some(format!("the total size cap ({} bytes) was reached", self.options.max_bytes));
            return Ok(None);
        }
        let Some(file) = local_path(url, is_page) else {
            self.done.failed.push((url.to_string(), "no usable file name".to_string()));
            return Ok(None);
        };
        let target = self.dir.join(&file);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
        }
        let part = target.with_extension(format!("{}part", target.extension().map_or(String::new(), |e| format!("{}.", e.to_string_lossy()))));
        let (head, date) = (crate::warc::head_of(&answer), std::time::SystemTime::now());
        let (mut written, mut hash, mut kept) = (0u64, Sha256::new(), Vec::new());
        let keep = (is_page && page) || (is_css && self.options.requisites);
        let mut out = tokio::fs::File::create(&part).await.map_err(|e| format!("Cannot write {}: {e}", part.display()))?;
        let mut answer = answer;
        let read = loop {
            match answer.chunk().await {
                Ok(Some(chunk)) => {
                    written += chunk.len() as u64;
                    if written > left {
                        break Err(None);
                    }
                    hash.update(&chunk);
                    if keep && written <= MAX_PAGE {
                        kept.extend_from_slice(&chunk);
                    }
                    if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut out, &chunk).await {
                        break Err(Some(format!("Cannot write {}: {e}", part.display())));
                    }
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err(Some(format!("Cannot reach {}: {e}", url.host_str().unwrap_or_default()))),
            }
        };
        drop(out);
        match read {
            Ok(()) => tokio::fs::rename(&part, &target).await.map_err(|e| format!("Cannot write {}: {e}", target.display()))?,
            Err(why) => {
                let _ = tokio::fs::remove_file(&part).await;
                match why {
                    None => self.done.stopped = Some(format!("the total size cap ({} bytes) was reached", self.options.max_bytes)),
                    Some(e) if e.starts_with("Cannot write") => return Err(e),
                    Some(e) => self.done.failed.push((url.to_string(), e)),
                }
                return Ok(None);
            }
        }
        if let Some(warc) = &mut self.warc {
            let mut body = std::fs::File::open(&target).map_err(|e| format!("Cannot read {}: {e}", target.display()))?;
            warc.response(&landed, date, &head, &mut body)?;
        }
        self.done.bytes += written;
        let counted_page = is_page && page;
        if counted_page {
            self.done.pages += 1;
        } else {
            self.done.files += 1;
        }
        let entry = Entry {
            url: url.to_string(),
            file: file.to_string_lossy().replace('\\', "/"),
            size: written,
            sha256: format!("{:x}", hash.finalize()),
            page: counted_page,
            rewritten: false,
        };
        self.record(entry, Some(&landed).filter(|l| *l != url));
        self.save_manifest()?;
        Ok((keep && written <= MAX_PAGE).then(|| (String::from_utf8_lossy(&kept).into_owned(), landed, is_page)))
    }

    /// The crawl, breadth first from the start, then the links of every saved page rewritten.
    async fn run(&mut self, mut tell: impl FnMut(&str)) -> Result<(), String> {
        let mut queue: VecDeque<(Url, usize, bool)> = VecDeque::from([(self.start.clone(), 0, true)]);
        let mut seen: HashSet<String> = HashSet::from([self.start.to_string()]);
        while let Some((url, depth, page)) = queue.pop_front() {
            if self.done.stopped.is_some() {
                break;
            }
            if page && self.done.pages >= self.options.max_pages {
                self.done.stopped = Some(format!("the page cap ({}) was reached", self.options.max_pages));
                break;
            }
            if !self.robots.allows(&robots_path(&url)) {
                self.done.disallowed += 1;
                continue;
            }
            // A file an earlier run saved is not asked for again, and a page or style sheet is read
            // from disk; one whose links were rewritten is asked for again, to find them.
            let earlier = self.known.get(url.as_str()).map(|&at| self.manifest.files[at].clone()).filter(|e| self.dir.join(&e.file).is_file());
            let text = match earlier {
                Some(entry) if !entry.rewritten => {
                    if entry.page {
                        self.done.pages += 1;
                    } else {
                        self.done.files += 1;
                    }
                    self.done.bytes += entry.size;
                    let reads = entry.page || (self.options.requisites && entry.file.to_ascii_lowercase().ends_with(".css"));
                    reads.then(|| self.saved_text(&entry)).flatten().map(|text| (text, url.clone(), entry.page))
                }
                _ => {
                    tell(&url.to_string());
                    self.fetch(&url, page).await?
                }
            };
            let Some((text, landed, is_page)) = text else { continue };
            let links = if is_page { html_links(&text, &landed) } else { css_links(&text, &landed) };
            for link in links {
                let (next, child) = if link.page && is_page { (depth + 1, true) } else { (depth, false) };
                let wanted = if child { next <= self.options.depth } else { self.options.requisites };
                if wanted && self.in_scope(&link.url, child) && seen.insert(link.url.to_string()) {
                    queue.push_back((link.url, next, child));
                }
            }
        }
        self.rewrite_all()
    }

    /// Rewrites the links of every saved page and style sheet: to a saved file, relative to it;
    /// any other to its full URL.
    fn rewrite_all(&mut self) -> Result<(), String> {
        let files: HashMap<String, PathBuf> = self.known.iter().map(|(url, &at)| (url.clone(), PathBuf::from(&self.manifest.files[at].file))).collect();
        for at in 0..self.manifest.files.len() {
            let entry = self.manifest.files[at].clone();
            let css = entry.file.to_ascii_lowercase().ends_with(".css");
            if entry.rewritten || !(entry.page || css) {
                continue;
            }
            let Some(text) = self.saved_text(&entry) else { continue };
            let Ok(base) = Url::parse(&entry.url) else { continue };
            let links = if entry.page { html_links(&text, &base) } else { css_links(&text, &base) };
            let own = PathBuf::from(&entry.file);
            let rewritten = rewrite(&text, &links, entry.page, |link| {
                Some(match files.get(link.url.as_str()) {
                    Some(target) => relative(&own, target),
                    None => link.url.to_string(),
                })
            });
            let target = self.dir.join(&entry.file);
            std::fs::write(&target, rewritten.as_bytes()).map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
            let file = &mut self.manifest.files[at];
            file.size = rewritten.len() as u64;
            file.sha256 = format!("{:x}", Sha256::digest(rewritten.as_bytes()));
            file.rewritten = true;
        }
        self.save_manifest()
    }
}

/// Mirrors the site at `start` into `dir` as `options` say (see the module), calling `tell` with
/// each URL it asks for. `Err` when the site cannot be mirrored at all (robots.txt forbids it, it
/// cannot be reached) or the folder cannot be written.
pub async fn mirror(http: &reqwest::Client, start: &Url, dir: &Path, options: &MirrorOptions, tell: impl FnMut(&str)) -> Result<Mirrored, String> {
    if !matches!(start.scheme(), "http" | "https") || start.host_str().is_none() {
        return Err(format!("{start} is not a web site's link"));
    }
    let mut start = start.clone();
    start.set_fragment(None);
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let robots = robots_of(http, &start, options.delay).await?;
    if !robots.allows(&robots_path(&start)) {
        return Err(format!("the site's robots.txt asks robots not to fetch {start}"));
    }
    let gap = robots.delay.map_or(options.delay, |asked| asked.max(options.delay));
    // A manifest of this start: what an earlier run saved.
    let manifest = std::fs::read(dir.join(MANIFEST)).ok().and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok()).filter(|m| m.start == start.as_str());
    let manifest = manifest.unwrap_or_else(|| Manifest { start: start.to_string(), files: Vec::new() });
    let known = manifest.files.iter().enumerate().map(|(at, e)| (e.url.clone(), at)).collect();
    let scope = start.path()[..start.path().rfind('/').map_or(1, |at| at + 1)].to_string();
    let warc = options.warc.as_deref().map(crate::warc::Writer::open).transpose()?;
    let mut crawl = Crawl { http, start, scope, dir, options, robots, gap, manifest, known, done: Mirrored::default(), warc };
    crawl.run(tell).await?;
    if let Some(warc) = crawl.warc.take() {
        warc.finish()?;
    }
    Ok(crawl.done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::serve;

    #[test]
    fn robots_rules_are_read_for_this_app_or_every_robot() {
        let text = "User-agent: Googlebot\nDisallow: /\n\nUser-agent: *\nDisallow: /private/\nAllow: /private/open$\nDisallow: /*.pdf$\nCrawl-delay: 3\n";
        let robots = Robots::parse(text, "EndosUnifiedDownloader/2.0.0");
        assert!(robots.allows("/") && robots.allows("/docs/a.html") && robots.allows("/private/open"));
        assert!(!robots.allows("/private/x") && !robots.allows("/files/a.pdf") && robots.allows("/files/a.pdf?x"));
        assert_eq!(robots.delay, Some(Duration::from_secs(3)));
        let ours = Robots::parse("User-agent: *\nDisallow:\n\nUser-agent: EndosUnifiedDownloader\nDisallow: /\n", "EndosUnifiedDownloader/2.0.0");
        assert!(!ours.allows("/anything"), "a group naming the app wins over *");
        assert!(Robots::parse("", "x").allows("/"));
    }

    #[test]
    fn files_go_under_host_and_path_and_links_become_relative() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(local_path(&url("https://example.com/docs/"), true).unwrap(), PathBuf::from("example.com/docs/index.html"));
        assert_eq!(local_path(&url("https://example.com/docs/about"), true).unwrap(), PathBuf::from("example.com/docs/about.html"));
        assert_eq!(local_path(&url("https://example.com/a/b.css?v=2"), false).unwrap(), PathBuf::from("example.com/a/b.css_v=2"));
        assert_eq!(local_path(&url("http://example.com:8080/x.png"), false).unwrap(), PathBuf::from("example.com_8080/x.png"));
        assert_eq!(relative(Path::new("h/docs/index.html"), Path::new("h/docs/a b.html")), "a%20b.html");
        assert_eq!(relative(Path::new("h/docs/x/index.html"), Path::new("h/img/logo.png")), "../../img/logo.png");
    }

    #[test]
    fn links_are_found_in_pages_and_style_sheets_and_rewritten_in_place() {
        let base = Url::parse("https://example.com/docs/").unwrap();
        let html = r#"<html><head><link rel="stylesheet" href="s.css"><link rel="canonical" href="/x"></head>
            <body><a href='a.html#top'>A</a> <a href=b.html>B</a> <img src="/i.png"> <a href="mailto:x@y">m</a> <a href="https://other.example/">o</a></body></html>"#;
        let links = html_links(html, &base);
        let found: Vec<(&str, bool)> = links.iter().map(|l| (l.url.as_str(), l.page)).collect();
        assert_eq!(found, [
            ("https://example.com/docs/s.css", false),
            ("https://example.com/docs/a.html", true),
            ("https://example.com/docs/b.html", true),
            ("https://example.com/i.png", false),
            ("https://other.example/", true),
        ]);
        let out = rewrite(html, &links, true, |l| (l.url.path() == "/docs/a.html").then(|| "a.html?x&y".to_string()));
        assert!(out.contains("<a href='a.html?x&amp;y'>A</a>") && out.contains("<a href=b.html>B</a>"), "{out}");
        let css = "@import 'base.css'; body { background: url( \"img/bg.png\" ) } .x { background: url(data:image/png;base64,AA) }";
        let links = css_links(css, &Url::parse("https://example.com/s/main.css").unwrap());
        assert_eq!(links.iter().map(|l| l.url.as_str()).collect::<Vec<_>>(), ["https://example.com/s/base.css", "https://example.com/s/img/bg.png"]);
        assert_eq!(rewrite(css, &links, false, |_| Some("Z".into())), "@import 'Z'; body { background: url( \"Z\" ) } .x { background: url(data:image/png;base64,AA) }");
    }

    /// A small site: robots.txt keeps /secret/ out, pages link each other, one style sheet and
    /// image; a page on another host and one above the start's folder are not fetched.
    fn site(path: &str, _: Option<crate::range::ByteRange>) -> crate::hls::tests::Reply {
        let html = |body: &str| (200, "Content-Type: text/html\r\n".to_string(), body.as_bytes().to_vec());
        match path {
            "/robots.txt" => (200, String::new(), b"User-agent: *\nDisallow: /docs/secret/\n".to_vec()),
            "/docs/" => html(r#"<link rel="stylesheet" href="s.css"><a href="a">A</a> <a href="secret/x.html">S</a> <a href="/up.html">U</a> <a href="https://other.example/">O</a>"#),
            "/docs/a" => html(r#"<img src="/img/i.png"><a href="deep/b.html">B</a> <a href="./">home</a>"#),
            "/docs/deep/b.html" => html(r#"<a href="c.html">C</a>"#),
            "/docs/s.css" => (200, "Content-Type: text/css\r\n".to_string(), b"body { background: url(/img/bg.png) }".to_vec()),
            "/img/i.png" | "/img/bg.png" => (200, "Content-Type: image/png\r\n".to_string(), b"PNG".to_vec()),
            _ => (404, String::new(), Vec::new()),
        }
    }

    /// The pages under the start's folder as deep as asked, what they use when asked, robots.txt
    /// honoured; links rewritten for browsing offline, a manifest with each file's SHA-256; a
    /// second run reads what the first saved instead of asking again.
    #[tokio::test]
    async fn a_site_is_mirrored_for_offline_browsing_and_a_second_run_resumes() {
        let (addr, hits) = serve(site).await;
        let dir = tempfile::tempdir().unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let start = Url::parse(&format!("http://{addr}/docs/")).unwrap();
        let options = MirrorOptions { depth: 1, requisites: true, delay: Duration::ZERO, ..MirrorOptions::default() };
        let mut asked = Vec::new();
        let done = mirror(&http, &start, dir.path(), &options, |u| asked.push(u.to_string())).await.unwrap();
        assert_eq!((done.pages, done.files, done.disallowed, done.stopped.as_deref()), (2, 3, 1, None), "{done:?}");
        let host = format!("127.0.0.1_{}", addr.port());
        let read = |p: &str| std::fs::read_to_string(dir.path().join(&host).join(p)).unwrap();
        let home = read("docs/index.html");
        assert!(home.contains(r#"href="s.css""#) && home.contains(r#"href="a.html""#), "{home}");
        assert!(home.contains(&format!(r#"href="http://{addr}/docs/secret/x.html""#)) && home.contains(&format!(r#"href="http://{addr}/up.html""#)), "{home}");
        let a = read("docs/a.html");
        assert!(a.contains(r#"src="../img/i.png""#) && a.contains(&format!(r#"href="http://{addr}/docs/deep/b.html""#)) && a.contains(r#"href="index.html""#), "{a}");
        assert_eq!(read("docs/s.css"), "body { background: url(../img/bg.png) }");
        assert_eq!(read("img/bg.png"), "PNG");
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(dir.path().join(MANIFEST)).unwrap()).unwrap();
        assert_eq!(manifest.files.len(), 5);
        let png = manifest.files.iter().find(|f| f.url.ends_with("/img/i.png")).unwrap();
        assert_eq!((png.size, png.sha256.as_str()), (3, "796120837694d3f3f29259cfeb25091698c2a0aa87873658d840b4993ee889b3"));
        assert!(hits.lock().get("/docs/secret/x.html").is_none() && hits.lock().get("/up.html").is_none());

        // Deeper now: the files saved are not asked for again, the pages (rewritten) are.
        let deeper = MirrorOptions { depth: 2, ..options };
        let done = mirror(&http, &start, dir.path(), &deeper, |_| {}).await.unwrap();
        assert_eq!(done.pages, 3, "{done:?}");
        assert_eq!(hits.lock()["/img/i.png"], 1);
        assert!(read("docs/a.html").contains(r#"href="deep/b.html""#));
    }

    /// With a WARC file, each answer goes into it as received (not as rewritten), indexed.
    #[tokio::test]
    async fn a_mirror_writes_its_answers_into_a_warc() {
        use std::io::Read;
        let (addr, _) = serve(site).await;
        let dir = tempfile::tempdir().unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let warc = dir.path().join("site.warc.gz");
        let options = MirrorOptions { depth: 1, requisites: true, delay: Duration::ZERO, warc: Some(warc.clone()), ..MirrorOptions::default() };
        let done = mirror(&http, &Url::parse(&format!("http://{addr}/docs/")).unwrap(), &dir.path().join("out"), &options, |_| {}).await.unwrap();
        let mut all = Vec::new();
        flate2::read::MultiGzDecoder::new(std::fs::File::open(&warc).unwrap()).read_to_end(&mut all).unwrap();
        let records = crate::warc::tests::records(&all);
        let responses: Vec<_> = records.iter().filter(|r| r.field("WARC-Type") == Some("response")).collect();
        assert_eq!(responses.len(), done.pages + done.files);
        let css = responses.iter().find(|r| r.field("WARC-Target-URI").is_some_and(|u| u.ends_with("/docs/s.css"))).unwrap();
        assert!(css.block.starts_with(b"HTTP/1.1 200 OK\r\n") && css.block.ends_with(b"\r\n\r\nbody { background: url(/img/bg.png) }"));
        assert_eq!(std::fs::read_to_string(crate::warc::index_path(&warc)).unwrap().lines().count(), responses.len());
    }

    #[tokio::test]
    async fn robots_forbidding_the_start_is_an_error_and_caps_stop_the_crawl() {
        let (addr, _) = serve(|path, _| match path {
            "/robots.txt" => (200, String::new(), b"User-agent: *\nDisallow: /private\n".to_vec()),
            p if p.starts_with("/pages/") => (200, "Content-Type: text/html\r\n".to_string(), format!("<a href=\"{}x\">x</a>", p.trim_end_matches(".html")).into_bytes()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let options = MirrorOptions { depth: 50, delay: Duration::ZERO, max_pages: 3, ..MirrorOptions::default() };
        let refused = mirror(&http, &Url::parse(&format!("http://{addr}/private/")).unwrap(), dir.path(), &options, |_| {}).await.unwrap_err();
        assert!(refused.contains("robots.txt"), "{refused}");
        let done = mirror(&http, &Url::parse(&format!("http://{addr}/pages/a")).unwrap(), dir.path(), &options, |_| {}).await.unwrap();
        assert_eq!((done.pages, done.stopped.as_deref()), (3, Some("the page cap (3) was reached")));
        let small = MirrorOptions { max_bytes: 10, ..options };
        let other = tempfile::tempdir().unwrap();
        let done = mirror(&http, &Url::parse(&format!("http://{addr}/pages/a")).unwrap(), other.path(), &small, |_| {}).await.unwrap();
        assert!(done.stopped.is_some_and(|s| s.contains("size cap")));
    }
}
