//! Music links (YouTube Music, SoundCloud) downloaded as a library: audio as the site sends it,
//! saved as `Artist/Album (Year)/01 Title`, tagged with its album's place and cover; a playlist
//! as an `.m3u8` file over songs stored once; synced lyrics from LRCLIB when asked for.

use crate::media::MediaQualityPreset;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use url::Url;

/// What a music link names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Song,
    /// An album, EP or single: its songs in their order.
    Album,
    /// A playlist, or a SoundCloud set, which its listing may show is an album.
    Playlist,
    /// An artist's page.
    Artist,
}

/// The tabs of a SoundCloud user's page: lists of their tracks, not tracks.
const SOUNDCLOUD_TABS: &[&str] = &["tracks", "albums", "sets", "reposts", "likes", "spotlight", "popular-tracks", "followers", "following", "comments"];

/// What `url` names when it is a music link, from its shape alone: a YouTube Music song, album
/// (`browse/MPREb...`, or its `OLAK5uy_` playlist), playlist or artist; a SoundCloud track, set or
/// user. None for any other link, a site's own pages among them.
pub fn kind(url: &Url) -> Option<Kind> {
    use crate::media::{has_param, param, url_shape, SOUNDCLOUD_PAGES};
    let (host, path) = url_shape(url);
    match (host.as_str(), &path[..]) {
        ("music.youtube.com", ["watch"]) if has_param(url, "v") => Some(Kind::Song),
        ("music.youtube.com", ["playlist"]) => {
            param(url, "list").filter(|list| !list.is_empty()).map(|list| if list.starts_with("OLAK5uy_") { Kind::Album } else { Kind::Playlist })
        }
        ("music.youtube.com", ["browse", id]) if id.starts_with("MPREb") => Some(Kind::Album),
        ("music.youtube.com", ["channel", _]) => Some(Kind::Artist),
        ("soundcloud.com", [user, ..]) if SOUNDCLOUD_PAGES.contains(user) => None,
        // A set's link may end in its secret token.
        ("soundcloud.com", [_, "sets", _] | [_, "sets", _, _]) => Some(Kind::Playlist),
        ("soundcloud.com", [_]) => Some(Kind::Artist),
        ("soundcloud.com", [_, tab]) if SOUNDCLOUD_TABS.contains(tab) => Some(Kind::Artist),
        // A private track's link ends in its secret token.
        ("soundcloud.com", [_, _] | [_, _, _]) => Some(Kind::Song),
        _ => None,
    }
}

/// What a listing knows of a song that its own page may not: its album and place on it. The
/// rest is read from what the helper tool finds at the song (see [`resolve`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Music {
    /// Its place on the album, from 1.
    pub track: Option<u32>,
    pub album: Option<String>,
    /// The artist whose folder it goes in.
    pub album_artist: Option<String>,
    pub year: Option<u32>,
    /// The playlist file it is listed in, kept in the playlist's order (see [`refresh_playlist`]).
    pub playlist: Option<PathBuf>,
    /// Its album's square cover, as the listing it came from shows it: embedded in place of the
    /// song's own thumbnail.
    pub cover: Option<String>,
    /// What the library knows it by besides its download archive line: the Spotify, Apple Music
    /// or Deezer song it was found for (see `crate::catalog`).
    pub key: Option<String>,
}

/// The text of `key` in `value`, when it has any.
fn text(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::trim).filter(|text| !text.is_empty()).map(String::from)
}

/// An artist as YouTube names their channel of songs ("<artist> - Topic").
fn artist_name(channel: &str) -> String {
    channel.strip_suffix(" - Topic").unwrap_or(channel).to_string()
}

/// The album a listing (`-J --flat-playlist`) is, when it is one: a YouTube Music album's
/// playlist (`OLAK5uy_...`, titled "Album - <name>"; its artist the one channel its songs are
/// on), or a SoundCloud set its artist calls an album, EP, single or compilation.
pub(crate) fn album_of(list: &Value) -> Option<Music> {
    let year = list["release_year"].as_u64().and_then(|year| u32::try_from(year).ok());
    if list["id"].as_str().is_some_and(|id| id.starts_with("OLAK5uy_")) {
        let title = text(list, "title")?;
        let album = ["Album - ", "EP - ", "Single - "].iter().find_map(|kind| title.strip_prefix(kind)).unwrap_or(&title).to_string();
        let mut channels = list["entries"].as_array().into_iter().flatten().filter_map(|entry| text(entry, "channel")).map(|c| artist_name(&c));
        let first = channels.next();
        let album_artist = text(list, "channel").map(|c| artist_name(&c)).or_else(|| first.filter(|first| channels.all(|c| &c == first)));
        // The album's own cover, the largest square one (1200 px): no frame of a song's to cut.
        let square = list["thumbnails"].as_array().into_iter().flatten().filter(|t| t["width"].as_u64().is_some_and(|w| w >= 300 && t["height"].as_u64() == Some(w)));
        let cover = square.max_by_key(|t| t["width"].as_u64()).and_then(|t| text(t, "url"));
        return Some(Music { album: Some(album), album_artist, year, cover, ..Default::default() });
    }
    match list["album_type"].as_str() {
        Some("album" | "ep" | "single" | "compilation") => Some(Music {
            album: text(list, "album").or_else(|| text(list, "title")),
            album_artist: text(list, "album_artist").or_else(|| list["album_artists"][0].as_str().map(String::from)),
            year,
            ..Default::default()
        }),
        _ => None,
    }
}

/// `given` with what the helper tool found at the song (`info`) filling in what the listing did
/// not know: its album, the artist (the album's, else the song's first), the year it came out and
/// its number on the album.
pub(crate) fn resolve(info: &Value, given: &Music) -> Music {
    // The first of `keys` that is set; of a list, its first item.
    let first = |keys: &[&str]| {
        keys.iter().find_map(|key| match &info[*key] {
            Value::Array(items) => items.first().and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).map(String::from),
            _ => text(info, key),
        })
    };
    let released = || info["release_date"].as_str()?.get(..4)?.parse().ok();
    let year = || info["release_year"].as_u64().or_else(released).and_then(|year| u32::try_from(year).ok());
    let artists = ["album_artist", "album_artists", "artists", "artist", "creators", "creator", "uploader", "channel"];
    Music {
        track: given.track.or_else(|| info["track_number"].as_u64().and_then(|n| u32::try_from(n).ok())),
        album: given.album.clone().or_else(|| first(&["album"])),
        album_artist: given.album_artist.clone().or_else(|| first(&artists).map(|artist| artist_name(&artist))),
        year: given.year.or_else(year),
        playlist: given.playlist.clone(),
        cover: given.cover.clone(),
        key: given.key.clone(),
    }
}

/// What the helper tool found at a song (`info`), with `cover` as its only thumbnail: the one it
/// embeds (see [`Music::cover`]).
pub(crate) fn with_cover(info: &Value, cover: &str) -> Value {
    let mut info = info.clone();
    info["thumbnails"] = serde_json::json!([{ "url": cover, "id": "cover" }]);
    info["thumbnail"] = Value::from(cover);
    info
}

/// The helper tool's output template of a song placed as `music` says, under the save folder:
/// `Artist/Album (Year)/01 Title.ext`, without the album's folder when there is no album (a year
/// alone names none), the number when it has no place on one. Its title is the song's own.
pub(crate) fn file_template(music: &Music) -> String {
    let clean = |name: &str| crate::ingest::clean_path([name]).ok().map(|name| crate::media::template_literal(&name.to_string_lossy()));
    let artist = music.album_artist.as_deref().and_then(clean);
    let mut parts = vec![artist.unwrap_or_else(|| "%(album_artist,artist,uploader|Unknown artist)s".to_string())];
    let album = music.album.as_deref().map(|album| music.year.map_or_else(|| album.to_string(), |year| format!("{album} ({year})")));
    parts.extend(album.as_deref().and_then(clean));
    let number = music.track.map_or_else(String::new, |n| format!("{n:02} "));
    parts.push(format!("{number}%(track,title)s.%(ext)s"));
    parts.join(std::path::MAIN_SEPARATOR_STR)
}

/// Cuts a cover to the square of its shorter side, about its middle: a YouTube Music song's
/// thumbnail is its album's cover between bars. yt-dlp converts (and so cuts) only a thumbnail
/// that is no JPEG already: SoundCloud's artwork, a JPEG, is square as it is. yt-dlp splits these
/// as a shell would, so `\\,` reaches ffmpeg as `\,`, a comma inside the filter's expression.
const SQUARE_COVER: &str = r"ThumbnailsConvertor+ffmpeg_o:-c:v mjpeg -qmin 1 -qscale:v 2 -vf crop=min(iw\\,ih):min(iw\\,ih)";

/// The helper tool's arguments that make a song's download a library file: audio alone, its codec
/// copied into a container of its own (Opus into .opus, AAC into .m4a, an original file kept as it
/// is) unless the quality `converts` it to MP3 or M4A; the cover cut square; and its tags from
/// `music`, each a `--parse-metadata` of the text itself (`%(hf_none|)s` is nothing, a `:` would
/// end it). The year goes into the date tag in place of the upload's date.
pub(crate) fn ytdlp_args(music: &Music, converts: bool) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if !converts {
        args.push("-x".to_string());
    }
    args.extend(["--convert-thumbnails", "jpg", "--ppa", SQUARE_COVER].map(String::from));
    let (track, year) = (music.track.map(|n| n.to_string()), music.year.map(|year| year.to_string()));
    let fields = [(music.album.as_deref(), "album"), (music.album_artist.as_deref(), "album_artist"), (track.as_deref(), "track_number"), (year.as_deref(), "meta_date")];
    for (value, field) in fields {
        if let Some(value) = value {
            let value = crate::media::template_literal(value).replace(':', "\\:");
            args.extend(["--parse-metadata".to_string(), format!("%(hf_none|)s{}:%({field})s", value.trim_end_matches('\\'))]);
        }
    }
    args
}

/// What the card says of a song the helper tool found (`info`): by whom, from which album, and
/// the artist's original file when a signed-in browser is offered it (SoundCloud's `download`).
pub(crate) fn song_sentence(info: &Value) -> String {
    let placed = resolve(info, &Music::default());
    let artists: Vec<&str> = info["artists"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let by = if artists.is_empty() { placed.album_artist.clone() } else { Some(artists.join(", ")) };
    let mut said = by.map_or_else(|| "A song".to_string(), |by| format!("A song by {by}"));
    if let Some(album) = &placed.album {
        said.push_str(&format!(" from {album}"));
        if let Some(year) = placed.year {
            said.push_str(&format!(" ({year})"));
        }
    }
    let original = info["formats"].as_array().into_iter().flatten().find(|f| f["format_id"] == "download");
    if let Some(ext) = original.and_then(|format| format["ext"].as_str()) {
        said.push_str(&format!(". Original file: {}", ext.to_ascii_uppercase()));
    }
    said
}

/// What the card says of a music list `kind` titled `title` of `count` songs, its first song
/// placed as `first` says: an album (as its songs' album says), a playlist, or an artist's songs.
pub(crate) fn list_sentence(kind: Kind, title: &str, count: usize, first: Option<&Music>) -> String {
    let songs = if count == 1 { "1 song".to_string() } else { format!("{count} songs") };
    match first.filter(|music| music.album.is_some()) {
        Some(Music { album_artist: Some(by), .. }) => format!("An album of {songs} by {by}"),
        Some(_) => format!("An album of {songs}"),
        None if kind == Kind::Album => format!("An album of {songs}"),
        None if kind == Kind::Artist => format!("{songs} by {title}"),
        None => format!("A playlist of {songs}"),
    }
}

/// The quality a music download takes of `preset`: audio alone, as asked (MP3, M4A, a format
/// selection of the user's own) or else as the site sends it, in the language picked.
pub(crate) fn audio_preset(preset: &MediaQualityPreset) -> MediaQualityPreset {
    use crate::quality::{AudioForm, MediaPick};
    match preset {
        MediaQualityPreset::AudioMp3 | MediaQualityPreset::AudioM4a | MediaQualityPreset::Custom(_) => preset.clone(),
        MediaQualityPreset::Pick(pick) if pick.audio.is_some() => preset.clone(),
        MediaQualityPreset::Pick(pick) => {
            MediaQualityPreset::Pick(MediaPick { audio: Some(AudioForm::Original), language: pick.language.clone(), ..Default::default() })
        }
        _ => MediaQualityPreset::Pick(MediaPick { audio: Some(AudioForm::Original), ..Default::default() }),
    }
}

/// The library index: a line `<download archive line>\t<file>` for every song downloaded, next
/// to the download archive (`download-archive.music.txt`). It tells which songs of a list are
/// in the library already, and where a playlist finds them. Its own lock (see
/// `history::lock_file`) is held while it or a playlist file is written.
pub(crate) fn library_file() -> Option<PathBuf> {
    crate::media::archive_file().map(|file| file.with_extension("music.txt"))
}

/// The songs of the library index `file`, by their download archive line; a later line of one
/// wins. Empty before it exists. Blocking.
pub(crate) fn read_library(file: &Path) -> HashMap<String, PathBuf> {
    let text = std::fs::read(file).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default();
    text.lines().filter_map(|line| line.split_once('\t')).map(|(id, path)| (id.trim().to_string(), PathBuf::from(path.trim_end()))).collect()
}

/// Adds the song `path`, under each of `ids`, to the library index `file`. Blocking.
pub(crate) fn record_library(file: &Path, ids: &[String], path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = file.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let lines: String = ids.iter().map(|id| format!("{id}\t{}\n", path.display())).collect();
    std::fs::OpenOptions::new().create(true).append(true).open(file)?.write_all(lines.as_bytes())
}

/// Writes the playlist `file` titled `title`: its songs in the order of `ids` (their download
/// archive lines), each a comment line, followed by its file when `library` has it on disk, by
/// its path from the playlist's folder when it is in it. A song still to come is only its line,
/// which players pass over, and which [`refresh_playlist`] fills in. Blocking.
pub(crate) fn write_playlist(file: &Path, title: &str, ids: &[String], library: &HashMap<String, PathBuf>) -> std::io::Result<()> {
    let dir = file.parent().unwrap_or(Path::new(""));
    let mut text = format!("#EXTM3U\n#PLAYLIST:{}\n", title.replace(['\r', '\n'], " "));
    for id in ids {
        text.push_str(&format!("# {id}\n"));
        let Some(song) = library.get(id).filter(|song| song.is_file()) else { continue };
        let line = match song.strip_prefix(dir) {
            Ok(inside) => inside.components().map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"),
            Err(_) => song.display().to_string(),
        };
        text.push_str(&line);
        text.push('\n');
    }
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(dir)?;
    }
    // Written whole, then put in place: a player never reads half of it.
    let part = file.with_extension("m3u8.part");
    std::fs::write(&part, text)?;
    std::fs::rename(&part, file)
}

/// Writes the playlist `file` again (see [`write_playlist`]), its title and songs as it lists
/// them, with what `library` holds now. Blocking.
pub(crate) fn refresh_playlist(file: &Path, library: &HashMap<String, PathBuf>) -> std::io::Result<()> {
    let text = std::fs::read_to_string(file)?;
    let title = text.lines().find_map(|line| line.strip_prefix("#PLAYLIST:")).unwrap_or_default().to_string();
    let ids: Vec<String> = text.lines().filter_map(|line| line.strip_prefix("# ")).map(String::from).collect();
    write_playlist(file, &title, &ids, library)
}

/// Where LRCLIB answers.
pub(crate) const LRCLIB: &str = "https://lrclib.net";

/// Most an answer of LRCLIB may hold.
const MAX_LYRICS: usize = 1 << 20;

/// The synced lyrics of the song at `audio` (what the helper tool found at it: `info`, placed as
/// `music` says), from LRCLIB at `base`, written next to it as `.lrc`: asked by its title, first
/// artist, album and length, with a User-Agent naming the app, and taken only when the answer's
/// title and artist are the song's and its length within 2 seconds of it. None, and nothing
/// written, otherwise.
pub(crate) async fn lyrics(client: &reqwest::Client, base: &str, info: &Value, music: &Music, audio: &Path) -> Option<PathBuf> {
    let title = text(info, "track").or_else(|| text(info, "title"))?;
    let artist = info["artists"].get(0).and_then(Value::as_str).map(String::from).or_else(|| text(info, "artist")).or_else(|| text(info, "uploader"))?;
    let duration = info["duration"].as_f64()?;
    let mut query = vec![("track_name", title.clone()), ("artist_name", artist.clone())];
    query.extend(music.album.clone().map(|album| ("album_name", album)));
    query.push(("duration", format!("{}", duration.round() as u64)));
    let agent = format!("EndosUnifiedDownloader/{} (https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader)", env!("CARGO_PKG_VERSION"));
    let request = client.get(format!("{base}/api/get")).query(&query).header(reqwest::header::USER_AGENT, agent);
    let mut response = request.timeout(std::time::Duration::from_secs(15)).send().await.ok()?.error_for_status().ok()?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_LYRICS {
            return None;
        }
    }
    let found: Value = serde_json::from_slice(&body).ok()?;
    let same = |key: &str, ours: &str| found[key].as_str().is_some_and(|theirs| theirs.trim().eq_ignore_ascii_case(ours.trim()));
    let close = found["duration"].as_f64().is_some_and(|theirs| (theirs - duration).abs() <= 2.0);
    let synced = found["syncedLyrics"].as_str().filter(|synced| !synced.trim().is_empty())?;
    if !(same("trackName", &title) && same("artistName", &artist) && close) {
        return None;
    }
    let file = audio.with_extension("lrc");
    tokio::fs::write(&file, synced).await.ok()?;
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quality::{AudioForm, MediaPick};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    /// The helper tool's answer for a YouTube Music song, as its real answers have it.
    fn song() -> Value {
        json!({
            "id": "aaaaaaaaaa2", "title": "Second Song", "track": "Second Song", "artist": "Sample Artist",
            "artists": ["Sample Artist"], "album": "Sample Album", "album_artist": null, "release_year": 2001,
            "release_date": null, "upload_date": "20141106", "duration": 226, "channel": "Sample Artist",
            "uploader": "Sample Artist", "extractor_key": "Youtube",
            "formats": [
                {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.2", "abr": 129.5, "filesize": 3_600_000},
                {"format_id": "251", "ext": "webm", "vcodec": "none", "acodec": "opus", "abr": 135.2, "filesize": 3_700_000}
            ]
        })
    }

    /// A SoundCloud track, with its original file as a signed-in browser gets it.
    fn track() -> Value {
        json!({
            "id": "293", "title": "Third Track", "track": "Third Track", "artist": "Sample Artist",
            "artists": ["Sample Artist"], "album": null, "album_artist": null, "uploader": "Sample Artist",
            "release_year": 2003, "duration": 213.886, "extractor_key": "Soundcloud",
            "formats": [
                {"format_id": "download", "ext": "flac", "vcodec": "none", "format_note": "Original"},
                {"format_id": "hls_aac_160k", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.2", "abr": 160}
            ]
        })
    }

    #[test]
    fn music_links_are_told_from_their_shape() {
        use Kind::*;
        for (link, kind_of) in [
            ("https://music.youtube.com/watch?v=aaaaaaaaaa1", Some(Song)),
            ("https://music.youtube.com/watch?v=aaaaaaaaaa1&list=PLsample", Some(Song)),
            ("https://music.youtube.com/playlist?list=OLAK5uy_sample", Some(Album)),
            ("https://music.youtube.com/browse/MPREb_sample", Some(Album)),
            ("https://music.youtube.com/playlist?list=PLsample", Some(Playlist)),
            ("https://music.youtube.com/channel/UCsample", Some(Artist)),
            ("https://soundcloud.com/sample-artist/first-track", Some(Song)),
            ("https://m.soundcloud.com/sample-artist/first-track/s-token", Some(Song)),
            ("https://soundcloud.com/sample-artist/sets/sample-set", Some(Playlist)),
            ("https://soundcloud.com/sample-artist", Some(Artist)),
            ("https://soundcloud.com/sample-artist/tracks", Some(Artist)),
            ("https://soundcloud.com/sample-artist/sets", Some(Artist)),
            ("https://www.youtube.com/watch?v=aaaaaaaaaa1", None),
            ("https://music.youtube.com/", None),
            ("https://music.youtube.com/watch", None),
            ("https://soundcloud.com/discover", None),
            ("https://soundcloud.com/discover/sets/sample", None),
            ("https://vimeo.com/76979871", None),
        ] {
            assert_eq!(kind(&url(link)), kind_of, "{link}");
        }
    }

    #[test]
    fn an_album_listing_names_its_album_and_artist() {
        let ytm = json!({
            "id": "OLAK5uy_sample", "title": "Album - Sample Album", "channel": null, "uploader": null, "release_year": null,
            "entries": [{"title": "First Song", "channel": "Sample Artist - Topic"}, {"title": "Second Song", "channel": "Sample Artist - Topic"}]
        });
        let want = Music { album: Some("Sample Album".into()), album_artist: Some("Sample Artist".into()), ..Default::default() };
        assert_eq!(album_of(&ytm), Some(want.clone()));
        // Its own square cover, the largest; a wide frame is none.
        let mut covered = ytm.clone();
        covered["thumbnails"] = json!([
            {"url": "https://i.ytimg.test/s_p/a/mqdefault.jpg", "width": 180, "height": 180},
            {"url": "https://i.ytimg.test/s_p/a/maxresdefault.jpg", "width": 1200, "height": 1200},
            {"url": "https://i.ytimg.test/vi/a/hqdefault.jpg", "width": 1280, "height": 720},
            {"url": "https://i.ytimg.test/s_p/a/sddefault.jpg", "width": 640, "height": 640}
        ]);
        let cover = Some("https://i.ytimg.test/s_p/a/maxresdefault.jpg".to_string());
        assert_eq!(album_of(&covered), Some(Music { cover: cover.clone(), ..want }));
        // What the helper tool found at a song then embeds that cover alone.
        let found = with_cover(&song(), cover.as_deref().unwrap());
        assert_eq!((found["thumbnail"].as_str(), found["thumbnails"].as_array().map(Vec::len)), (cover.as_deref(), Some(1)));
        // An EP; songs of several artists name none.
        let mut ep = ytm.clone();
        ep["title"] = "EP - Sample EP".into();
        ep["entries"][1]["channel"] = "Other Artist".into();
        assert_eq!(album_of(&ep), Some(Music { album: Some("Sample EP".into()), ..Default::default() }));
        let set = json!({"id": "18", "title": "Sample Set", "album": "Sample Set", "album_artist": "Sample Artist", "album_type": "album", "release_year": 2003});
        let want = Music { album: Some("Sample Set".into()), album_artist: Some("Sample Artist".into()), year: Some(2003), ..Default::default() };
        assert_eq!(album_of(&set), Some(want));
        // A set that is a playlist, and any other playlist, is no album.
        assert_eq!(album_of(&json!({"id": "19", "title": "Mix", "album_type": "playlist"})), None);
        assert_eq!(album_of(&json!({"id": "PLsample", "title": "Road Trip"})), None);
    }

    #[test]
    fn a_song_is_placed_in_its_artists_and_albums_folder() {
        let placed = resolve(&song(), &Music::default());
        let want = Music { album: Some("Sample Album".into()), album_artist: Some("Sample Artist".into()), year: Some(2001), ..Default::default() };
        assert_eq!(placed, want);
        let folder = Path::new("Sample Artist").join("Sample Album (2001)");
        assert_eq!(file_template(&placed), format!("{}{}%(track,title)s.%(ext)s", folder.display(), std::path::MAIN_SEPARATOR));
        // The listing's knowledge comes first: its place on the album.
        let listed = Music { track: Some(2), album: Some("Sample Album".into()), ..Default::default() };
        let placed = resolve(&song(), &listed);
        assert_eq!(placed.track, Some(2));
        assert!(file_template(&placed).ends_with(&format!("{}02 %(track,title)s.%(ext)s", std::path::MAIN_SEPARATOR)));
        // Without an album, under the artist; a year alone names no folder.
        let placed = resolve(&track(), &Music::default());
        assert_eq!(file_template(&placed), format!("Sample Artist{}%(track,title)s.%(ext)s", std::path::MAIN_SEPARATOR));
        // Names stand for themselves: a % is no field, a / no folder.
        let odd = Music { album_artist: Some("50% Off".into()), album: Some("A/B".into()), ..Default::default() };
        let template = file_template(&odd);
        assert!(template.starts_with("50%% Off") && !template.contains("A/B") && !template.contains("A\\B"), "{template}");
        // Nothing known of the artist: the helper tool names it.
        assert!(file_template(&Music::default()).starts_with("%(album_artist,artist,uploader|Unknown artist)s"));
    }

    #[test]
    fn a_songs_tags_and_square_cover_are_asked_of_the_helper_tool() {
        let placed = Music { track: Some(2), album: Some("Sample Album".into()), album_artist: Some("Sample: Artist".into()), year: Some(2001), ..Default::default() };
        let args = ytdlp_args(&placed, false);
        let has = |pair: [&str; 2]| args.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1]);
        assert!(args.contains(&"-x".to_string()), "{args:?}");
        assert!(has(["--convert-thumbnails", "jpg"]), "{args:?}");
        assert!(args.iter().any(|a| a.starts_with("ThumbnailsConvertor+ffmpeg_o:") && a.contains("crop=")), "{args:?}");
        assert!(has(["--parse-metadata", "%(hf_none|)sSample Album:%(album)s"]), "{args:?}");
        assert!(has(["--parse-metadata", "%(hf_none|)sSample\\: Artist:%(album_artist)s"]), "{args:?}");
        assert!(has(["--parse-metadata", "%(hf_none|)s2:%(track_number)s"]), "{args:?}");
        assert!(has(["--parse-metadata", "%(hf_none|)s2001:%(meta_date)s"]), "{args:?}");
        // MP3 or M4A converts already; nothing unknown is asked for.
        let args = ytdlp_args(&Music::default(), true);
        assert!(!args.contains(&"-x".to_string()) && !args.contains(&"--parse-metadata".to_string()), "{args:?}");
    }

    #[test]
    fn the_card_says_what_a_music_link_is() {
        assert_eq!(song_sentence(&song()), "A song by Sample Artist from Sample Album (2001)");
        assert_eq!(song_sentence(&track()), "A song by Sample Artist. Original file: FLAC");
        let album = Music { album: Some("Sample Album".into()), album_artist: Some("Sample Artist".into()), track: Some(1), ..Default::default() };
        assert_eq!(list_sentence(Kind::Album, "Album - Sample Album", 14, Some(&album)), "An album of 14 songs by Sample Artist");
        // A set is an album when its listing says so.
        assert_eq!(list_sentence(Kind::Playlist, "Sample Set", 11, Some(&album)), "An album of 11 songs by Sample Artist");
        assert_eq!(list_sentence(Kind::Playlist, "Road Trip", 1, Some(&Music::default())), "A playlist of 1 song");
        assert_eq!(list_sentence(Kind::Artist, "Sample Artist", 30, None), "30 songs by Sample Artist");
    }

    #[test]
    fn music_is_audio_whatever_the_quality_setting() {
        let original = |language: Option<&str>| {
            MediaQualityPreset::Pick(MediaPick { audio: Some(AudioForm::Original), language: language.map(String::from), ..Default::default() })
        };
        assert_eq!(audio_preset(&MediaQualityPreset::BestVideoAudio), original(None));
        assert_eq!(audio_preset(&MediaQualityPreset::Fhd1080p), original(None));
        let video = MediaQualityPreset::Pick(MediaPick { up_to: Some(1080), language: Some("de".into()), ..Default::default() });
        assert_eq!(audio_preset(&video), original(Some("de")));
        for kept in [
            MediaQualityPreset::AudioMp3,
            MediaQualityPreset::AudioM4a,
            MediaQualityPreset::Pick(MediaPick { audio: Some(AudioForm::M4a), ..Default::default() }),
            MediaQualityPreset::Custom("ba[abr>100]".into()),
        ] {
            assert_eq!(audio_preset(&kept), kept);
        }
    }

    #[test]
    fn a_playlist_file_lists_its_songs_in_order_as_they_arrive() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let first = dir.path().join("Sample Artist").join("Sample Album (2001)").join("01 First Song.opus");
        let third = elsewhere.path().join("Third Track.m4a");
        for file in [&first, &third] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, b"audio").unwrap();
        }
        let index = dir.path().join("library.txt");
        record_library(&index, &["youtube a1".to_string()], &first).unwrap();
        record_library(&index, &["soundcloud 293".to_string()], &dir.path().join("gone.m4a")).unwrap();
        // The later line wins.
        record_library(&index, &["soundcloud 293".to_string()], &third).unwrap();
        let library = read_library(&index);
        assert_eq!(library.get("soundcloud 293"), Some(&third));
        let file = dir.path().join("Road Trip.m3u8");
        let ids = ["youtube a1", "youtube a2", "soundcloud 293", "youtube gone"].map(String::from);
        record_library(&index, &["youtube gone".to_string()], &dir.path().join("deleted.opus")).unwrap();
        write_playlist(&file, "Road Trip", &ids, &read_library(&index)).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let want = format!(
            "#EXTM3U\n#PLAYLIST:Road Trip\n# youtube a1\nSample Artist/Sample Album (2001)/01 First Song.opus\n# youtube a2\n# soundcloud 293\n{}\n# youtube gone\n",
            third.display()
        );
        assert_eq!(text, want);
        // The second song arrives: it takes its place.
        let second = dir.path().join("Sample Artist").join("Second Song.opus");
        std::fs::write(&second, b"audio").unwrap();
        record_library(&index, &["youtube a2".to_string()], &second).unwrap();
        refresh_playlist(&file, &read_library(&index)).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("# youtube a1\nSample Artist/Sample Album (2001)/01 First Song.opus\n# youtube a2\nSample Artist/Second Song.opus\n# soundcloud 293\n"), "{text}");
    }

    /// Answers one request with `status` and `body`, and hands back the request's head.
    async fn lrclib_stand_in(status: &'static str, body: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 2048];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&buf[..n]);
            }
            let reply = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            sock.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&head).into_owned()
        });
        (base, served)
    }

    #[tokio::test]
    async fn synced_lyrics_are_taken_only_for_the_same_song() {
        // Made-up lines: no song's words.
        let synced = "[00:01.00] made-up line one\n[00:05.00] made-up line two";
        let answer = |duration: f64| json!({"trackName": "Second Song", "artistName": "Sample Artist", "albumName": "Sample Album", "duration": duration, "syncedLyrics": synced}).to_string();
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("02 Second Song.opus");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let placed = resolve(&song(), &Music::default());
        let (base, served) = lrclib_stand_in("200 OK", answer(226.4)).await;
        let lrc = tokio::time::timeout(std::time::Duration::from_secs(20), lyrics(&client, &base, &song(), &placed, &audio)).await.unwrap();
        assert_eq!(lrc.as_deref(), Some(dir.path().join("02 Second Song.lrc").as_path()));
        assert_eq!(std::fs::read_to_string(dir.path().join("02 Second Song.lrc")).unwrap(), synced);
        let head = served.await.unwrap().to_ascii_lowercase();
        let line = head.lines().next().unwrap_or_default().to_string();
        assert!(line.starts_with("get /api/get?"), "{line}");
        for part in ["track_name=second+song", "artist_name=sample+artist", "album_name=sample+album", "duration=226"] {
            assert!(line.contains(part), "{line} lacks {part}");
        }
        assert!(head.contains(&format!("user-agent: endosunifieddownloader/{}", env!("CARGO_PKG_VERSION"))), "{head}");
        // Another length is another recording: nothing is written.
        std::fs::remove_file(dir.path().join("02 Second Song.lrc")).unwrap();
        let (base, served) = lrclib_stand_in("200 OK", answer(240.0)).await;
        assert_eq!(lyrics(&client, &base, &song(), &placed, &audio).await, None);
        served.await.unwrap();
        let (base, served) = lrclib_stand_in("404 Not Found", json!({"code": 404}).to_string()).await;
        assert_eq!(lyrics(&client, &base, &song(), &placed, &audio).await, None);
        served.await.unwrap();
        assert!(!dir.path().join("02 Second Song.lrc").exists());
    }
}
