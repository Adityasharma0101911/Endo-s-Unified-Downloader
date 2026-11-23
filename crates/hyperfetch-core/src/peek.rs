//! What a link is, asked of its server: for a link whose text does not say (no site the app
//! knows, no file's extension), the link bar's card asks before anything is downloaded (see the
//! GUI's `kinds::unsure`). One HEAD request, which takes none of the file, tells a file: its type
//! as the server labels it, its name and its size. A web page is then read, as far as the engine
//! reads one it looks into, for what it holds: its title, whether it plays a video or sound, and
//! the files it links to. Nothing here decides a download: the engine judges the answer it gets
//! itself (see `DownloadEngine::fetch_resolved`).

use std::time::Duration;

use reqwest::header::{ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use url::Url;

use crate::resolver::{self, HtmlVideoResolver};

/// How long the server has to answer each of the two requests.
const LIMIT: Duration = Duration::from_secs(10);

/// Most files of a page that are kept.
pub const MAX_FILES: usize = 500;

/// How long a page's title is kept, in characters.
const MAX_TITLE: usize = 150;

/// The extensions of sound files.
const SOUND: &[&str] = &["mp3", "flac", "wav", "m4a", "m4b", "aac", "ogg", "opus", "wma", "mka"];

/// What a link's server says is there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Peek {
    File(File),
    Page(Page),
}

/// A file, as its server's headers describe it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct File {
    /// Its name, when the server gives one (Content-Disposition).
    pub name: Option<String>,
    /// Its type as labelled (`audio/mpeg`), in lower case, parameters aside; empty when unlabelled.
    pub mime: String,
    pub size: Option<u64>,
}

impl File {
    /// What sort of file its label says it is, in a word or two; None for a label that says
    /// nothing (`application/octet-stream`) or that is not known.
    pub fn sort(&self) -> Option<&'static str> {
        let mime = self.mime.as_str();
        Some(match mime {
            "application/pdf" => "PDF document",
            "application/epub+zip" => "E-book",
            "application/x-bittorrent" => "Torrent",
            "application/vnd.apple.mpegurl" | "application/x-mpegurl" | "application/dash+xml" => "Stream",
            "application/zip" | "application/x-zip-compressed" | "application/x-7z-compressed" | "application/vnd.rar" | "application/x-rar-compressed" | "application/gzip"
            | "application/x-gzip" | "application/x-tar" | "application/x-xz" | "application/x-bzip2" | "application/zstd" => "Archive",
            "application/x-msdownload" | "application/x-msi" | "application/vnd.microsoft.portable-executable" | "application/vnd.android.package-archive"
            | "application/x-apple-diskimage" => "Program",
            "application/json" | "application/xml" | "text/xml" | "text/csv" => "Data",
            _ if mime.starts_with("audio/") => "Sound",
            _ if mime.starts_with("video/") => "Video",
            _ if mime.starts_with("image/") => "Picture",
            _ if mime.starts_with("font/") => "Font",
            _ if mime.starts_with("text/") => "Text",
            _ => return None,
        })
    }
}

/// A web page and what it holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub title: Option<String>,
    /// It plays a video: one the engine finds in it (see `resolver::extract_html_video_source`),
    /// the player of a site yt-dlp reads, or a `<video>`.
    pub video: bool,
    /// It plays sound: an `<audio>`, or a link to a sound file.
    pub sound: bool,
    /// The files it links to (see `resolver::named_file`), in its order, each once, at most
    /// [`MAX_FILES`].
    pub files: Vec<Url>,
}

/// What a link was asked: where it landed, after its redirects, and what is there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peeked {
    pub landed: Url,
    pub what: Peek,
}

/// What the server of `url` says it is (see the module). An error when it does not answer the
/// question: many servers refuse HEAD, and the link is then what its text says.
pub async fn peek(http: &reqwest::Client, url: &Url) -> Result<Peeked, String> {
    // Uncompressed, so that the length is the file's.
    let head = http.head(url.clone()).header(ACCEPT_ENCODING, "identity").timeout(LIMIT).send().await.map_err(|e| e.to_string())?;
    if !head.status().is_success() {
        return Err(format!("HTTP {}", head.status()));
    }
    let landed = head.url().clone();
    let headers = head.headers();
    if !resolver::html_type(headers) {
        let label = headers.get(CONTENT_TYPE).and_then(|value| value.to_str().ok()).and_then(|value| value.split(';').next());
        let mime = label.unwrap_or_default().trim().to_ascii_lowercase();
        let size = headers.get(CONTENT_LENGTH).and_then(|value| value.to_str().ok()).and_then(|value| value.trim().parse().ok());
        return Ok(Peeked { landed, what: Peek::File(File { name: crate::engine::disposition_name(headers), mime, size }) });
    }
    let page = http.get(landed).timeout(LIMIT).send().await.map_err(|e| e.to_string())?;
    if !page.status().is_success() {
        return Err(format!("HTTP {}", page.status()));
    }
    let landed = page.url().clone();
    let html = HtmlVideoResolver::read_page(page).await.map_err(|e| e.to_string())?;
    Ok(Peeked { what: Peek::Page(read_page(&html, &landed)), landed })
}

/// What the page `html`, from `page`, holds.
pub fn read_page(html: &str, page: &Url) -> Page {
    let lower = html.to_ascii_lowercase();
    let tags = |name: &str| resolver::start_tags(html, &lower, name);
    let title = lower.find("<title").and_then(|at| {
        let start = at + lower[at..].find('>')? + 1;
        let end = start + lower[start..].find("</title")?;
        let title = crate::folders::unescape(&html[start..end]).split_whitespace().collect::<Vec<_>>().join(" ");
        (!title.is_empty()).then(|| crate::ingest::truncate_chars(&title, MAX_TITLE))
    });
    let mut files: Vec<Url> = Vec::new();
    for link in crate::sitemirror::html_links(html, page) {
        if link.page && files.len() < MAX_FILES && resolver::named_file(&link.url).is_some() && !files.contains(&link.url) {
            files.push(link.url);
        }
    }
    let meta = |keys: &[&str]| {
        tags("meta").into_iter().any(|tag| {
            let key = resolver::attr_value(tag, "property").or_else(|| resolver::attr_value(tag, "name")).unwrap_or_default().to_ascii_lowercase();
            keys.contains(&key.as_str()) && resolver::attr_value(tag, "content").is_some_and(|content| !content.trim().is_empty())
        })
    };
    let player = ["iframe", "embed"].into_iter().flat_map(tags).any(|tag| {
        let src = resolver::attr_value(tag, "src").and_then(|src| page.join(&src).ok());
        src.is_some_and(|src| crate::media::is_supported_media_site(&src))
    });
    let video = resolver::extract_html_video_source(html, page).is_some() || player || !tags("video").is_empty() || meta(&["og:video", "og:video:url", "og:video:secure_url"]);
    let named_sound = |url: &Url| url.path().rsplit_once('.').is_some_and(|(_, ext)| SOUND.contains(&ext.to_ascii_lowercase().as_str()));
    let sound = !tags("audio").is_empty() || meta(&["og:audio", "og:audio:url", "og:audio:secure_url"]) || files.iter().any(named_sound);
    Page { title, video, sound, files }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{serve, Reply};

    const PAGE: &str = r#"<!doctype html><html><head><title> Sample &amp;
  page </title><meta property="og:audio" content="https://cdn.example/s.mp3"></head><body>
<a href="files/report.pdf">Report</a> <a href="/files/pack.zip#top">Pack</a> <a href="files/report.pdf">again</a>
<a href="about.html">About</a> <a href="https://other.example/song.MP3">Song</a> <img src="logo.png">
<iframe src="https://www.youtube.com/embed/aqz-KE-bpKQ"></iframe></body></html>"#;

    fn site(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        let typed = |kind: &str, more: &str, body: &[u8]| (200, format!("Content-Type: {kind}\r\n{more}"), body.to_vec());
        match path {
            "/get?id=5" => typed("audio/mpeg; charset=binary", "Content-Disposition: attachment; filename=\"My Song.mp3\"\r\n", b"ID3 sound"),
            "/blob" => typed("application/octet-stream", "", b"12345"),
            "/page" => typed("text/html; charset=utf-8", "", PAGE.as_bytes()),
            "/plain" => typed("text/html", "", b"<p>Nothing here</p>"),
            "/go" => (302, "Location: /page\r\n".to_string(), Vec::new()),
            _ => (404, String::new(), Vec::new()),
        }
    }

    /// A link without a file's extension is a file when its server says so: its type, the name
    /// it gives and its size come from one HEAD request, and the file itself is never asked for.
    #[tokio::test]
    async fn a_file_is_told_by_its_headers_alone() {
        let (addr, hits) = serve(site).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let link = |path: &str| Url::parse(&format!("http://{addr}{path}")).unwrap();
        let Peek::File(file) = peek(&http, &link("/get?id=5")).await.unwrap().what else { panic!("a file") };
        assert_eq!(file, File { name: Some("My Song.mp3".to_string()), mime: "audio/mpeg".to_string(), size: Some(9) });
        assert_eq!(file.sort(), Some("Sound"));
        let Peek::File(blob) = peek(&http, &link("/blob")).await.unwrap().what else { panic!("a file") };
        assert_eq!((blob.name.as_deref(), blob.size, blob.sort()), (None, Some(5), None));
        assert_eq!(peek(&http, &link("/gone")).await, Err("HTTP 404 Not Found".to_string()));
        let hits = hits.lock().clone();
        assert_eq!((hits["/get?id=5"], hits["/blob"], hits["/gone"]), (1, 1, 1), "one request each");
    }

    /// A web page is read for what it holds: its title, the files it links to (each once, in
    /// order, a page or a picture it shows not among them), and whether it plays a video or
    /// sound; a redirect is followed, and where it landed is told.
    #[tokio::test]
    async fn a_page_is_read_for_what_it_holds() {
        let (addr, hits) = serve(site).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let link = |path: &str| Url::parse(&format!("http://{addr}{path}")).unwrap();
        let peeked = peek(&http, &link("/go")).await.unwrap();
        assert_eq!(peeked.landed, link("/page"));
        let Peek::Page(page) = peeked.what else { panic!("a page") };
        assert_eq!(page.title.as_deref(), Some("Sample & page"));
        let files: Vec<String> = page.files.iter().map(Url::to_string).collect();
        assert_eq!(files, [format!("http://{addr}/files/report.pdf"), format!("http://{addr}/files/pack.zip"), "https://other.example/song.MP3".to_string()]);
        assert!(page.video, "a YouTube player");
        assert!(page.sound);
        assert_eq!(hits.lock()["/page"], 2, "asked what it is, then read");
        let Peek::Page(plain) = peek(&http, &link("/plain")).await.unwrap().what else { panic!("a page") };
        assert_eq!(plain, Page::default());
    }

    /// What a page plays is told by its tags as well as its links.
    #[test]
    fn a_page_tells_what_it_plays() {
        let at = Url::parse("https://site.example/watch/1").unwrap();
        let read = |html: &str| read_page(html, &at);
        assert!(read(r#"<video src="blob:https://site.example/1"></video>"#).video);
        assert!(read(r#"<meta property="og:video" content="https://cdn.example/v.mp4">"#).video);
        assert!(!read(r#"<a href="clip.mp4">clip</a>"#).video, "a linked file is a file, not what the page plays");
        assert!(read(r#"<audio controls><source src="a.ogg"></audio>"#).sound);
        assert!(read(r#"<a href="/dl/track.flac">FLAC</a>"#).sound);
        let quiet = read("<title></title><p>text</p>");
        assert_eq!((quiet.title, quiet.video, quiet.sound, quiet.files.len()), (None, false, false, 0));
    }

    /// A file's label says its sort in a word; one that says nothing gives none.
    #[test]
    fn a_label_names_the_sort_of_file() {
        let sort = |mime: &str| File { mime: mime.to_string(), ..File::default() }.sort();
        assert_eq!(sort("application/pdf"), Some("PDF document"));
        assert_eq!(sort("video/mp4"), Some("Video"));
        assert_eq!(sort("image/webp"), Some("Picture"));
        assert_eq!(sort("application/zip"), Some("Archive"));
        assert_eq!(sort("text/plain"), Some("Text"));
        assert_eq!(sort("application/octet-stream"), None);
        assert_eq!(sort(""), None);
    }
}
