//! Spotify, Apple Music and Deezer links: the songs of a song, album or playlist, read from what
//! the service shows anyone (no account, no key, none of its audio), each found on YouTube Music
//! through the helper tool's search, then downloaded into the music library as its own songs are
//! (see `crate::music`).

use crate::ingest::{ListOptions, Task};
use crate::music::{Kind, Music};
use serde_json::Value;
use std::path::PathBuf;
use url::Url;

/// A service whose links name songs to find.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Spotify,
    Apple,
    Deezer,
    Qobuz,
}

impl Service {
    /// Its name, as the card says it.
    pub fn name(self) -> &'static str {
        match self {
            Service::Spotify => "Spotify",
            Service::Apple => "Apple Music",
            Service::Deezer => "Deezer",
            Service::Qobuz => "Qobuz",
        }
    }
}

/// What a link names (see [`link`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    service: Service,
    kind: Kind,
    id: String,
    /// Of Apple Music, the country whose store it is (`us`).
    country: String,
    /// The link's path, of which Apple Music's playlist page is read.
    path: String,
}

/// What `url` names when it is a Spotify, Apple Music or Deezer song, album or playlist; an
/// artist's page, or any other, is none.
fn link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.trim_start_matches("www.");
    let parts: Vec<&str> = url.path_segments()?.filter(|part| !part.is_empty()).collect();
    let id_like = |id: &str| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    let make = |service, kind, id: &str| id_like(id).then(|| Link { service, kind, id: id.to_string(), country: String::new(), path: url.path().to_string() });
    match host {
        "open.spotify.com" => {
            // A country's page (`intl-de`) and the player to embed show the same.
            let parts: Vec<&str> = parts.into_iter().filter(|part| !part.starts_with("intl-") && *part != "embed").collect();
            match parts[..] {
                ["track", id] => make(Service::Spotify, Kind::Song, id),
                ["album", id] => make(Service::Spotify, Kind::Album, id),
                ["playlist", id] => make(Service::Spotify, Kind::Playlist, id),
                _ => None,
            }
        }
        "music.apple.com" => {
            let [country, what, .., id] = parts[..] else { return None };
            // A song is shown on its album's page (`?i=<song>`).
            let song = url.query_pairs().find(|(key, _)| key == "i").map(|(_, song)| song.into_owned());
            let found = match (what, song) {
                ("album", Some(song)) => make(Service::Apple, Kind::Song, &song),
                ("album", None) => make(Service::Apple, Kind::Album, id),
                ("song", _) => make(Service::Apple, Kind::Song, id),
                ("playlist", _) => make(Service::Apple, Kind::Playlist, id),
                _ => None,
            };
            found.filter(|_| country.len() == 2).map(|link| Link { country: country.to_string(), ..link })
        }
        "deezer.com" => {
            // Its pages in a language (`/fr/album/...`) are the same.
            let parts: Vec<&str> = parts.into_iter().skip_while(|part| part.len() == 2).collect();
            let (what, id) = match parts[..] {
                [what, id] if id.chars().all(|c| c.is_ascii_digit()) => (what, id),
                _ => return None,
            };
            match what {
                "track" => make(Service::Deezer, Kind::Song, id),
                "album" => make(Service::Deezer, Kind::Album, id),
                "playlist" => make(Service::Deezer, Kind::Playlist, id),
                _ => None,
            }
        }
        "qobuz.com" | "play.qobuz.com" | "open.qobuz.com" => {
            let parts: Vec<&str> = parts.into_iter().skip_while(|part| part.contains('-') || part.len() == 2).collect();
            match parts[..] {
                ["track", id] => make(Service::Qobuz, Kind::Song, id),
                ["album", id] | ["album", _, id] => make(Service::Qobuz, Kind::Album, id),
                ["playlist", id] => make(Service::Qobuz, Kind::Playlist, id),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether `url` is a Spotify, Apple Music or Deezer song, album or playlist, whose songs are
/// found on YouTube Music (see [`list`]).
pub fn lists(url: &Url) -> bool {
    link(url).is_some()
}

/// A song of a list, as its service shows it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Song {
    /// What the music library knows it by: `<service> <id>` (`deezer 3135556`).
    pub key: String,
    pub title: String,
    /// Its artists, the main one first.
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub seconds: Option<f64>,
    /// The recording's ISRC, which Deezer gives.
    pub isrc: Option<String>,
    /// Its place on its album, from 1.
    pub track: Option<u32>,
    /// Its album's square cover.
    pub cover: Option<String>,
    /// The music library has its file already.
    pub owned: bool,
}

impl Song {
    /// "Title by Artist, Other (3:45)", as the card and the command line tell it.
    pub fn said(&self) -> String {
        said(&self.title, &self.artists.join(", "), self.seconds)
    }
}

/// `title` by `artist` and its length, as a song is told.
fn said(title: &str, artist: &str, seconds: Option<f64>) -> String {
    let by = if artist.is_empty() { String::new() } else { format!(" by {artist}") };
    let length = seconds.map_or_else(String::new, |seconds| {
        let seconds = seconds.round() as u64;
        format!(" ({}:{:02})", seconds / 60, seconds % 60)
    });
    format!("{title}{by}{length}")
}

/// A song, an album or a playlist of a service, as it shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub service: Service,
    pub kind: Kind,
    pub title: String,
    /// An album's artist, or a song's.
    pub artist: Option<String>,
    pub year: Option<u32>,
    pub cover: Option<String>,
    pub songs: Vec<Song>,
    /// What its page did not show, in a sentence ("Apple Music's page shows 100 of its 250 songs").
    pub unread: Option<String>,
}

/// Where each service answers; another for a test's stand-in.
#[derive(Debug, Clone)]
struct Bases {
    spotify: String,
    apple: String,
    itunes: String,
    deezer: String,
    qobuz: String,
}

impl Default for Bases {
    fn default() -> Self {
        Self {
            spotify: "https://open.spotify.com".to_string(),
            apple: "https://music.apple.com".to_string(),
            itunes: "https://itunes.apple.com".to_string(),
            deezer: "https://api.deezer.com".to_string(),
            qobuz: "https://www.qobuz.com/api.json/0.2".to_string(),
        }
    }
}

/// Most an answer of a service may hold.
const MOST_READ: usize = 8 << 20;

/// Spotify's player page shows no more songs of a playlist than this.
const SPOTIFY_SHOWS: usize = 100;

/// Most pages of 100 songs read of a Deezer album or playlist beyond its first answer.
const DEEZER_PAGES: usize = 20;

/// Why a page of `service` could not be read.
fn unreadable(service: Service) -> String {
    format!("{}'s page could not be read: its layout may have changed", service.name())
}

/// The body of `url`, a page or answer of `service`.
async fn get(http: &reqwest::Client, url: &str, service: Service) -> Result<Vec<u8>, String> {
    let name = service.name();
    let mut response = http.get(url).send().await.map_err(|e| format!("Could not reach {name}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{name} answered HTTP {}", response.status().as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("{name} stopped answering: {e}"))? {
        body.extend_from_slice(&chunk);
        if body.len() > MOST_READ {
            return Err(format!("{name}'s answer is over {} MiB", MOST_READ >> 20));
        }
    }
    Ok(body)
}

/// The JSON answer at `url` of `service`.
async fn get_json(http: &reqwest::Client, url: &str, service: Service) -> Result<Value, String> {
    serde_json::from_slice(&get(http, url, service).await?).map_err(|_| unreadable(service))
}

/// The text of `key` in `value`, when it has any.
fn text(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::trim).filter(|text| !text.is_empty()).map(String::from)
}

/// The year a date (`2001-03-07`, `2001-03-12T08:00:00Z`) is in.
fn year_of(date: Option<String>) -> Option<u32> {
    date?.get(..4)?.parse().ok()
}

/// The text inside the first `<script>` whose tag holds `marker`.
fn script<'a>(html: &'a str, marker: &str) -> Option<&'a str> {
    let at = html.find(marker)?;
    let start = at + html[at..].find('>')? + 1;
    let end = start + html[start..].find("</script>")?;
    Some(&html[start..end])
}

/// The songs of the Spotify, Apple Music or Deezer link `url`, as its service shows them to
/// anyone, each told whether the music library holds it already. Err says why it could not be
/// read, in a sentence.
pub async fn read(http: &reqwest::Client, url: &Url) -> Result<Listing, String> {
    read_from(http, url, &Bases::default(), crate::music::library_file()).await
}

/// [`read`] from the services at `bases`, with the library index `library`.
async fn read_from(http: &reqwest::Client, url: &Url, bases: &Bases, library: Option<PathBuf>) -> Result<Listing, String> {
    let link = link(url).ok_or_else(|| format!("{url} is no Spotify, Apple Music or Deezer song, album or playlist"))?;
    let service = link.service;
    let mut listing = match service {
        Service::Spotify => {
            let what = match link.kind {
                Kind::Song => "track",
                Kind::Album => "album",
                _ => "playlist",
            };
            // The player to embed in pages shows its songs to anyone, in the page itself.
            let page = get(http, &format!("{}/embed/{what}/{}", bases.spotify, link.id), service).await?;
            spotify(&String::from_utf8_lossy(&page), link.kind)?
        }
        Service::Apple if link.kind == Kind::Playlist => {
            let page = get(http, &format!("{}{}", bases.apple, link.path), service).await?;
            apple_playlist(&String::from_utf8_lossy(&page))?
        }
        Service::Apple => {
            let songs = if link.kind == Kind::Album { "&entity=song" } else { "" };
            let found = get_json(http, &format!("{}/lookup?id={}&country={}{songs}", bases.itunes, link.id, link.country), service).await?;
            itunes(&found, link.kind)?
        }
        Service::Deezer => {
            let what = match link.kind {
                Kind::Song => "track",
                Kind::Album => "album",
                _ => "playlist",
            };
            let found = get_json(http, &format!("{}/{what}/{}", bases.deezer, link.id), service).await?;
            let mut listing = deezer(&found, link.kind)?;
            if link.kind != Kind::Song {
                // Its answer holds its first songs; the rest come a page at a time.
                let total = found["nb_tracks"].as_u64().map_or(0, |n| n as usize);
                for _ in 0..DEEZER_PAGES {
                    if listing.songs.len() >= total {
                        break;
                    }
                    let next = format!("{}/{what}/{}/tracks?index={}&limit=100", bases.deezer, link.id, listing.songs.len());
                    let more = deezer_songs(&get_json(http, &next, service).await?);
                    if more.is_empty() {
                        break;
                    }
                    listing.songs.extend(more);
                }
                if listing.songs.len() < total {
                    listing.unread = Some(format!("Deezer listed {} of its {total} songs", listing.songs.len()));
                }
            }
            listing
        }
        Service::Qobuz => {
            let endpoint = match link.kind {
                Kind::Song => format!("{}/track/get?track_id={}&app_id={}", bases.qobuz, link.id, crate::qobuz::QOBUZ_APP_ID),
                Kind::Album => format!("{}/album/get?album_id={}&app_id={}", bases.qobuz, link.id, crate::qobuz::QOBUZ_APP_ID),
                _ => format!("{}/playlist/get?playlist_id={}&app_id={}", bases.qobuz, link.id, crate::qobuz::QOBUZ_APP_ID),
            };
            let found = get_json(http, &endpoint, service).await?;
            qobuz_listing(&found, link.kind)?
        }
    };
    if listing.songs.is_empty() {
        return Err(format!("{} shows no songs there", service.name()));
    }
    // An album's songs in its order, of it, with its cover.
    if listing.kind == Kind::Album {
        let (title, cover) = (&listing.title, &listing.cover);
        for (at, song) in listing.songs.iter_mut().enumerate() {
            song.track = song.track.or(u32::try_from(at + 1).ok());
            song.album.get_or_insert_with(|| title.clone());
            if song.cover.is_none() {
                song.cover.clone_from(cover);
            }
        }
    }
    if let Some(file) = library {
        let keys: Vec<String> = listing.songs.iter().map(|song| song.key.clone()).collect();
        let owned = tokio::task::spawn_blocking(move || {
            let library = crate::music::read_library(&file);
            keys.iter().map(|key| library.get(key).is_some_and(|song| song.is_file())).collect::<Vec<bool>>()
        });
        for (song, owned) in listing.songs.iter_mut().zip(owned.await.map_err(|e| format!("Background task failed: {e}"))?) {
            song.owned = owned;
        }
    }
    Ok(listing)
}

/// What Spotify's player page (`/embed/<kind>/<id>`) shows of a song, album or playlist: its
/// `__NEXT_DATA__`'s entity, its songs' titles, artists and lengths. No more than
/// [`SPOTIFY_SHOWS`] songs of a playlist, which the listing then says.
fn spotify(html: &str, kind: Kind) -> Result<Listing, String> {
    let unreadable = || unreadable(Service::Spotify);
    let data: Value = script(html, "id=\"__NEXT_DATA__\"").and_then(|json| serde_json::from_str(json).ok()).ok_or_else(unreadable)?;
    let entity = &data["props"]["pageProps"]["state"]["data"]["entity"];
    let title = text(entity, "name").or_else(|| text(entity, "title")).ok_or_else(unreadable)?;
    let images = entity["visualIdentity"]["image"].as_array().into_iter().flatten();
    let cover = images.max_by_key(|image| image["maxWidth"].as_u64()).and_then(|image| text(image, "url"));
    let year = year_of(text(&entity["releaseDate"], "isoString"));
    let seconds = |item: &Value| item["duration"].as_f64().filter(|ms| *ms > 0.0).map(|ms| ms / 1000.0);
    let mut listing = Listing { service: Service::Spotify, kind, title: title.clone(), artist: None, year, cover: cover.clone(), songs: Vec::new(), unread: None };
    if kind == Kind::Song {
        let id = text(entity, "id").ok_or_else(unreadable)?;
        let artists: Vec<String> = entity["artists"].as_array().into_iter().flatten().filter_map(|artist| text(artist, "name")).collect();
        listing.artist = artists.first().cloned();
        listing.songs.push(Song { key: format!("spotify {id}"), title, artists, seconds: seconds(entity), cover, ..Default::default() });
        return Ok(listing);
    }
    let tracks = entity["trackList"].as_array().ok_or_else(unreadable)?;
    listing.artist = text(entity, "subtitle").filter(|_| kind == Kind::Album);
    for track in tracks {
        // A podcast's episode in a playlist is no song.
        let Some(id) = track["uri"].as_str().and_then(|uri| uri.strip_prefix("spotify:track:")) else { continue };
        let Some(title) = text(track, "title") else { continue };
        let artists = text(track, "subtitle").map(|by| by.split(", ").map(String::from).collect()).unwrap_or_default();
        listing.songs.push(Song { key: format!("spotify {id}"), title, artists, seconds: seconds(track), ..Default::default() });
    }
    if kind == Kind::Playlist && tracks.len() >= SPOTIFY_SHOWS {
        listing.unread = Some(format!("Spotify's page shows the first {} songs: the playlist may hold more", tracks.len()));
    }
    Ok(listing)
}

/// An Apple Music artist line ("A, B & C") as its artists.
fn apple_artists(by: &str) -> Vec<String> {
    by.split(", ").flat_map(|part| part.split(" & ")).map(str::trim).filter(|name| !name.is_empty()).map(String::from).collect()
}

/// An Apple Music artwork's square image, 1200 pixels a side.
fn artwork(value: &Value) -> Option<String> {
    let url = value["dictionary"]["url"].as_str()?;
    Some(url.replace("{w}", "1200").replace("{h}", "1200").replace("{f}", "jpg"))
}

/// What Apple Music's playlist page shows: its `serialized-server-data`'s header and track list
/// (title, artist, album, length, cover), and how many songs it has (its `ld+json`'s `numTracks`).
fn apple_playlist(html: &str) -> Result<Listing, String> {
    let unreadable = || unreadable(Service::Apple);
    let data: Value = script(html, "id=\"serialized-server-data\"").and_then(|json| serde_json::from_str(json).ok()).ok_or_else(unreadable)?;
    let sections = data["data"][0]["data"]["sections"].as_array().ok_or_else(unreadable)?;
    let section = |kind: &str| sections.iter().find(|section| section["itemKind"] == kind);
    let header = section("containerDetailHeaderLockup").map(|section| &section["items"][0]).ok_or_else(unreadable)?;
    let items = section("trackLockup").and_then(|section| section["items"].as_array()).ok_or_else(unreadable)?;
    let title = text(header, "title").ok_or_else(unreadable)?;
    let songs: Vec<Song> = items
        .iter()
        .filter_map(|item| {
            let id = item["contentDescriptor"]["identifiers"]["storeAdamID"].as_str()?;
            Some(Song {
                key: format!("apple {id}"),
                title: text(item, "title")?,
                artists: text(item, "artistName").map(|by| apple_artists(&by)).unwrap_or_default(),
                album: text(&item["tertiaryLinks"][0], "title"),
                seconds: item["duration"].as_f64().filter(|ms| *ms > 0.0).map(|ms| ms / 1000.0),
                cover: artwork(&item["artwork"]),
                ..Default::default()
            })
        })
        .collect();
    let described: Option<Value> = script(html, "application/ld+json").and_then(|json| serde_json::from_str(json).ok());
    let total = described.and_then(|ld| ld["numTracks"].as_u64()).map_or(0, |n| n as usize);
    let unread = (total > songs.len()).then(|| format!("Apple Music's page shows {} of its {total} songs", songs.len()));
    Ok(Listing { service: Service::Apple, kind: Kind::Playlist, title, artist: None, year: None, cover: artwork(&header["artwork"]), songs, unread })
}

/// What Apple's store tells of a song, or an album and its songs (`lookup?id=..&entity=song`).
fn itunes(found: &Value, kind: Kind) -> Result<Listing, String> {
    let results = found["results"].as_array().ok_or_else(|| unreadable(Service::Apple))?;
    let songs: Vec<Song> = results
        .iter()
        .filter(|result| result["kind"] == "song")
        .filter_map(|result| {
            Some(Song {
                key: format!("apple {}", result["trackId"].as_u64()?),
                title: text(result, "trackName")?,
                artists: text(result, "artistName").map(|by| apple_artists(&by)).unwrap_or_default(),
                album: text(result, "collectionName"),
                seconds: result["trackTimeMillis"].as_f64().map(|ms| ms / 1000.0),
                track: result["trackNumber"].as_u64().and_then(|n| u32::try_from(n).ok()),
                cover: text(result, "artworkUrl100").map(|url| url.replace("100x100bb", "1200x1200bb")),
                ..Default::default()
            })
        })
        .collect();
    let first = songs.first().ok_or_else(|| "Apple Music's store has no such song or album in that country".to_string())?;
    let album = results.iter().find(|result| result["wrapperType"] == "collection");
    let (title, artist, released) = match album.filter(|_| kind == Kind::Album) {
        Some(album) => (text(album, "collectionName"), text(album, "artistName"), text(album, "releaseDate")),
        None => (Some(first.title.clone()), first.artists.first().cloned(), text(&results[0], "releaseDate")),
    };
    let (title, cover) = (title.unwrap_or_else(|| first.title.clone()), first.cover.clone());
    Ok(Listing { service: Service::Apple, kind, title, artist, year: year_of(released), cover, songs, unread: None })
}

/// A song of Deezer's answers (a track's own, or one of a list's).
fn deezer_song(track: &Value) -> Option<Song> {
    let id = track["id"].as_u64()?;
    let mut artists: Vec<String> = text(&track["artist"], "name").into_iter().collect();
    for name in track["contributors"].as_array().into_iter().flatten().filter_map(|artist| text(artist, "name")) {
        if !artists.contains(&name) {
            artists.push(name);
        }
    }
    Some(Song {
        key: format!("deezer {id}"),
        title: text(track, "title")?,
        artists,
        album: text(&track["album"], "title"),
        seconds: track["duration"].as_f64().filter(|seconds| *seconds > 0.0),
        isrc: text(track, "isrc"),
        track: track["track_position"].as_u64().and_then(|n| u32::try_from(n).ok()),
        cover: text(&track["album"], "cover_xl"),
        ..Default::default()
    })
}

/// The songs of a page of Deezer's (`{"data": [...]}`).
fn deezer_songs(page: &Value) -> Vec<Song> {
    page["data"].as_array().into_iter().flatten().filter_map(deezer_song).collect()
}

/// What Deezer's API tells of a track, an album or a playlist, with the songs its first answer
/// holds.
fn deezer(found: &Value, kind: Kind) -> Result<Listing, String> {
    if let Some(error) = found["error"]["message"].as_str() {
        return Err(format!("Deezer answered: {error}"));
    }
    let title = text(found, "title").ok_or_else(|| unreadable(Service::Deezer))?;
    let year = year_of(text(found, "release_date").or_else(|| text(&found["album"], "release_date")));
    let mut listing = Listing { service: Service::Deezer, kind, title, artist: None, year, cover: None, songs: Vec::new(), unread: None };
    if kind == Kind::Song {
        let song = deezer_song(found).ok_or_else(|| unreadable(Service::Deezer))?;
        (listing.artist, listing.cover) = (song.artists.first().cloned(), song.cover.clone());
        listing.songs.push(song);
    } else {
        listing.artist = text(&found["artist"], "name").filter(|_| kind == Kind::Album);
        listing.cover = text(found, "cover_xl").or_else(|| text(found, "picture_xl"));
        listing.songs = deezer_songs(&found["tracks"]);
    }
    Ok(listing)
}

/// A song of Qobuz's answers.
fn qobuz_song(found: &Value) -> Option<Song> {
    let track = crate::qobuz::parse_track_json(found)?;
    Some(Song {
        key: format!("qobuz {}", track.id),
        title: track.title,
        artists: vec![track.artist],
        album: track.album,
        seconds: Some(track.duration).filter(|&s| s > 0.0),
        isrc: track.isrc,
        track: found["track_number"].as_u64().and_then(|n| u32::try_from(n).ok()),
        cover: track.cover,
        ..Default::default()
    })
}

/// What Qobuz's API tells of a track, an album or a playlist.
fn qobuz_listing(found: &Value, kind: Kind) -> Result<Listing, String> {
    if found["status"].as_str() == Some("error") {
        return Err(format!("Qobuz answered: {}", found["message"].as_str().unwrap_or("error")));
    }
    let title = text(found, "title").ok_or_else(|| unreadable(Service::Qobuz))?;
    let year = year_of(text(found, "release_date_original").or_else(|| text(found, "release_date_download")));
    let mut listing = Listing { service: Service::Qobuz, kind, title, artist: None, year, cover: None, songs: Vec::new(), unread: None };
    if kind == Kind::Song {
        let song = qobuz_song(found).ok_or_else(|| unreadable(Service::Qobuz))?;
        (listing.artist, listing.cover) = (song.artists.first().cloned(), song.cover.clone());
        listing.songs.push(song);
    } else {
        listing.artist = text(&found["artist"], "name").filter(|_| kind == Kind::Album);
        listing.cover = text(&found["image"], "large").or_else(|| text(&found["image"], "extralarge"));
        if let Some(items) = found["tracks"]["items"].as_array() {
            listing.songs = items.iter().filter_map(qobuz_song).collect();
        }
    }
    Ok(listing)
}

/// A song of YouTube Music's that a search found.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Its video id.
    pub id: String,
    pub title: String,
    pub artist: String,
    pub seconds: Option<f64>,
}

impl Found {
    /// Its link, a YouTube Music song's (or direct streaming link).
    pub fn url(&self) -> Url {
        if self.id.starts_with("http://") || self.id.starts_with("https://") {
            if let Ok(url) = Url::parse(&self.id) {
                return url;
            }
        }
        let mut url = Url::parse("https://music.youtube.com/watch").expect("a link");
        url.query_pairs_mut().append_pair("v", &self.id);
        url
    }

    /// Its line in the download archive.
    fn archive_line(&self) -> String {
        if self.id.starts_with("http://") || self.id.starts_with("https://") {
            format!("url {}", self.id)
        } else {
            format!("youtube {}", self.id)
        }
    }

    /// "Title by Artist (3:45)".
    pub fn said(&self) -> String {
        said(&self.title, &self.artist, self.seconds)
    }
}

/// How a song's search went.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// The same title by the same artist, its length within [`SURE_APART`] seconds, no other
    /// version of it.
    Sure(Found),
    /// The nearest that could be it, as unsure as the card says.
    Unsure(Found),
    NotFound,
}

/// A result further from a song's length than this, in seconds, is not it.
const MOST_APART: f64 = 30.0;

/// A sure match is within this many seconds of the song's length.
const SURE_APART: f64 = 5.0;

/// Words that name another version of a song, as `normalized` writes them, with the version they
/// name: a result whose title has one that the song's lacks is that version, not the song.
const VERSION_WORDS: &[(&str, &str)] = &[
    ("remix", "remix"),
    ("live", "live"),
    ("remaster", "remaster"),
    ("remastered", "remaster"),
    ("reverb", "reverb"),
    ("slowed", "slowed"),
    ("sped up", "sped up"),
    ("bass boosted", "bass boosted"),
    ("bassboosted", "bass boosted"),
    ("8d", "8d"),
    ("karaoke", "karaoke"),
    ("instrumental", "instrumental"),
    ("cover", "cover"),
    ("acoustic", "acoustic"),
    ("nightcore", "nightcore"),
];

/// `text` in lower case, its words alone, "&" as "and".
fn normalized(text: &str) -> String {
    let text = text.to_lowercase().replace('&', " and ");
    let words: String = text.chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    words.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A title without what is in brackets ("(feat. X)", "[Live]") or after " - " ("- Radio Edit"),
/// [`normalized`]; the whole title when that leaves nothing.
fn core_title(title: &str) -> String {
    let mut kept = String::new();
    let mut depth = 0usize;
    for c in title.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            c if depth == 0 => kept.push(c),
            _ => {}
        }
    }
    let kept = kept.split(" - ").next().unwrap_or_default();
    let core = normalized(kept);
    if core.is_empty() {
        normalized(title)
    } else {
        core
    }
}

/// How alike two texts are, from 0 to 1: one less their edit distance over the longer's length.
fn similarity(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1).min(row[j] + 1).min(diagonal + usize::from(ca != cb));
            diagonal = above;
        }
    }
    1.0 - row[b.len()] as f64 / longest as f64
}

/// The versions a title names (see [`VERSION_WORDS`]).
fn versions(title: &str) -> Vec<&'static str> {
    let padded = format!(" {} ", normalized(title));
    VERSION_WORDS.iter().filter(|(word, _)| padded.contains(&format!(" {word} "))).map(|(_, version)| *version).collect()
}

/// The artists of a result: its own, else its channel's (`<artist> - Topic`).
fn their_artists(info: &Value) -> Vec<String> {
    let mut names: Vec<String> = info["artists"].as_array().into_iter().flatten().filter_map(|name| name.as_str()).map(String::from).collect();
    if names.is_empty() {
        names.extend(text(info, "artist").into_iter().flat_map(|by| by.split(", ").map(String::from).collect::<Vec<_>>()));
    }
    for channel in ["channel", "uploader"].iter().filter_map(|key| text(info, key)) {
        names.push(channel.strip_suffix(" - Topic").unwrap_or(&channel).to_string());
    }
    names
}

/// A result rated as the match of a song.
struct Rated {
    found: Found,
    score: f64,
    sure: bool,
}

/// How well the result `info` (what the helper tool found) matches `song`, the way the most used
/// open matcher of such lists weighs it: None unless its title and the song's main artist agree
/// and its length is within [`MOST_APART`] seconds; then its title's likeness, its length's
/// nearness, a song (with an album) over a video, the album agreeing, and less for a version
/// (live, remix, ...) the song is not.
fn rate(song: &Song, info: &Value) -> Option<Rated> {
    let id = text(info, "id")?;
    let title = text(info, "track").or_else(|| text(info, "title"))?;
    let alike = similarity(&core_title(&song.title), &core_title(&title));
    if alike < 0.6 {
        return None;
    }
    let main = normalized(song.artists.first()?);
    let theirs = their_artists(info);
    let named = format!(" {} ", normalized(&format!("{} {title}", theirs.join(" "))));
    let artist_agrees = !main.is_empty() && (theirs.iter().any(|name| similarity(&main, &normalized(name)) >= 0.85) || named.contains(&format!(" {main} ")));
    if !artist_agrees {
        return None;
    }
    let seconds = info["duration"].as_f64();
    let apart = song.seconds.zip(seconds).map(|(wanted, theirs)| (wanted - theirs).abs());
    if apart.is_some_and(|apart| apart > MOST_APART) {
        return None;
    }
    let wanted = versions(&song.title);
    let other_version = versions(&title).iter().any(|version| !wanted.contains(version));
    let is_song = text(info, "album").is_some();
    let album_agrees = song.album.as_deref().zip(text(info, "album")).is_some_and(|(ours, theirs)| similarity(&core_title(ours), &core_title(&theirs)) >= 0.85);
    let near = apart.map_or(0.5, |apart| 1.0 - apart / MOST_APART);
    let score = 50.0 * alike + 30.0 * near + if is_song { 10.0 } else { 0.0 } + if album_agrees { 10.0 } else { 0.0 } - if other_version { 40.0 } else { 0.0 };
    let sure = alike >= 0.9 && apart.is_some_and(|apart| apart <= SURE_APART) && !other_version;
    let artist = theirs.into_iter().next().unwrap_or_default();
    Some(Rated { found: Found { id, title, artist, seconds }, score, sure })
}

/// Which of `results` (what the helper tool found) is `song`: the best rated (see [`rate`]),
/// sure or not; none when none agrees.
pub(crate) fn choose(song: &Song, results: &[Value]) -> Verdict {
    let mut best: Option<Rated> = None;
    for rated in results.iter().filter_map(|info| rate(song, info)) {
        if best.as_ref().is_none_or(|best| rated.score > best.score) {
            best = Some(rated);
        }
    }
    match best {
        Some(Rated { found, sure: true, .. }) => Verdict::Sure(found),
        Some(Rated { found, .. }) => Verdict::Unsure(found),
        None => Verdict::NotFound,
    }
}

/// A title without its featured artists ("(feat. X)", "[with X]"), which the artist finds anyway.
fn without_features(title: &str) -> String {
    let (mut kept, mut rest) = (String::new(), title);
    while let Some(open) = rest.find(['(', '[']) {
        let Some(close) = rest[open..].find([')', ']']).map(|close| open + close) else { break };
        let inner = rest[open + 1..close].trim().to_lowercase();
        kept.push_str(&rest[..open]);
        if !(inner.starts_with("feat") || inner.starts_with("ft.") || inner.starts_with("with ")) {
            kept.push_str(&rest[open..=close]);
        }
        rest = &rest[close + 1..];
    }
    kept.push_str(rest);
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a song is searched for by: its main artist and title.
fn query(song: &Song) -> String {
    let artist = song.artists.first().map_or("", String::as_str);
    format!("{artist} {}", without_features(&song.title)).trim().to_string()
}

/// YouTube Music's search of songs for `query`.
fn search_url(query: &str) -> Url {
    let mut url = Url::parse("https://music.youtube.com/search").expect("a link");
    url.query_pairs_mut().append_pair("q", query);
    url.set_fragment(Some("songs"));
    url
}

/// Songs looked for in one run of the helper tool (two when some are searched by their ISRC
/// first): a card shows how far a list is a batch at a time.
pub const BATCH: usize = 5;

/// Each of `songs` (a [`BATCH`] or so) found on YouTube Music through the helper tool's search,
/// read through `proxy`: sure, unsure or not found (see [`choose`]). Err when the helper tool
/// could not search.
pub async fn find(songs: &[Song], proxy: Option<&str>) -> Result<Vec<Verdict>, String> {
    find_with(songs, |urls| youtube_music(urls, proxy.map(String::from))).await
}

/// What the helper tool's searches `urls` find (see `media::search_music`), through `proxy`.
async fn youtube_music(urls: Vec<Url>, proxy: Option<String>) -> Result<Vec<Result<Vec<Value>, String>>, String> {
    use crate::quality::{AudioForm, MediaPick};
    let preset = crate::media::MediaQualityPreset::Pick(MediaPick { audio: Some(AudioForm::Original), ..Default::default() });
    let options = crate::media::MediaDownloadOptions { proxy, preset, ..Default::default() };
    crate::media::search_music(&urls, &options).await
}

/// [`find`] with `search`, which gives what the searches it is given find. A song's ISRC names
/// its recording: it is searched first where the service gives it, then, unless that found it
/// surely, its artist and title too.
async fn find_with<S, F>(songs: &[Song], search: S) -> Result<Vec<Verdict>, String>
where
    S: Fn(Vec<Url>) -> F,
    F: std::future::Future<Output = Result<Vec<Result<Vec<Value>, String>>, String>>,
{
    let first: Vec<Url> = songs.iter().map(|song| search_url(&song.isrc.clone().unwrap_or_else(|| query(song)))).collect();
    let mut results: Vec<Vec<Value>> = search(first).await?.into_iter().map(Result::unwrap_or_default).collect();
    results.resize(songs.len(), Vec::new());
    let mut verdicts: Vec<Verdict> = songs.iter().zip(&results).map(|(song, found)| choose(song, found)).collect();
    let again: Vec<usize> = (0..songs.len()).filter(|&at| songs[at].isrc.is_some() && !matches!(verdicts[at], Verdict::Sure(_))).collect();
    if !again.is_empty() {
        let found = search(again.iter().map(|&at| search_url(&query(&songs[at]))).collect()).await?;
        for (&at, found) in again.iter().zip(found) {
            results[at].extend(found.unwrap_or_default());
            verdicts[at] = choose(&songs[at], &results[at]);
        }
    }
    Ok(verdicts)
}

/// Attempts to find a bit-perfect lossless match for `song`.
/// Prioritizes Qobuz 24-bit Hi-Res audio (Tier 1), falling back to Deezer 16-bit FLAC (Tier 2).
pub async fn find_lossless(http: &reqwest::Client, song: &Song) -> Option<Found> {
    // Tier 1: Try Qobuz Hi-Res lossless (24-bit/96kHz - 192kHz) first via ISRC or search
    if let Some(isrc) = &song.isrc {
        if let Ok(Some(track)) = crate::qobuz::resolve_isrc(http, isrc).await {
            return Some(Found {
                id: format!("https://play.qobuz.com/track/{}", track.id),
                title: track.title,
                artist: track.artist,
                seconds: Some(track.duration),
            });
        }
    }
    let artist = song.artists.first().map(String::as_str).unwrap_or("");
    if let Ok(Some(track)) = crate::qobuz::search_track(http, artist, &song.title).await {
        let alike = similarity(&core_title(&song.title), &core_title(&track.title));
        let apart = song.seconds.map(|s| (s - track.duration).abs());
        if alike >= 0.85 && apart.is_none_or(|a| a <= SURE_APART) {
            return Some(Found {
                id: format!("https://play.qobuz.com/track/{}", track.id),
                title: track.title,
                artist: track.artist,
                seconds: Some(track.duration),
            });
        }
    }

    // Tier 2: Fall back to Deezer 16-bit lossless FLAC
    if let Some(isrc) = &song.isrc {
        if let Ok(Some(track)) = crate::deezer::resolve_isrc(http, isrc).await {
            return Some(Found {
                id: format!("https://www.deezer.com/track/{}", track.id),
                title: track.title,
                artist: track.artist,
                seconds: Some(track.duration),
            });
        }
    }
    if let Ok(Some(track)) = crate::deezer::search_track(http, artist, &song.title).await {
        let alike = similarity(&core_title(&song.title), &core_title(&track.title));
        let apart = song.seconds.map(|s| (s - track.duration).abs());
        if alike >= 0.85 && apart.is_none_or(|a| a <= SURE_APART) {
            return Some(Found {
                id: format!("https://www.deezer.com/track/{}", track.id),
                title: track.title,
                artist: track.artist,
                seconds: Some(track.duration),
            });
        }
    }
    None
}

/// The downloads of the songs `picks` chose of `listing` (each its place in the list and the
/// song found for it), into the music library as its own songs are, each placed as the list says
/// (an album's artist, year and numbers; its cover); a playlist also writes its `.m3u8` in
/// `save_dir`, in its order, naming the songs the library holds. A song found on YouTube Music
/// that the library holds already is not downloaded again: the library knows it by this song too.
pub async fn tasks(listing: &Listing, picks: Vec<(usize, Found)>, save_dir: Option<PathBuf>) -> Result<Vec<Task>, String> {
    tasks_in(listing, picks, save_dir, crate::music::library_file()).await
}

/// [`tasks`] with the library index `library`.
async fn tasks_in(listing: &Listing, picks: Vec<(usize, Found)>, save_dir: Option<PathBuf>, library: Option<PathBuf>) -> Result<Vec<Task>, String> {
    let name = crate::ingest::clean_path([listing.title.as_str()]).ok().filter(|_| listing.kind == Kind::Playlist);
    let playlist = save_dir.zip(name).map(|(dir, name)| dir.join(format!("{}.m3u8", name.display())));
    let listing = listing.clone();
    let made = tokio::task::spawn_blocking(move || {
        // Its songs of an earlier listing may be finishing (see `media::song_downloaded`).
        let _lock = library.as_deref().map(crate::history::lock_file).transpose().map_err(|e| format!("Cannot open the music library: {e}"))?;
        let mut have = library.as_deref().map(crate::music::read_library).unwrap_or_default();
        let mut tasks = Vec::new();
        for (at, found) in picks {
            let Some(song) = listing.songs.get(at) else { continue };
            if let Some(file) = have.get(&found.archive_line()).filter(|file| file.is_file()).cloned() {
                if let Some(library) = &library {
                    crate::music::record_library(library, std::slice::from_ref(&song.key), &file).map_err(|e| format!("Cannot write the music library: {e}"))?;
                }
                have.insert(song.key.clone(), file);
                continue;
            }
            let music = placed(&listing, song, playlist.clone());
            tasks.push(Task { urls: vec![found.url()], from_document: true, music: Some(music), ..Default::default() });
        }
        if let Some(file) = &playlist {
            let keys: Vec<String> = listing.songs.iter().map(|song| song.key.clone()).collect();
            crate::music::write_playlist(file, &listing.title, &keys, &have).map_err(|e| format!("Cannot write the playlist {}: {e}", file.display()))?;
        }
        Ok(tasks)
    });
    made.await.map_err(|e| format!("Background task failed: {e}"))?
}

/// Where and as what `song` of `listing` goes in the library: an album's songs and a song in
/// their artist's folder as the service names them, a playlist's as YouTube Music has them.
fn placed(listing: &Listing, song: &Song, playlist: Option<PathBuf>) -> Music {
    let mut music = Music { track: song.track, album: song.album.clone(), cover: song.cover.clone(), key: Some(song.key.clone()), playlist, ..Default::default() };
    if listing.kind != Kind::Playlist {
        (music.album_artist, music.year) = (listing.artist.clone(), listing.year);
    }
    music
}

/// The downloads of the Spotify, Apple Music or Deezer link `url` read with no card to tick its
/// songs on (a queue line, the command line): its songs found on YouTube Music a [`BATCH`] at a
/// time, as far as it is logged; the sure ones downloaded, the unsure ones too when
/// `options.take_unsure` says so; each unsure or not found, and what its page did not show, told
/// in `options`' notes. The songs the library holds are neither looked for nor downloaded again.
pub async fn list(http: &reqwest::Client, url: &Url, options: &ListOptions) -> Result<Vec<Task>, String> {
    let proxy = options.proxy.clone();
    let search = |urls: Vec<Url>| youtube_music(urls, proxy.clone());
    list_with(http, url, options, &Bases::default(), crate::music::library_file(), search).await
}

/// [`list`] from the services at `bases`, with the library index `library`, searching with `search`.
async fn list_with<S, F>(http: &reqwest::Client, url: &Url, options: &ListOptions, bases: &Bases, library: Option<PathBuf>, search: S) -> Result<Vec<Task>, String>
where
    S: Fn(Vec<Url>) -> F,
    F: std::future::Future<Output = Result<Vec<Result<Vec<Value>, String>>, String>>,
{
    let listing = read_from(http, url, bases, library.clone()).await?;
    let wanted: Vec<usize> = (0..listing.songs.len()).filter(|&at| !listing.songs[at].owned).collect();
    let mut verdicts: Vec<Option<Verdict>> = vec![None; listing.songs.len()];
    let mut looked = 0;
    for batch in wanted.chunks(BATCH) {
        let songs: Vec<Song> = batch.iter().map(|&at| listing.songs[at].clone()).collect();
        for (&at, verdict) in batch.iter().zip(find_with(&songs, &search).await?) {
            verdicts[at] = Some(verdict);
        }
        looked += batch.len();
        tracing::info!("Looked for {looked} of {} songs of {} on YouTube Music", wanted.len(), listing.title);
    }
    if let Some(unread) = &listing.unread {
        options.note(unread.clone());
    }
    let mut picks = Vec::new();
    for (at, verdict) in verdicts.into_iter().enumerate() {
        let song = listing.songs[at].said();
        match verdict {
            Some(Verdict::Sure(found)) => picks.push((at, found)),
            Some(Verdict::Unsure(found)) if options.take_unsure => {
                options.note(format!("Taken though unsure: {song} as {}", found.said()));
                picks.push((at, found));
            }
            Some(Verdict::Unsure(found)) => options.note(format!("Left out as unsure: {song}, nearest {}", found.said())),
            Some(Verdict::NotFound) => options.note(format!("Not found on YouTube Music: {song}")),
            None => {}
        }
    }
    tasks_in(&listing, picks, options.save_dir.clone(), library).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    /// Spotify's player page with `entity` in its `__NEXT_DATA__`, as its real pages have it.
    fn spotify_page(entity: Value) -> String {
        let data = json!({"props": {"pageProps": {"state": {"data": {"entity": entity}, "settings": {}}}}});
        format!("<!DOCTYPE html><html><head></head><body><script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script></body></html>")
    }

    /// A song of a Spotify page's track list.
    fn spotify_track(n: usize) -> Value {
        json!({"uri": format!("spotify:track:track{n:04}"), "title": format!("Song {n}"), "subtitle": "Sample Artist, Guest", "duration": 200_000 + n})
    }

    /// Apple Music's playlist page of `items`, whose description says it has `total` songs.
    fn apple_page(items: Vec<Value>, total: usize) -> String {
        let data = json!({"data": [{"data": {"sections": [
            {"itemKind": "containerDetailHeaderLockup", "items": [{"title": "Sample Mix", "artwork": {"dictionary": {"url": "https://art.test/mix/{w}x{h}bb.{f}"}}}]},
            {"itemKind": "trackLockup", "items": items}
        ]}}]});
        let described = json!({"@type": "MusicPlaylist", "name": "Sample Mix", "numTracks": total});
        format!(
            "<html><head><script id=schema:music-playlist type=\"application/ld+json\">{described}</script></head>\
             <body><script type=\"application/json\" id=\"serialized-server-data\">{data}</script></body></html>"
        )
    }

    /// A song of Apple Music's playlist page.
    fn apple_item(id: u64, title: &str, by: &str, album: &str, ms: u64) -> Value {
        json!({
            "title": title, "artistName": by, "duration": ms, "tertiaryLinks": [{"title": album}],
            "contentDescriptor": {"kind": "song", "identifiers": {"storeAdamID": id.to_string()}},
            "artwork": {"dictionary": {"url": format!("https://art.test/{id}/{{w}}x{{h}}bb.{{f}}")}}
        })
    }

    /// Apple's store's answer for an album and its songs.
    fn lookup() -> Value {
        json!({"resultCount": 3, "results": [
            {"wrapperType": "collection", "collectionType": "Album", "artistName": "Sample Artist", "collectionName": "Sample Album",
             "releaseDate": "2001-03-12T08:00:00Z", "artworkUrl100": "https://art.test/a/100x100bb.jpg"},
            {"wrapperType": "track", "kind": "song", "trackId": 21, "artistName": "Sample Artist", "collectionName": "Sample Album",
             "trackName": "First Song", "trackNumber": 1, "trackTimeMillis": 320_357, "releaseDate": "2000-11-30T08:00:00Z",
             "artworkUrl100": "https://art.test/a/100x100bb.jpg"},
            {"wrapperType": "track", "kind": "song", "trackId": 22, "artistName": "Sample Artist", "collectionName": "Sample Album",
             "trackName": "Second Song", "trackNumber": 2, "trackTimeMillis": 212_000, "artworkUrl100": "https://art.test/a/100x100bb.jpg"}
        ]})
    }

    /// A song of Deezer's answers.
    fn deezer_track(id: u64, title: &str, isrc: &str, seconds: u64) -> Value {
        json!({"id": id, "title": title, "isrc": isrc, "duration": seconds, "artist": {"name": "Sample Artist"},
               "album": {"title": "Sample Album", "cover_xl": format!("https://covers.test/{id}/1000x1000.jpg")}})
    }

    /// What the helper tool finds at a YouTube Music song.
    fn result(id: &str, title: &str, artist: &str, album: Option<&str>, seconds: f64) -> Value {
        json!({"id": id, "title": title, "track": title, "artists": [artist], "album": album, "duration": seconds, "channel": format!("{artist} - Topic")})
    }

    /// The stand-in services at a local server's `base`.
    fn bases(base: &str) -> Bases {
        Bases { spotify: base.to_string(), apple: base.to_string(), itunes: base.to_string(), deezer: base.to_string(), qobuz: base.to_string() }
    }

    /// The query of each of YouTube Music's searches `urls`.
    fn queries(urls: &[Url]) -> Vec<String> {
        urls.iter().map(|url| url.query_pairs().find(|(key, _)| key == "q").unwrap().1.into_owned()).collect()
    }

    #[test]
    fn spotify_apple_music_and_deezer_links_are_told_apart() {
        let named = |text: &str| link(&url(text)).map(|link| (link.service, link.kind, link.id, link.country));
        let spotify = |kind, id: &str| Some((Service::Spotify, kind, id.to_string(), String::new()));
        assert_eq!(named("https://open.spotify.com/track/track0001"), spotify(Kind::Song, "track0001"));
        assert_eq!(named("https://open.spotify.com/intl-de/album/album0001?si=abc"), spotify(Kind::Album, "album0001"));
        assert_eq!(named("https://open.spotify.com/embed/playlist/mix0001"), spotify(Kind::Playlist, "mix0001"));
        let apple = |kind, id: &str| Some((Service::Apple, kind, id.to_string(), "us".to_string()));
        assert_eq!(named("https://music.apple.com/us/album/sample-album/20"), apple(Kind::Album, "20"));
        assert_eq!(named("https://music.apple.com/us/album/first-song/20?i=21"), apple(Kind::Song, "21"));
        assert_eq!(named("https://music.apple.com/us/song/first-song/21"), apple(Kind::Song, "21"));
        assert_eq!(named("https://music.apple.com/us/playlist/sample-mix/pl.sample"), apple(Kind::Playlist, "pl.sample"));
        let deezer = |kind, id: &str| Some((Service::Deezer, kind, id.to_string(), String::new()));
        assert_eq!(named("https://www.deezer.com/track/3"), deezer(Kind::Song, "3"));
        assert_eq!(named("https://www.deezer.com/fr/album/8"), deezer(Kind::Album, "8"));
        assert_eq!(named("https://deezer.com/playlist/7"), deezer(Kind::Playlist, "7"));
        let qobuz = |kind, id: &str| Some((Service::Qobuz, kind, id.to_string(), String::new()));
        assert_eq!(named("https://play.qobuz.com/track/12345678"), qobuz(Kind::Song, "12345678"));
        assert_eq!(named("https://open.qobuz.com/album/abcdef"), qobuz(Kind::Album, "abcdef"));
        assert_eq!(named("https://www.qobuz.com/us-en/album/album-title/1234567"), qobuz(Kind::Album, "1234567"));
        for other in [
            "https://open.spotify.com/artist/artist0001",
            "https://open.spotify.com/",
            "https://music.apple.com/us/artist/sample-artist/5",
            "https://music.apple.com/us/browse",
            "https://www.deezer.com/artist/27",
            "https://www.deezer.com/album/abc",
            "https://music.youtube.com/watch?v=aaaaaaaaaa1",
        ] {
            assert_eq!(named(other), None, "{other}");
        }
        assert!(lists(&url("https://www.deezer.com/album/8")) && !lists(&url("https://www.deezer.com/")));
    }

    #[test]
    fn spotifys_player_page_shows_its_songs() {
        let images = json!({"image": [{"url": "https://img.test/300", "maxWidth": 300}, {"url": "https://img.test/640", "maxWidth": 640}, {"url": "https://img.test/64", "maxWidth": 64}]});
        let mut tracks: Vec<Value> = (1..=3).map(spotify_track).collect();
        tracks.insert(1, json!({"uri": "spotify:episode:episode0001", "title": "An Episode", "subtitle": "A Show", "duration": 1_000_000}));
        let album = spotify_page(json!({"type": "album", "name": "Sample Album", "subtitle": "Sample Artist", "releaseDate": null, "trackList": tracks, "visualIdentity": images}));
        let listing = spotify(&album, Kind::Album).unwrap();
        let about = (listing.title.as_str(), listing.artist.as_deref(), listing.cover.as_deref(), listing.unread.as_deref());
        assert_eq!(about, ("Sample Album", Some("Sample Artist"), Some("https://img.test/640"), None));
        // Its songs, not a podcast's episode.
        assert_eq!(listing.songs.iter().map(|song| song.key.as_str()).collect::<Vec<_>>(), ["spotify track0001", "spotify track0002", "spotify track0003"]);
        let artists = vec!["Sample Artist".to_string(), "Guest".to_string()];
        assert_eq!(listing.songs[0], Song { key: "spotify track0001".into(), title: "Song 1".into(), artists, seconds: Some(200.001), ..Default::default() });
        // A playlist of as many songs as the page shows may hold more, which it says.
        let playlist = spotify_page(json!({"type": "playlist", "name": "Sample Mix", "subtitle": "Someone", "trackList": (1..=100).map(spotify_track).collect::<Vec<_>>(), "visualIdentity": images}));
        let listing = spotify(&playlist, Kind::Playlist).unwrap();
        let told = Some("Spotify's page shows the first 100 songs: the playlist may hold more");
        assert_eq!((listing.songs.len(), listing.artist.as_deref(), listing.unread.as_deref()), (100, None, told));
        // A song, the year it came out.
        let song = spotify_page(json!({
            "type": "track", "id": "track0009", "name": "Song 9", "artists": [{"name": "Sample Artist"}],
            "releaseDate": {"isoString": "1987-11-12T00:00:00Z"}, "duration": 213_573, "visualIdentity": images
        }));
        let listing = spotify(&song, Kind::Song).unwrap();
        let song = &listing.songs[0];
        assert_eq!((listing.year, listing.artist.as_deref(), song.key.as_str(), song.cover.as_deref()), (Some(1987), Some("Sample Artist"), "spotify track0009", Some("https://img.test/640")));
        // A page laid out otherwise says so, and guesses nothing.
        for page in ["<html><body>Sign in</body></html>".to_string(), spotify_page(json!({"type": "album"}))] {
            assert_eq!(spotify(&page, Kind::Album), Err("Spotify's page could not be read: its layout may have changed".to_string()));
        }
    }

    #[test]
    fn apple_musics_playlist_page_and_store_show_their_songs() {
        let items = vec![apple_item(11, "First Song", "Sample Artist", "Sample Album", 209_680), apple_item(12, "Second Song", "Sample Artist & Guest", "Other Album", 180_000)];
        let listing = apple_playlist(&apple_page(items, 250)).unwrap();
        assert_eq!((listing.title.as_str(), listing.kind, listing.unread.as_deref()), ("Sample Mix", Kind::Playlist, Some("Apple Music's page shows 2 of its 250 songs")));
        let want = Song {
            key: "apple 12".into(),
            title: "Second Song".into(),
            artists: vec!["Sample Artist".into(), "Guest".into()],
            album: Some("Other Album".into()),
            seconds: Some(180.0),
            cover: Some("https://art.test/12/1200x1200bb.jpg".into()),
            ..Default::default()
        };
        assert_eq!(listing.songs[1], want);
        assert_eq!(apple_playlist(&apple_page(vec![apple_item(11, "First Song", "Sample Artist", "Sample Album", 209_680)], 1)).unwrap().unread, None);
        assert_eq!(apple_playlist("<html></html>").map(|_| ()), Err(unreadable(Service::Apple)));
        // An album from the store: its songs with their numbers, its artist and year, its cover large.
        let album = itunes(&lookup(), Kind::Album).unwrap();
        let about = (album.title.as_str(), album.artist.as_deref(), album.year, album.cover.as_deref());
        assert_eq!(about, ("Sample Album", Some("Sample Artist"), Some(2001), Some("https://art.test/a/1200x1200bb.jpg")));
        assert_eq!(album.songs.iter().map(|song| (song.key.as_str(), song.track)).collect::<Vec<_>>(), [("apple 21", Some(1)), ("apple 22", Some(2))]);
        // A song alone.
        let song = json!({"resultCount": 1, "results": [lookup()["results"][1].clone()]});
        let listing = itunes(&song, Kind::Song).unwrap();
        assert_eq!((listing.title.as_str(), listing.year, listing.songs.len()), ("First Song", Some(2000), 1));
        assert!(itunes(&json!({"resultCount": 0, "results": []}), Kind::Album).is_err());
    }

    /// Each service read from a stand-in of it: Spotify's player page, Apple Music's playlist page
    /// and its store's lookup, Deezer's API a page at a time; the songs the library holds told.
    #[tokio::test]
    async fn each_service_is_read_from_what_it_shows_anyone() {
        let album = spotify_page(json!({
            "type": "album", "name": "Sample Album", "subtitle": "Sample Artist", "trackList": (1..=2).map(spotify_track).collect::<Vec<_>>(),
            "visualIdentity": {"image": [{"url": "https://img.test/640", "maxWidth": 640}]}
        }));
        let apple = apple_page(vec![apple_item(11, "First Song", "Sample Artist", "Sample Album", 209_680)], 1);
        let tracks = [deezer_track(1, "First Song", "AAA000000001", 200), deezer_track(2, "Second Song", "AAA000000002", 180)];
        let mix = json!({"id": 7, "title": "Sample Mix", "nb_tracks": 3, "picture_xl": "https://covers.test/mix.jpg", "tracks": {"data": tracks}});
        let rest = json!({"data": [deezer_track(3, "Third Song", "AAA000000003", 240)]});
        let (addr, hits) = crate::hls::tests::serve(move |path, _| {
            let body = match path {
                "/embed/album/album0001" => album.clone(),
                "/us/playlist/sample-mix/pl.sample" => apple.clone(),
                "/lookup?id=20&country=us&entity=song" => lookup().to_string(),
                "/playlist/7" => mix.to_string(),
                "/playlist/7/tracks?index=2&limit=100" => rest.to_string(),
                "/album/8" => json!({"error": {"type": "DataException", "message": "no data", "code": 800}}).to_string(),
                _ => return (404, String::new(), Vec::new()),
            };
            (200, String::new(), body.into_bytes())
        })
        .await;
        let bases = bases(&format!("http://{addr}"));
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library.txt");
        let file = dir.path().join("Second Song.opus");
        std::fs::write(&file, "audio").unwrap();
        crate::music::record_library(&library, &["deezer 2".to_string()], &file).unwrap();
        let http = reqwest::Client::new();
        let read = |link: &str| {
            let (link, http, bases, library) = (url(link), &http, &bases, library.clone());
            async move { read_from(http, &link, bases, Some(library)).await }
        };
        // Spotify's album: its songs numbered in its order, of it, with its cover.
        let listing = read("https://open.spotify.com/album/album0001").await.unwrap();
        let placed: Vec<_> = listing.songs.iter().map(|song| (song.track, song.album.as_deref(), song.cover.as_deref())).collect();
        assert_eq!(placed, [(Some(1), Some("Sample Album"), Some("https://img.test/640")), (Some(2), Some("Sample Album"), Some("https://img.test/640"))]);
        // Apple Music's playlist from its page, an album from its store.
        assert_eq!(read("https://music.apple.com/us/playlist/sample-mix/pl.sample").await.unwrap().songs[0].key, "apple 11");
        assert_eq!(read("https://music.apple.com/us/album/sample-album/20").await.unwrap().songs.len(), 2);
        // Deezer's playlist, a page at a time, each song with its ISRC; the one the library holds told.
        let listing = read("https://www.deezer.com/playlist/7").await.unwrap();
        let songs: Vec<_> = listing.songs.iter().map(|song| (song.key.as_str(), song.isrc.as_deref(), song.owned)).collect();
        assert_eq!(songs, [("deezer 1", Some("AAA000000001"), false), ("deezer 2", Some("AAA000000002"), true), ("deezer 3", Some("AAA000000003"), false)]);
        assert_eq!(listing.unread, None);
        assert_eq!(hits.lock().get("/playlist/7/tracks?index=2&limit=100"), Some(&1));
        // What it cannot read it says.
        assert_eq!(read("https://www.deezer.com/album/8").await, Err("Deezer answered: no data".to_string()));
        assert_eq!(read("https://open.spotify.com/playlist/gone0001").await, Err("Spotify answered HTTP 404".to_string()));
    }

    #[test]
    fn the_match_is_the_same_song_by_its_artist_at_its_length() {
        let song = Song {
            key: "deezer 1".into(),
            title: "First Song (feat. Guest)".into(),
            artists: vec!["Sample Artist".into(), "Guest".into()],
            album: Some("Sample Album".into()),
            seconds: Some(200.0),
            ..Default::default()
        };
        let pick = |results: Vec<Value>| match choose(&song, &results) {
            Verdict::Sure(found) => format!("sure {}", found.id),
            Verdict::Unsure(found) => format!("unsure {}", found.id),
            Verdict::NotFound => "none".to_string(),
        };
        // Its title without the featured artist, its artist, its length: sure.
        assert_eq!(pick(vec![result("a1", "First Song", "Sample Artist", Some("Sample Album"), 201.0)]), "sure a1");
        // A live take loses to the song itself, though nearer in length.
        let live = result("b1", "First Song (Live)", "Sample Artist", Some("Live at Home"), 200.0);
        assert_eq!(pick(vec![live, result("b2", "First Song", "Sample Artist", Some("Sample Album"), 203.0)]), "sure b2");
        // Only another version, or its length 8 s off: unsure.
        assert_eq!(pick(vec![result("c1", "First Song (Slowed + Reverb)", "Sample Artist", None, 200.0)]), "unsure c1");
        assert_eq!(pick(vec![result("c2", "First Song", "Sample Artist", Some("Sample Album"), 208.0)]), "unsure c2");
        // Another artist's, another song, one far longer: none.
        assert_eq!(pick(vec![result("d1", "First Song", "Other Singer", Some("Sample Album"), 200.0)]), "none");
        assert_eq!(pick(vec![result("d2", "Something Else Entirely", "Sample Artist", Some("Sample Album"), 200.0)]), "none");
        assert_eq!(pick(vec![result("d3", "First Song", "Sample Artist", Some("Sample Album"), 245.0)]), "none");
        assert_eq!(pick(Vec::new()), "none");
        // A song over a video of it; the album agreeing over another album.
        let mut video = result("e1", "First Song", "Sample Artist", None, 200.0);
        video["channel"] = "Sample Artist".into();
        assert_eq!(pick(vec![video, result("e2", "First Song", "Sample Artist", Some("Hits"), 200.0)]), "sure e2");
        assert_eq!(pick(vec![result("f1", "First Song", "Sample Artist", Some("Hits"), 200.0), result("f2", "First Song", "Sample Artist", Some("Sample Album"), 200.0)]), "sure f2");
        // The found song as the card tells it.
        let found = Found { id: "a1".into(), title: "First Song".into(), artist: "Sample Artist".into(), seconds: Some(201.4) };
        assert_eq!((found.said(), found.url().as_str()), ("First Song by Sample Artist (3:21)".to_string(), "https://music.youtube.com/watch?v=a1"));
    }

    #[tokio::test]
    async fn a_song_is_searched_by_its_isrc_first_then_by_artist_and_title() {
        let song = |key: &str, title: &str, seconds: f64, isrc: Option<&str>| Song {
            key: key.into(),
            title: title.into(),
            artists: vec!["Sample Artist".into()],
            seconds: Some(seconds),
            isrc: isrc.map(String::from),
            ..Default::default()
        };
        let songs = [song("deezer 1", "First Song", 200.0, Some("AAA000000001")), song("deezer 2", "Second Song (feat. Guest)", 180.0, Some("AAA000000002")), song("spotify t3", "Third Song", 240.0, None)];
        let asked = std::sync::Mutex::new(Vec::new());
        let search = |urls: Vec<Url>| {
            let queries = queries(&urls);
            asked.lock().unwrap().push(queries.clone());
            let found: Vec<Result<Vec<Value>, String>> = queries
                .iter()
                .map(|query| {
                    Ok(match query.as_str() {
                        "AAA000000001" => vec![result("a1", "First Song", "Sample Artist", Some("Sample Album"), 200.0)],
                        "Sample Artist Second Song" => vec![result("b1", "Second Song", "Sample Artist", Some("Sample Album"), 181.0)],
                        "Sample Artist Third Song" => vec![result("c1", "Third Song (Live)", "Sample Artist", None, 240.0)],
                        _ => Vec::new(),
                    })
                })
                .collect();
            std::future::ready(Ok(found))
        };
        let verdicts = find_with(&songs, search).await.unwrap();
        let ids: Vec<String> = verdicts
            .iter()
            .map(|verdict| match verdict {
                Verdict::Sure(found) => format!("sure {}", found.id),
                Verdict::Unsure(found) => format!("unsure {}", found.id),
                Verdict::NotFound => "none".to_string(),
            })
            .collect();
        assert_eq!(ids, ["sure a1", "sure b1", "unsure c1"]);
        // The ISRC where known, else artist and title; then the artist and title (without the
        // featured artist) of the song its ISRC did not find.
        assert_eq!(*asked.lock().unwrap(), [vec!["AAA000000001", "AAA000000002", "Sample Artist Third Song"], vec!["Sample Artist Second Song"]]);
        assert_eq!(search_url("Sample Artist First Song").as_str(), "https://music.youtube.com/search?q=Sample+Artist+First+Song#songs");
    }

    /// A list read with no card to tick on (a queue line, the command line).
    #[tokio::test]
    async fn a_list_brings_its_sure_songs_tells_of_the_others_and_later_only_new_ones() {
        let tracks = [deezer_track(1, "First Song", "AAA000000001", 200), deezer_track(2, "Second Song", "AAA000000002", 180), deezer_track(3, "Third Song", "AAA000000003", 240)];
        let mix = json!({"id": 7, "title": "Sample Mix", "nb_tracks": 3, "tracks": {"data": tracks}});
        let (addr, _) = crate::hls::tests::serve(move |path, _| match path {
            "/playlist/7" => (200, String::new(), mix.to_string().into_bytes()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let bases = bases(&format!("http://{addr}"));
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library.txt");
        let asked = std::sync::Mutex::new(Vec::new());
        let search = |urls: Vec<Url>| {
            let queries = queries(&urls);
            asked.lock().unwrap().extend(queries.clone());
            let found: Vec<Result<Vec<Value>, String>> = queries
                .iter()
                .map(|query| {
                    Ok(match query.as_str() {
                        "AAA000000001" => vec![result("a1", "First Song", "Sample Artist", Some("Sample Album"), 200.0)],
                        "AAA000000002" | "Sample Artist Second Song" => vec![result("b1", "Second Song (Remix)", "Sample Artist", None, 180.0)],
                        _ => Vec::new(),
                    })
                })
                .collect();
            std::future::ready(Ok(found))
        };
        let http = reqwest::Client::new();
        let link = url("https://www.deezer.com/playlist/7");
        let (notes, noted) = std::sync::mpsc::channel();
        let options = ListOptions { notes: Some(notes), save_dir: Some(dir.path().to_path_buf()), ..Default::default() };
        let tasks = list_with(&http, &link, &options, &bases, Some(library.clone()), &search).await.unwrap();
        // The sure song alone, placed as Deezer has it, with its album's cover, in the playlist.
        let playlist = dir.path().join("Sample Mix.m3u8");
        assert_eq!(tasks.iter().map(|task| task.urls[0].as_str()).collect::<Vec<_>>(), ["https://music.youtube.com/watch?v=a1"]);
        let want = Music {
            album: Some("Sample Album".into()),
            cover: Some("https://covers.test/1/1000x1000.jpg".into()),
            key: Some("deezer 1".into()),
            playlist: Some(playlist.clone()),
            ..Default::default()
        };
        assert_eq!(tasks[0].music, Some(want));
        // The others told, none left out silently.
        let told: Vec<String> = noted.try_iter().map(|note| note.text).collect();
        let unsure = "Left out as unsure: Second Song by Sample Artist (3:00), nearest Second Song (Remix) by Sample Artist (3:00)";
        assert_eq!(told, [unsure, "Not found on YouTube Music: Third Song by Sample Artist (4:00)"]);
        assert_eq!(std::fs::read_to_string(&playlist).unwrap(), "#EXTM3U\n#PLAYLIST:Sample Mix\n# deezer 1\n# deezer 2\n# deezer 3\n");
        // Asked to, the unsure ones too.
        let options = ListOptions { take_unsure: true, save_dir: Some(dir.path().to_path_buf()), ..Default::default() };
        let tasks = list_with(&http, &link, &options, &bases, Some(library.clone()), &search).await.unwrap();
        assert_eq!(tasks.iter().map(|task| task.urls[0].as_str()).collect::<Vec<_>>(), ["https://music.youtube.com/watch?v=a1", "https://music.youtube.com/watch?v=b1"]);
        // Once the first is in the library, the list again looks for and brings the others alone,
        // and its playlist names it.
        let first = dir.path().join("First Song.opus");
        std::fs::write(&first, "audio").unwrap();
        crate::music::record_library(&library, &["youtube a1".to_string(), "deezer 1".to_string()], &first).unwrap();
        asked.lock().unwrap().clear();
        let tasks = list_with(&http, &link, &options, &bases, Some(library.clone()), &search).await.unwrap();
        assert_eq!(tasks.iter().map(|task| task.urls[0].as_str()).collect::<Vec<_>>(), ["https://music.youtube.com/watch?v=b1"]);
        assert!(!asked.lock().unwrap().contains(&"AAA000000001".to_string()), "{:?}", asked.lock().unwrap());
        assert!(std::fs::read_to_string(&playlist).unwrap().contains("# deezer 1\nFirst Song.opus\n# deezer 2\n"));
    }

    #[tokio::test]
    async fn an_albums_songs_go_in_its_folder_and_one_downloaded_before_is_not_again() {
        let song = |n: u32| Song {
            key: format!("spotify t{n}"),
            title: format!("Song {n}"),
            artists: vec!["Sample Artist".into()],
            album: Some("Sample Album".into()),
            track: Some(n),
            cover: Some("https://img.test/640".into()),
            ..Default::default()
        };
        let listing = Listing {
            service: Service::Spotify,
            kind: Kind::Album,
            title: "Sample Album".into(),
            artist: Some("Sample Artist".into()),
            year: Some(2001),
            cover: Some("https://img.test/640".into()),
            songs: vec![song(1), song(2)],
            unread: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library.txt");
        let before = dir.path().join("Song 2.opus");
        std::fs::write(&before, "audio").unwrap();
        crate::music::record_library(&library, &["youtube b1".to_string()], &before).unwrap();
        let found = |id: &str| Found { id: id.into(), title: "Song".into(), artist: "Sample Artist".into(), seconds: None };
        let picks = vec![(0, found("a1")), (1, found("b1"))];
        let tasks = tasks_in(&listing, picks, Some(dir.path().to_path_buf()), Some(library.clone())).await.unwrap();
        let want = Music {
            track: Some(1),
            album: Some("Sample Album".into()),
            album_artist: Some("Sample Artist".into()),
            year: Some(2001),
            cover: Some("https://img.test/640".into()),
            key: Some("spotify t1".into()),
            playlist: None,
        };
        assert_eq!(tasks.iter().map(|task| task.music.clone()).collect::<Vec<_>>(), [Some(want)]);
        // No playlist file of an album; the library knows the second song by its Spotify key now.
        assert!(!dir.path().join("Sample Album.m3u8").exists());
        assert_eq!(crate::music::read_library(&library).get("spotify t2"), Some(&before));
    }
}
