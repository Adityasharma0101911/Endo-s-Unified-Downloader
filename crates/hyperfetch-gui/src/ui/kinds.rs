//! What a link is, in plain words: the kinds of things the app downloads, as data. The link bar
//! names the kind of what is typed in it (see [`recognise`]) on its preview card; the Sources page
//! lists every kind with an example to try and what it needs (your account, a helper tool). A new
//! source is a new row of [`KINDS`], not a new screen.

use hyperfetch_core::{archives, cloud, debrid, feeds, folders, gallery, iasearch, itch, kaggle, mega, mods, oci, osm, packages, repos, resolver, shares, torrent, wayback};
use url::Url;

use super::*;

/// What a kind of link needs beyond the app.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Needs {
    Nothing,
    /// Your own account with the service: the setting (searched for in Settings) that holds it.
    Account(&'static str),
    /// A helper program the app fetches, from its official release, the first time it is needed.
    Tool(&'static str),
}

/// The choices that matter for a kind, offered on the link bar's preview card.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    /// Nothing to choose before it starts.
    None,
    /// The quality of a video or its sound (the Quality setting).
    Quality,
    /// The tracks of an HLS or DASH stream (Choose tracks…).
    Tracks,
    /// The files of a ZIP, listed before downloading it (See what's inside).
    Zip,
    /// The files of a list (a folder, a torrent, a playlist): asked once it is read.
    Files,
    /// A web page: downloaded as it is, or saved as one file that opens offline.
    Page,
    /// A Spotify, Apple Music or Deezer list: its songs as found on YouTube Music, to tick.
    Songs,
}

/// One kind of thing to download.
pub struct Kind {
    /// What it is, as the preview card says it: "A video".
    pub name: &'static str,
    /// The Sources page's group it is listed in.
    pub group: &'static str,
    pub glyph: &'static str,
    /// What the app does with it, in a line.
    pub about: &'static str,
    /// Something to try (empty: none that works for everyone).
    pub example: &'static str,
    pub needs: Needs,
    pub choice: Choice,
    /// Whether a single link or input `text` (its URL when it parses as one) is of this kind.
    pub matches: fn(&str, Option<&Url>) -> bool,
}

/// The Sources page's groups, in order.
pub const GROUPS: [&str; 7] = ["Files & links", "Video & audio", "Torrents & Usenet", "Code, packages & models", "Science, archives & maps", "Cloud & accounts", "Games & mods"];

fn host_is(url: Option<&Url>, hosts: &[&str]) -> bool {
    url.and_then(Url::host_str).is_some_and(|host| {
        let host = host.trim_start_matches("www.");
        hosts.iter().any(|h| host == *h || host.ends_with(&format!(".{h}")))
    })
}

fn ends_with(text: &str, url: Option<&Url>, extensions: &[&str]) -> bool {
    let path = url.map_or_else(|| text.trim().to_lowercase(), |url| url.path().to_lowercase());
    extensions.iter().any(|ext| path.ends_with(ext))
}

fn web(url: Option<&Url>) -> Option<&Url> {
    url.filter(|url| matches!(url.scheme(), "http" | "https"))
}

/// Every kind, the most particular first: the first that matches names a link.
pub const KINDS: &[Kind] = &[
    Kind {
        name: "A magnet link",
        group: "Torrents & Usenet",
        glyph: icon::MAGNET,
        about: "Downloads from the BitTorrent swarm, or through your debrid service when it has the torrent.",
        example: "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny",
        needs: Needs::Nothing,
        choice: Choice::Files,
        matches: |text, _| torrent::is_magnet_uri(text),
    },
    Kind {
        name: "A torrent",
        group: "Torrents & Usenet",
        glyph: icon::MAGNET,
        about: "Every file it lists, from its web seeds first, then from peers.",
        example: "https://webtorrent.io/torrents/big-buck-bunny.torrent",
        needs: Needs::Nothing,
        choice: Choice::Files,
        matches: |text, url| ends_with(text, url, &[".torrent"]),
    },
    Kind {
        name: "A Usenet download",
        group: "Torrents & Usenet",
        glyph: icon::NEWSPAPER,
        about: "An .nzb's files from your Usenet servers, checked part by part and against its PAR2 set.",
        example: "",
        needs: Needs::Account("Usenet"),
        choice: Choice::Files,
        matches: |text, url| ends_with(text, url, &[".nzb"]),
    },
    Kind {
        name: "A metalink",
        group: "Files & links",
        glyph: icon::TREE_STRUCTURE,
        about: "One file from all the mirrors it lists at once, each piece checked against its hash.",
        example: "",
        needs: Needs::Nothing,
        choice: Choice::Files,
        matches: |text, url| ends_with(text, url, &[".metalink", ".meta4"]),
    },
    Kind {
        name: "A chunked image",
        group: "Files & links",
        glyph: icon::STACK,
        about: "A casync or desync index: the file or folder it lists, each chunk checked, those already on disk not fetched again.",
        example: "",
        needs: Needs::Nothing,
        choice: Choice::None,
        matches: |text, url| ends_with(text, url, &[".caibx", ".caidx"]),
    },
    Kind {
        name: "A TV channel list",
        group: "Video & audio",
        glyph: icon::TELEVISION,
        about: "Its channels to tick; each records for as long as Settings say.",
        example: "",
        needs: Needs::Nothing,
        choice: Choice::Files,
        matches: |text, _| ingest::might_be_channel_list(text.trim()),
    },
    Kind {
        name: "A stream",
        group: "Video & audio",
        glyph: icon::BROADCAST,
        about: "An HLS or DASH stream: pick its video, audio and subtitle tracks, or take the best.",
        example: "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8",
        needs: Needs::Nothing,
        choice: Choice::Tracks,
        matches: |text, _| crate::tracks::stream_link(text).is_some(),
    },
    Kind {
        name: "Songs to find",
        group: "Video & audio",
        glyph: icon::MUSIC_NOTES,
        about: "A Spotify, Apple Music or Deezer song, album or playlist: each song found on YouTube Music and shown before anything downloads.",
        example: "https://www.deezer.com/album/302127",
        needs: Needs::Tool("yt-dlp"),
        choice: Choice::Songs,
        matches: |_, url| web(url).is_some_and(hyperfetch_core::catalog::lists),
    },
    Kind {
        name: "A playlist or channel",
        group: "Video & audio",
        glyph: icon::FILM_STRIP,
        about: "Every video of it, the ones downloaded before left out when Settings say so.",
        example: "https://www.youtube.com/@BlenderOfficial/videos",
        needs: Needs::Tool("yt-dlp"),
        choice: Choice::Quality,
        matches: |_, url| web(url).is_some_and(media::lists),
    },
    Kind {
        name: "A video",
        group: "Video & audio",
        glyph: icon::FILM_STRIP,
        about: "From YouTube, Vimeo and over a thousand other sites, in the quality you choose.",
        example: "https://vimeo.com/1084537",
        needs: Needs::Tool("yt-dlp"),
        choice: Choice::Quality,
        matches: |_, url| web(url).is_some_and(media::is_supported_media_site),
    },
    Kind {
        name: "An image gallery",
        group: "Video & audio",
        glyph: icon::IMAGES,
        about: "Imgur, Pixiv, DeviantArt, ArtStation, Flickr, boorus and more, picture by picture.",
        example: "",
        needs: Needs::Tool("gallery-dl"),
        choice: Choice::Files,
        matches: |_, url| web(url).is_some_and(gallery::handles),
    },
    Kind {
        name: "A podcast or feed",
        group: "Video & audio",
        glyph: icon::WAVEFORM,
        about: "Its episodes or files, newest first, the ones you have left out.",
        example: "",
        needs: Needs::Nothing,
        choice: Choice::Files,
        matches: |_, url| web(url).is_some_and(feeds::lists),
    },
    // Listed in Sources only: a live stream's link reads as a video, subtitles come after one.
    Kind {
        name: "A live stream",
        group: "Video & audio",
        glyph: icon::RECORD,
        about: "Recorded until it ends or reaches the time limit in Settings; streamlink records the ones yt-dlp cannot follow, when one of its plugins takes the link.",
        example: "",
        needs: Needs::Tool("yt-dlp"),
        choice: Choice::None,
        matches: |_, _| false,
    },
    Kind {
        name: "Subtitles",
        group: "Video & audio",
        glyph: icon::SUBTITLES,
        about: "A finished video's subtitles from OpenSubtitles, found by its hash or its title, in the languages Settings name: the Library's Find subtitles.",
        example: "",
        needs: Needs::Account("OpenSubtitles"),
        choice: Choice::None,
        matches: |_, _| false,
    },
    // Before container images: an Ollama link reads as an image reference too.
    Kind {
        name: "A local AI model",
        group: "Code, packages & models",
        glyph: icon::BRAIN,
        about: "An Ollama model's layers, as a GGUF file or straight into an Ollama models folder.",
        example: "https://ollama.com/library/smollm:135m",
        needs: Needs::Nothing,
        choice: Choice::None,
        matches: |text, url| text.trim().starts_with("ollama://") || host_is(url, &["ollama.com"]),
    },
    Kind {
        name: "A container image",
        group: "Code, packages & models",
        glyph: icon::CUBE,
        about: "Its layers from Docker Hub, GitHub, Quay or any registry, each checked against its digest; a Helm chart (oci://) as its .tgz with its provenance.",
        example: "docker.io/library/alpine:latest",
        needs: Needs::Nothing,
        choice: Choice::None,
        matches: |text, _| oci::reference(text).is_some(),
    },
    Kind {
        name: "A model or dataset",
        group: "Code, packages & models",
        glyph: icon::BRAIN,
        about: "Hugging Face and Civitai files with their published hashes.",
        example: "https://huggingface.co/openai-community/gpt2",
        needs: Needs::Nothing,
        choice: Choice::Files,
        matches: |_, url| host_is(url, &["huggingface.co", "hf.co", "civitai.com"]),
    },
    Kind {
        name: "A package",
        group: "Code, packages & models",
        glyph: icon::PACKAGE,
        about: "A package's page on PyPI, npm, crates.io, Maven Central, NuGet, pkg.go.dev, Homebrew, Flathub, anaconda.org, RubyGems, Packagist or F-Droid, with the checksums its registry publishes.",
        example: "https://f-droid.org/packages/org.fdroid.fdroid/",
        needs: Needs::Nothing,
        choice: Choice::None,
        matches: |_, url| host_is(url, &["pypi.org", "npmjs.com", "crates.io"]) || web(url).is_some_and(packages::lists),
    },
    Kind {
        name: "A code repository",
        group: "Code, packages & models",
        glyph: icon::GIT_BRANCH,
        about: "A repository's releases or source from GitHub, Codeberg and other forges.",
        example: "https://github.com/sharkdp/fd/releases/latest",
        needs: Needs::Nothing,
        choice: Choice::Files,
        // An Internet Archive item is listed there too, but is its own kind.
        matches: |_, url| !host_is(url, &["archive.org"]) && web(url).is_some_and(|url| repos::lists(url) || repos::handles(url)),
    },
    Kind {
        name: "A paper",
        group: "Science, archives & maps",
        glyph: icon::BOOK_OPEN,
        about: "The legal open copy of a DOI, arXiv, PubMed Central or bioRxiv paper (through Unpaywall with your email), else its page; no paywall is bypassed.",
        example: "https://arxiv.org/abs/1706.03762",
        needs: Needs::Nothing,
        choice: Choice::None,
        matches: |text, url| {
            let text = text.trim();
            let doi = text.strip_prefix("doi:").unwrap_or(text);
            (doi.starts_with("10.") && doi.contains('/')) || host_is(url, &["arxiv.org", "doi.org", "dx.doi.org", "biorxiv.org", "medrxiv.org"])
        },
    },
    Kind {
        name: "A book or document",
        group: "Science, archives & maps",
        glyph: icon::BOOKS,
        about: "Books, papers and articles from Anna's Archive, Library Genesis and digital repositories, resolved to fast unthrottled mirrors.",
        example: "https://annas-archive.gl/md5/8f07ef504d743288f62539da685d2543",
        needs: Needs::Nothing,
        choice: Choice::None,
        matches: |_, url| web(url).is_some_and(|url| {
            resolver::AnnaArchiveResolver::can_handle(url)
                || resolver::LibGenResolver::can_handle(url)
        }),
    },
    Kind {
        name: "A map extract",
        group: "Science, archives & maps",
        glyph: icon::MAP_TRIFOLD,
        about: "OpenStreetMap regions from Geofabrik and the planet file, checked against their published MD5.",
        example: "https://download.geofabrik.de/europe/monaco-latest.osm.pbf",
        needs: Needs::Nothing,
        choice: Choice::None,
        matches: |_, url| web(url).is_some_and(osm::lists),
    },
    Kind {
        name: "A dataset",
        group: "Science, archives & maps",
        glyph: icon::DATABASE,
        about: "A Kaggle dataset as one archive, or a competition's data with your kaggle.json; public datasets need no sign-in.",
        example: "",
        needs: Needs::Account("kaggle.json"),
        choice: Choice::None,
        matches: |_, url| web(url).is_some_and(kaggle::lists),
    },
    Kind {
        name: "An archive item",
        group: "Science, archives & maps",
        glyph: icon::ARCHIVE,
        about: "Internet Archive items, the Wayback Machine, Zenodo, Kiwix, Project Gutenberg and more.",
        example: "https://archive.org/details/BigBuckBunny_124",
        needs: Needs::Nothing,
        choice: Choice::Files,
        matches: |_, url| web(url).is_some_and(|url| archives::lists(url) || iasearch::lists(url) || wayback::lists(url) || wayback::handles(url)),
    },
    Kind {
        name: "A shared folder",
        group: "Cloud & accounts",
        glyph: icon::FOLDERS,
        about: "Google Drive, Dropbox, OneDrive, MEGA and other shared folders, file by file.",
        example: "",
        needs: Needs::Nothing,
        choice: Choice::Files,
        matches: |_, url| web(url).is_some_and(|url| cloud::lists(url) || folders::lists(url) || mega::lists(url) || shares::lists(url)),
    },
    Kind {
        name: "A cloud drive",
        group: "Cloud & accounts",
        glyph: icon::CLOUD,
        about: "A folder of one of your own rclone remotes (rclone://), its files downloaded with the app's own connections.",
        example: "",
        needs: Needs::Tool("rclone"),
        choice: Choice::Files,
        matches: |text, _| text.trim().starts_with("rclone://"),
    },
    Kind {
        name: "A bucket",
        group: "Cloud & accounts",
        glyph: icon::HARD_DRIVES,
        about: "S3 and S3-compatible buckets and Google Cloud Storage, private ones with your keys.",
        example: "",
        needs: Needs::Account("S3"),
        choice: Choice::Files,
        matches: |_, url| url.is_some_and(|url| matches!(url.scheme(), "s3" | "gs")),
    },
    Kind {
        name: "A file on a server",
        group: "Files & links",
        glyph: icon::HARD_DRIVES,
        about: "FTP, FTPS, SFTP and WebDAV, resumed and checked like any download.",
        example: "",
        needs: Needs::Nothing,
        choice: Choice::None,
        matches: |_, url| url.is_some_and(|url| matches!(url.scheme(), "ftp" | "ftps" | "sftp" | "webdav" | "webdavs" | "dav" | "davs")),
    },
    Kind {
        name: "A file host link",
        group: "Cloud & accounts",
        glyph: icon::CLOUD_ARROW_DOWN,
        about: "Hoster links at full speed through your debrid account.",
        example: "",
        needs: Needs::Account("Debrid"),
        choice: Choice::None,
        matches: |_, url| web(url).is_some_and(debrid::is_debrid_host),
    },
    Kind {
        name: "A game from your library",
        group: "Games & mods",
        glyph: icon::GAME_CONTROLLER,
        about: "Games you own on Epic, GOG or Steam by their epic://, gog:// or steam:// link (never a store page), through helper programs and your own sign-in.",
        example: "",
        needs: Needs::Account("Accounts"),
        choice: Choice::Files,
        // A web link never starts one: any page can send one through the extension.
        matches: |_, url| url.is_some_and(|url| matches!(url.scheme(), "epic" | "gog" | "steam")),
    },
    Kind {
        name: "A game from itch.io",
        group: "Games & mods",
        glyph: icon::GAME_CONTROLLER,
        about: "A game's uploads, a jam's entries, a collection or your purchases, with your itch.io API key.",
        example: "",
        needs: Needs::Account("itch.io"),
        choice: Choice::Files,
        matches: |_, url| web(url).is_some_and(itch::lists),
    },
    Kind {
        name: "A mod or modpack",
        group: "Games & mods",
        glyph: icon::PUZZLE_PIECE,
        about: "Modrinth and Thunderstore files and whole packs (a .mrpack with modpack: before it), CurseForge's with your API key, each with its hash; a Nexus Mods link says which page to open.",
        example: "https://modrinth.com/mod/sodium",
        needs: Needs::Nothing,
        choice: Choice::Files,
        // The pages and links the app reads as mods: not a file's own link on their servers.
        matches: |_, url| url.is_some_and(mods::lists),
    },
    Kind {
        name: "A ZIP archive",
        group: "Files & links",
        glyph: icon::FILE_ZIP,
        about: "Its files are listed before downloading it, and only those you tick are fetched.",
        example: "",
        needs: Needs::Nothing,
        choice: Choice::Zip,
        matches: |text, url| web(url).is_some() && ends_with(text, url, &[".zip"]),
    },
    Kind {
        name: "A file",
        group: "Files & links",
        glyph: icon::FILE,
        about: "Any file over http(s), split over many connections, resumed after a cut and checked.",
        example: "https://ftp.gnu.org/gnu/hello/hello-2.12.1.tar.gz",
        needs: Needs::Nothing,
        choice: Choice::None,
        // A name with an extension, but not a page's (.html is a web page), or a host SmartResolver handles.
        matches: |text, url| {
            let handled = web(url).is_some_and(|url| resolver::SmartResolver::handles(url));
            let named = web(url).is_some_and(|url| url.path_segments().and_then(|mut s| s.next_back()).is_some_and(|last| last.contains('.')));
            handled || (named && !ends_with(text, url, PAGES))
        },
    },
    Kind {
        name: "A web page",
        group: "Files & links",
        glyph: icon::GLOBE,
        about: "Whatever it holds: its video or its files, or the page itself saved as one file that opens offline.",
        example: "https://www.gnu.org/philosophy/free-sw.html",
        needs: Needs::Nothing,
        choice: Choice::Page,
        matches: |_, url| web(url).is_some(),
    },
];

/// What a web page's link ends in.
const PAGES: &[&str] = &[".html", ".htm", ".xhtml", ".shtml"];

/// Whether a link of `kind` names its file by its last part, as a file's link does: the link
/// bar's card then shows that name. The others are named by their site once they start (a
/// video page's link ends in `watch`, a playlist's in `videos`).
pub fn named_by_link(kind: &Kind) -> bool {
    matches!(kind.name, "A torrent" | "A Usenet download" | "A metalink" | "A chunked image" | "A file on a server" | "A map extract" | "A ZIP archive" | "A file")
}

/// What the ranges of the link bar's `text` stand for (see `ingest::range_span`), counted once
/// for each text it holds rather than on every frame.
pub fn range_span(text: &str) -> Result<Option<(usize, String, String)>, String> {
    type Span = Result<Option<(usize, String, String)>, String>;
    thread_local! {
        static LAST: std::cell::RefCell<Option<(String, Span)>> = const { std::cell::RefCell::new(None) };
    }
    let text = text.trim();
    LAST.with_borrow_mut(|last| {
        if let Some((_, span)) = last.as_ref().filter(|(seen, _)| seen == text) {
            return span.clone();
        }
        let span = ingest::range_span(text);
        *last = Some((text.to_string(), span.clone()));
        span
    })
}

/// What `text` (the link bar's input) is: its kind, and where it comes from (a host, or "this
/// computer"); None for nothing, or text that is not a link. Several lines are "Several links".
pub fn recognise(text: &str) -> Option<(&'static Kind, String)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let tokens = ingest::split_tokens(text);
    if text.lines().filter(|line| !line.trim().is_empty()).count() > 1 {
        return Some((&SEVERAL, format!("{} lines", text.lines().filter(|line| !line.trim().is_empty()).count())));
    }
    // Links to one file from several servers, separated by spaces, are one download.
    let first = tokens.first().map(String::as_str).unwrap_or(text);
    let url = Url::parse(first).ok();
    // A link with ranges (`img[001-120].jpg`) stands for several.
    if range_span(text).is_ok_and(|span| span.is_some_and(|(count, ..)| count > 1)) {
        return Some((&SEVERAL, url.as_ref().and_then(Url::host_str).unwrap_or_default().trim_start_matches("www.").to_string()));
    }
    let kind = KINDS.iter().find(|kind| (kind.matches)(first, url.as_ref()))?;
    let source = match url.as_ref().and_then(Url::host_str) {
        Some(host) => host.trim_start_matches("www.").to_string(),
        None if url.is_none() && (first.contains('\\') || first.starts_with('/') || first.starts_with('~')) => "this computer".to_string(),
        None => String::new(),
    };
    let source = if tokens.len() > 1 { format!("{source} and {} more servers", tokens.len() - 1) } else { source };
    Some((kind, source))
}

/// The kind named `name`.
pub fn named(name: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|kind| kind.name == name)
}

/// The single http(s) link `text` is, when its text alone does not say what it is: a web page
/// only because nothing else took it, or a file only by a dot in its name that no file's
/// extension follows (`report.php?id=3`, a wiki's `Node.js`). Its server is asked (see
/// `hyperfetch_core::peek`): the link bar's card then says what it is and what it holds. Not one
/// whose host is a bare word (`https://exa`): a link still being typed is asked of nobody.
pub fn unsure(text: &str) -> Option<Url> {
    let text = text.trim();
    let url = Url::parse(text).ok().filter(|url| matches!(url.scheme(), "http" | "https") && !text.contains(char::is_whitespace))?;
    let whole = match url.host()? {
        url::Host::Domain(name) => name.contains('.') || name == "localhost",
        _ => true,
    };
    if !whole {
        return None;
    }
    let sure = match recognise(text)?.0.name {
        "A web page" => false,
        "A file" => resolver::named_file(&url).is_some() || resolver::SmartResolver::handles(&url),
        _ => true,
    };
    (!sure).then_some(url)
}

/// Several links, a line each.
pub static SEVERAL: Kind = Kind {
    name: "Several links",
    group: "Files & links",
    glyph: icon::LIST_DASHES,
    about: "One download a line, one for each link a range in a link stands for ([1-20], [001-120], [a-f], [0-100:10]); links to the same file from several servers go on one line.",
    example: "",
    needs: Needs::Nothing,
    choice: Choice::None,
    matches: |_, _| false,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Links are named by their kind, the most particular first, with where they come from.
    #[test]
    fn links_are_named_by_their_kind() {
        let name = |text: &str| recognise(text).map(|(kind, source)| (kind.name, source));
        assert_eq!(name(""), None);
        assert_eq!(name("  "), None);
        assert_eq!(name("https://example.com/a.iso"), Some(("A file", "example.com".to_string())));
        assert_eq!(name("https://www.example.com/img[001-120].jpg"), Some(("Several links", "example.com".to_string())));
        assert_eq!(name("https://example.com/a.iso?part[1-2]").unwrap().0, "A file");
        assert_eq!(name("https://example.com/pack.zip").unwrap().0, "A ZIP archive");
        assert_eq!(name("https://example.com/").unwrap().0, "A web page");
        assert_eq!(name("https://example.com/x.torrent").unwrap().0, "A torrent");
        assert_eq!(name("magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c").unwrap().0, "A magnet link");
        assert_eq!(name("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), Some(("A video", "youtube.com".to_string())));
        assert_eq!(name("https://cdn.example/v/master.m3u8").unwrap().0, "A stream");
        assert_eq!(name("https://arxiv.org/abs/1706.03762").unwrap().0, "A paper");
        assert_eq!(name("10.1038/nature12373").unwrap().0, "A paper");
        assert_eq!(name("https://annas-archive.gl/md5/8f07ef504d743288f62539da685d2543").unwrap().0, "A book or document");
        assert_eq!(name("https://annas-archive.org/md5/8f07ef504d743288f62539da685d2543").unwrap().0, "A book or document");
        assert_eq!(name("https://library.lol/main/8f07ef504d743288f62539da685d2543").unwrap().0, "A book or document");
        assert_eq!(name("https://a.example/f.iso https://b.example/f.iso").map(|(_, source)| source).as_deref(), Some("a.example and 1 more servers"));
        assert_eq!(name("https://a.example/1.iso\nhttps://a.example/2.iso").unwrap().0, "Several links");
        assert_eq!(name("ftp://ftp.example/pub/a.iso").unwrap().0, "A file on a server");
        assert_eq!(name("https://example.com/about/index.html").unwrap().0, "A web page");
        assert_eq!(name("https://example.com/report.php?id=3").unwrap().0, "A file");
        // Account games start from their own links, never from a store page.
        assert_eq!(name("steam://app/228980").unwrap().0, "A game from your library");
        assert_eq!(name("gog://game/1207").unwrap().0, "A game from your library");
        assert_ne!(name("https://store.steampowered.com/app/228980/").unwrap().0, "A game from your library");
        // A plain .mrpack link is a file; with modpack: before it, a modpack.
        assert_eq!(name("https://cdn.modrinth.com/data/x/pack.mrpack").unwrap().0, "A file");
        assert_eq!(name("modpack:https://cdn.modrinth.com/data/x/pack.mrpack").unwrap().0, "A mod or modpack");
        assert_eq!(name("nxm://skyrimspecialedition/mods/266/files/1000172397").unwrap().0, "A mod or modpack");
        assert_eq!(name("https://www.nexusmods.com/skyrimspecialedition/mods/266").unwrap().0, "A mod or modpack");
        assert_eq!(name("oci://ghcr.io/org/charts/app:1.0.0").unwrap().0, "A container image");
        // A streaming service's list is found on YouTube Music; its artist's page is not.
        assert_eq!(name("https://open.spotify.com/playlist/mix0001"), Some(("Songs to find", "open.spotify.com".to_string())));
        assert_eq!(name("https://music.apple.com/us/album/sample-album/20").unwrap().0, "Songs to find");
        assert_ne!(name("https://www.deezer.com/artist/27").unwrap().0, "Songs to find");
    }

    /// Only kinds whose links end in their file's name show it; every name listed is a kind.
    #[test]
    fn only_file_links_are_named_by_their_link() {
        let named: Vec<&str> = KINDS.iter().filter(|kind| named_by_link(kind)).map(|kind| kind.name).collect();
        assert_eq!(named.len(), 8, "{named:?}");
        for text in [
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://www.youtube.com/@BlenderOfficial/videos",
            "https://arxiv.org/abs/1706.03762",
            "https://annas-archive.gl/md5/8f07ef504d743288f62539da685d2543",
            "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8",
            "https://example.com/",
        ] {
            assert!(!named_by_link(recognise(text).unwrap().0), "{text}");
        }
        assert!(named_by_link(recognise("https://ftp.gnu.org/gnu/hello/hello-2.12.1.tar.gz").unwrap().0));
    }

    /// A link's server is asked what it is only where its text leaves that open: a page that
    /// nothing else took, or a name with a dot that no file's extension follows.
    #[test]
    fn only_a_link_whose_text_does_not_say_is_asked() {
        for text in [
            "https://example.com/",
            "https://example.com/download?id=5",
            "https://example.com/about/index.html",
            "https://example.com/report.php?id=3",
            "https://en.wikipedia.org/wiki/Node.js",
            "https://bit.ly/3abcde",
            "http://127.0.0.1:8080/get",
            "http://localhost/get",
        ] {
            assert_eq!(unsure(text).map(|url| url.to_string()).as_deref(), Some(text), "{text}");
        }
        for text in [
            "",
            "not a link",
            "https://exa",
            "https://example.com/a.iso",
            "https://example.com/pack.zip",
            "https://example.com/song.mp3",
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://annas-archive.gl/md5/8f07ef504d743288f62539da685d2543",
            "https://drive.google.com/file/d/1AbCdEfGhIjKlMnOpQrStUvWxYz012345/view",
            "https://github.com/sharkdp/fd/releases/latest",
            "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c",
            "ftp://ftp.example/pub/a.dat",
            "https://a.example/f.dat https://b.example/f.dat",
            "https://a.example/1\nhttps://a.example/2",
        ] {
            assert_eq!(unsure(text), None, "{text}");
        }
        assert_eq!(named("A file").map(|kind| kind.choice), Some(Choice::None));
        assert_eq!(named("A web page").map(|kind| kind.choice), Some(Choice::Page));
    }

    /// Every kind's example is named as that kind (or a more particular one in its group), and
    /// every kind is in a group the Sources page shows.
    #[test]
    fn every_example_is_its_own_kind() {
        for kind in KINDS {
            assert!(GROUPS.contains(&kind.group), "{}", kind.name);
            if kind.example.is_empty() {
                continue;
            }
            let (found, _) = recognise(kind.example).unwrap_or_else(|| panic!("{} not recognised", kind.example));
            assert_eq!(found.name, kind.name, "{}", kind.example);
        }
    }
}
