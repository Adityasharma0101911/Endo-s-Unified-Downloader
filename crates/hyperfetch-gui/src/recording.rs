//! Video the browser extension records from a page (the buffers a player feeds its MediaSource,
//! or MediaRecorder's output), and streams it downloads through the page itself, arrive through
//! the local API (see `ipc`) in chunks, kept one file per track and part in a temporary folder,
//! and are joined into the save folder once finished. A new part of a track starts where the
//! player switched to another init segment (a quality switch). What an earlier run of the app
//! left in that folder is joined when the app starts (see [`Recordings::recover`]).

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyperfetch_core::engine::TargetClaim;
use hyperfetch_core::{claim_target, engine, ingest};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// Highest track index a recording may have.
pub const MAX_TRACK: u8 = 15;
/// Recordings open at once; another is refused.
const MAX_OPEN: usize = 8;
/// Bytes one recording may hold, its tracks together, so a runaway one cannot fill the disk.
const MAX_BYTES: u64 = 32 << 30;
/// A recording that got no chunk for this long was left open by a browser that went away: it
/// is closed and joined to make room when [`MAX_OPEN`] are open.
const STALE: Duration = Duration::from_secs(10 * 60);
/// A finished recording smaller than this holds no video worth keeping and is deleted.
const MIN_BYTES: u64 = 64 * 1024;
/// The file in a recording's folder that says what it records, for a later run to read.
const META: &str = "meta.json";
/// Largest moof box read for its decode time; a bigger one is no MediaSource fragment.
const MAX_MOOF: u64 = 1 << 20;
/// How far into a WebM its first Cluster is looked for.
const WEBM_HEAD: u64 = 4 << 20;

/// Whether `id` can name a recording: 1 to 32 ASCII letters and digits, so it is safe in a path.
pub fn valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The recordings open now, each in its own folder under `root`.
pub struct Recordings {
    root: PathBuf,
    // ponytail: one lock for every recording keeps each track's chunks in arrival order; give
    // each recording its own lock if several recording at once ever wait on a slow disk.
    open: Mutex<HashMap<String, Recording>>,
    started: AtomicU64,
}

struct Recording {
    title: String,
    dir: PathBuf,
    /// Each part's file and its size, by track and part index.
    parts: BTreeMap<(u8, u8), (PathBuf, u64)>,
    bytes: u64,
    last_chunk: Instant,
}

/// A recording the extension finished, to be joined into the save folder (see [`merge`]).
#[derive(Debug)]
pub struct Finished {
    pub title: String,
    /// The temporary folder that holds the tracks, deleted once they are saved.
    pub dir: PathBuf,
    /// Each track's index and its parts' files, in order.
    pub tracks: Vec<(u8, Vec<PathBuf>)>,
    /// An earlier run of the app left it (see [`Recordings::recover`]).
    pub recovered: bool,
}

/// Why a recording request was refused.
#[derive(Debug, PartialEq)]
pub enum Refusal {
    /// No open recording has that id.
    Unknown,
    /// Too many recordings at once, or too many bytes in one.
    Limit(&'static str),
    /// The temporary folder could not be written.
    Disk(String),
}

impl Recordings {
    /// Recordings kept in folders under `root`.
    pub fn new(root: PathBuf) -> Self {
        Self { root, open: Mutex::new(HashMap::new()), started: AtomicU64::new(0) }
    }

    /// Closes the recordings a browser that went away left open (see [`STALE`]) once
    /// [`MAX_OPEN`] are open, to make room for another: what they recorded, to be joined.
    pub async fn close_stale(&self) -> Vec<Finished> {
        let mut open = self.open.lock().await;
        if open.len() < MAX_OPEN {
            return Vec::new();
        }
        let stale: Vec<String> = open.iter().filter(|(_, r)| r.last_chunk.elapsed() >= STALE).map(|(id, _)| id.clone()).collect();
        stale.iter().filter_map(|id| open.remove(id)).filter_map(Recording::close).collect()
    }

    /// The recordings an earlier run of the app left in `root` (it ended while the browser was
    /// recording), closed as a finish closes them; the folders of recordings open now are left
    /// alone, and so are those written to within [`STALE`]: another copy of the app (an older one
    /// the extension still talks to) may be recording into them. One too small to keep is deleted.
    // ponytail: what a run that just crashed left is joined at the next start after STALE; add
    // an instance lock if that wait ever matters.
    pub async fn recover(&self) -> Vec<Finished> {
        let open = self.open.lock().await;
        let Ok(entries) = std::fs::read_dir(&self.root) else { return Vec::new() };
        entries
            .flatten()
            .filter(|entry| entry.file_name().to_str().is_some_and(|id| valid_id(id) && !open.contains_key(id)))
            .filter(|entry| entry.path().is_dir() && !written_within(&entry.path(), STALE))
            .filter_map(|entry| Recording::left_in(entry.path()).close())
            .map(|finished| Finished { recovered: true, ..finished })
            .collect()
    }

    /// Opens a recording titled `title`, of the page at `page_url`, and returns its id. Its
    /// folder gets a [`META`] file saying so, for a later run should this one end first.
    pub async fn start(&self, title: &str, page_url: &str) -> Result<String, Refusal> {
        let mut open = self.open.lock().await;
        if open.len() >= MAX_OPEN {
            return Err(Refusal::Limit("too many recordings at once"));
        }
        // Unique in this run by the count, and not guessable by the random part.
        let count = self.started.fetch_add(1, Ordering::Relaxed);
        let id = format!("{:x}{:016x}", count, RandomState::new().build_hasher().finish());
        let dir = self.root.join(&id);
        let started = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let meta = serde_json::json!({"title": title, "page_url": page_url, "started": started}).to_string();
        let made = match tokio::fs::create_dir_all(&dir).await {
            Ok(()) => tokio::fs::write(dir.join(META), meta).await,
            Err(e) => Err(e),
        };
        if let Err(e) = made {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(Refusal::Disk(format!("Cannot create {}: {}", dir.display(), e)));
        }
        let recording = Recording { title: title.to_string(), dir, parts: BTreeMap::new(), bytes: 0, last_chunk: Instant::now() };
        open.insert(id.clone(), recording);
        Ok(id)
    }

    /// Appends `data` to part `part` of track `track` of recording `id` and returns the size of
    /// the track, its parts together. The first chunk of a part names its file
    /// (`track<K>.part<P>.<ext>`) for `mime`; later ones go to that file whatever theirs.
    pub async fn append(&self, id: &str, track: u8, part: u8, mime: &str, data: &[u8]) -> Result<u64, Refusal> {
        let mut open = self.open.lock().await;
        let recording = open.get_mut(id).ok_or(Refusal::Unknown)?;
        let size = data.len() as u64;
        if recording.bytes + size > MAX_BYTES {
            return Err(Refusal::Limit("the recording is too large"));
        }
        let dir = &recording.dir;
        let (path, part_bytes) = recording
            .parts
            .entry((track, part))
            .or_insert_with(|| (dir.join(format!("track{}.part{}.{}", track, part, ext_for_mime(mime))), 0));
        let disk = |e: std::io::Error| Refusal::Disk(format!("Cannot write {}: {}", path.display(), e));
        let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await.map_err(disk)?;
        file.write_all(data).await.map_err(disk)?;
        // Done before the next chunk opens the file again.
        file.flush().await.map_err(disk)?;
        *part_bytes += size;
        recording.bytes += size;
        recording.last_chunk = Instant::now();
        Ok(recording.parts.range((track, 0)..=(track, u8::MAX)).map(|(_, (_, bytes))| bytes).sum())
    }

    /// Closes recording `id`: what it recorded, to be joined, or None when it is too small to
    /// keep and was deleted.
    pub async fn finish(&self, id: &str) -> Result<Option<Finished>, Refusal> {
        let recording = self.open.lock().await.remove(id).ok_or(Refusal::Unknown)?;
        Ok(recording.close())
    }

    /// Closes recording `id` and deletes what it recorded.
    pub async fn abort(&self, id: &str) -> Result<(), Refusal> {
        let recording = self.open.lock().await.remove(id).ok_or(Refusal::Unknown)?;
        let _ = tokio::fs::remove_dir_all(&recording.dir).await;
        Ok(())
    }
}

impl Recording {
    /// The recording an earlier run left in `dir`: its title from [`META`] (none when that is
    /// gone) and its part files, as named by [`part_of`].
    fn left_in(dir: PathBuf) -> Recording {
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct Meta {
            title: String,
        }
        let meta = std::fs::read(dir.join(META)).ok().and_then(|bytes| serde_json::from_slice::<Meta>(&bytes).ok());
        let mut parts = BTreeMap::new();
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if let Some(key) = entry.file_name().to_str().and_then(part_of) {
                parts.insert(key, (entry.path(), entry.metadata().map_or(0, |m| m.len())));
            }
        }
        let bytes = parts.values().map(|(_, bytes)| bytes).sum();
        Recording { title: meta.unwrap_or_default().title, dir, parts, bytes, last_chunk: Instant::now() }
    }

    /// What it recorded, to be joined, or None when it is too small to keep and was deleted.
    fn close(self) -> Option<Finished> {
        if self.bytes < MIN_BYTES {
            let _ = std::fs::remove_dir_all(&self.dir);
            return None;
        }
        let mut tracks: Vec<(u8, Vec<PathBuf>)> = Vec::new();
        // A part a failed write left empty holds nothing to join.
        for ((track, _), (path, _)) in self.parts.into_iter().filter(|(_, (_, bytes))| *bytes > 0) {
            match tracks.last_mut() {
                Some((last, parts)) if *last == track => parts.push(path),
                _ => tracks.push((track, vec![path])),
            }
        }
        Some(Finished { title: self.title, dir: self.dir, tracks, recovered: false })
    }
}

/// Whether a file in `dir` was written within `age` (or, by a clock set back, in the future).
fn written_within(dir: &Path, age: Duration) -> bool {
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    entries.filter_map(|entry| entry.metadata().ok()?.modified().ok()).any(|modified| modified.elapsed().map_or(true, |since| since < age))
}

/// The track and part a recording's file holds, by its name: `track<K>.part<P>.<ext>`, or
/// `track<K>.<ext>` (part 0) as an older version named it.
fn part_of(name: &str) -> Option<(u8, u8)> {
    let pieces: Vec<&str> = name.split('.').collect();
    let (track, part) = match pieces[..] {
        [track, _ext] => (track, "part0"),
        [track, part, _ext] => (track, part),
        _ => return None,
    };
    let track = track.strip_prefix("track")?.parse::<u8>().ok().filter(|&track| track <= MAX_TRACK)?;
    Some((track, part.strip_prefix("part")?.parse().ok()?))
}

/// The file extension of a track recorded as `mime`.
pub fn ext_for_mime(mime: &str) -> &'static str {
    let mime = mime.trim().to_ascii_lowercase();
    if mime.contains("webm") {
        "webm"
    } else if mime.starts_with("audio/mp4") {
        "m4a"
    } else if mime.contains("mp4") {
        "mp4"
    } else if mime.starts_with("audio/mpeg") {
        "mp3"
    } else if mime.contains("mp2t") {
        "ts"
    } else if mime.contains("aac") {
        // Packed audio (ADTS) of an HLS audio rendition, as a browser download sends it.
        "aac"
    } else {
        "bin"
    }
}

fn ext_of(path: &Path) -> &str {
    path.extension().and_then(|ext| ext.to_str()).unwrap_or("bin")
}

/// The container tracks with the extensions `exts` are joined into: MP4 for MP4, MPEG-TS and AAC
/// (remuxed), WebM for WebM, else Matroska, which takes anything.
fn container_for(exts: &[&str]) -> &'static str {
    if exts.iter().all(|&ext| ext == "webm") {
        "webm"
    } else if exts.iter().all(|&ext| matches!(ext, "mp4" | "m4a" | "ts" | "aac")) {
        "mp4"
    } else {
        "mkv"
    }
}

/// ffmpeg's arguments to join `inputs` into `out` without encoding them again: their video and
/// audio, not the data streams (ID3 tags of an MPEG-TS) an MP4 cannot hold. A single track is
/// copied all the same: that indexes a fragmented MP4 and gives MediaRecorder's WebM a duration.
fn ffmpeg_args(inputs: &[PathBuf], out: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-y", "-nostdin", "-hide_banner", "-loglevel", "error"].map(OsString::from).into();
    for input in inputs {
        args.extend(["-i".into(), input.into()]);
    }
    for n in 0..inputs.len() {
        args.extend(["-map".into(), format!("{}:v?", n).into(), "-map".into(), format!("{}:a?", n).into()]);
    }
    args.extend(["-c".into(), "copy".into()]);
    if out.extension().is_some_and(|ext| ext == "mp4") {
        args.extend(["-movflags".into(), "+faststart".into()]);
    }
    args.push(out.into());
    args
}

/// `<stem>.<ext>` in `dir`, or `<stem> (1).<ext>`, `<stem> (2).<ext>`… when that is taken: by a
/// file, or by a download or another recording that claimed it and has not written it yet (see
/// `claim_target`). The name stays ours while the claim returned with it is held.
fn free_path(dir: &Path, stem: &str, ext: &str) -> Result<(PathBuf, TargetClaim), String> {
    let mut n = 0;
    loop {
        let name = if n == 0 { format!("{}.{}", stem, ext) } else { format!("{} ({}).{}", stem, n, ext) };
        let path = dir.join(name);
        n += 1;
        if let Some(claim) = claim_target(&path)? {
            if !path.exists() {
                return Ok((path, claim));
            }
        }
    }
}

/// Joins each track of `finished` from its parts (see [`join_track`]), then the tracks into
/// `<title>.<container>` in `save_dir` with `ffmpeg`; or, without ffmpeg or when it fails, moves
/// what it has there as it is (`<title>.track<k>.<ext>`, or `<title>.track<k>.part<p>.<ext>` for
/// a track whose parts only ffmpeg could join); then deletes the temporary folder. Returns the
/// notice to show: an error when the tracks were saved unjoined, or could not be saved (they are
/// kept in the temporary folder then).
pub fn merge(finished: &Finished, save_dir: &Path, ffmpeg: Option<&Path>) -> Result<String, String> {
    let kept = || format!("the recording is kept in {}", finished.dir.display());
    if save_dir.as_os_str().is_empty() {
        return Err(format!("Choose a folder to save downloads to; {}", kept()));
    }
    std::fs::create_dir_all(save_dir).map_err(|e| format!("Cannot create {}: {}; {}", save_dir.display(), e, kept()))?;
    let stem = Some(engine::sanitize_filename(&finished.title)).filter(|s| !s.is_empty()).unwrap_or_else(|| "recording".to_string());
    // One file per track to join, and what is saved as it is should that fail, by name.
    let (mut tracks, mut pieces, mut why) = (Vec::new(), Vec::new(), None);
    for (track, parts) in &finished.tracks {
        match join_track(ffmpeg, &finished.dir, *track, parts) {
            Ok(file) => {
                pieces.push((format!("track{}", track), file.clone()));
                tracks.push(file);
            }
            Err(e) => {
                why.get_or_insert(e);
                pieces.extend(parts.iter().enumerate().map(|(n, part)| (format!("track{}.part{}", track, n), part.clone())));
            }
        }
    }
    let why = match (why, ffmpeg) {
        (Some(why), _) => why,
        (None, None) => "ffmpeg was not found".to_string(),
        (None, Some(ffmpeg)) => {
            let exts: Vec<&str> = tracks.iter().map(|track| ext_of(track)).collect();
            // Recordings finishing together, and downloads, never write the same file.
            let (out, _claim) = free_path(save_dir, &stem, container_for(&exts)).map_err(|e| format!("{}; {}", e, kept()))?;
            match run_ffmpeg(ffmpeg, ffmpeg_args(&tracks, &out)) {
                Ok(()) => {
                    let _ = std::fs::remove_dir_all(&finished.dir);
                    return Ok(format!("Saved the recording as {}", out.display()));
                }
                Err(e) => {
                    // What ffmpeg left of the file it could not finish.
                    let _ = std::fs::remove_file(&out);
                    e
                }
            }
        }
    };
    let mut saved = Vec::new();
    for (name, file) in &pieces {
        let (to, _claim) = free_path(save_dir, &format!("{}.{}", stem, name), ext_of(file)).map_err(|e| format!("{}; {}", e, kept()))?;
        // A rename cannot cross drives; a copy can, and the temporary folder goes anyway.
        std::fs::rename(file, &to)
            .or_else(|_| std::fs::copy(file, &to).map(drop))
            .map_err(|e| format!("Cannot save {}: {}; {}", to.display(), e, kept()))?;
        saved.push(to.file_name().unwrap_or_default().to_string_lossy().into_owned());
    }
    let _ = std::fs::remove_dir_all(&finished.dir);
    Err(format!(
        "Saved the recording in {} as {} without joining its tracks ({}): ffmpeg is needed to join them",
        save_dir.display(),
        saved.join(", "),
        why
    ))
}

/// Joins the parts of track `track` into one file in `dir`, or returns its only part when that
/// needs no change. Parts that all start with the same init segment are joined byte by byte,
/// with that init segment once; parts that do not (a quality switch) are encoded again by
/// `ffmpeg` (see [`reencode`]). Fragmented MP4 is put in time order first, without repeats; a
/// timeline the player started again (see [`timelines`]) goes after the one before as a file of
/// its own, which `ffmpeg` joins running the time on (in one file, the second would play in an
/// instant).
fn join_track(ffmpeg: Option<&Path>, dir: &Path, track: u8, parts: &[PathBuf]) -> Result<PathBuf, String> {
    let failed = |e: io::Error| format!("Cannot join the parts of track {}: {}", track, e);
    let layouts: Vec<Layout> = parts.iter().map(|part| Layout::of(part)).collect::<io::Result<_>>().map_err(failed)?;
    let inits: Vec<Vec<u8>> = parts.iter().zip(&layouts).map(|(part, layout)| layout.init_bytes(part)).collect::<io::Result<_>>().map_err(failed)?;
    if inits.iter().all(|init| *init == inits[0]) {
        if let ([part], [layout]) = (parts, &layouts[..]) {
            if layout.in_order() {
                return Ok(part.clone());
            }
        }
        let ext = ext_of(&parts[0]);
        let out = dir.join(format!("track{}.joined.{}", track, ext));
        let timelines = timelines(&layouts);
        let Some(ffmpeg) = ffmpeg.filter(|_| timelines.len() > 1) else {
            write_joined(&out, &inits[0], parts, &timelines.concat()).map_err(failed)?;
            return Ok(out);
        };
        let mut files = Vec::new();
        for (n, timeline) in timelines.iter().enumerate() {
            let file = dir.join(format!("track{}.timeline{}.{}", track, n, ext));
            write_joined(&file, &inits[0], parts, timeline).map_err(failed)?;
            files.push(file);
        }
        concat_copy(ffmpeg, &files, &out)?;
        return Ok(out);
    }
    let ffmpeg = ffmpeg.ok_or_else(|| "ffmpeg was not found".to_string())?;
    let mut inputs = Vec::new();
    for (n, ((part, layout), init)) in parts.iter().zip(&layouts).zip(&inits).enumerate() {
        if layout.in_order() {
            inputs.push(part.clone());
            continue;
        }
        // Each of its timelines a file of its own, which the concat filter plays one after another.
        for (t, timeline) in timelines(std::slice::from_ref(layout)).iter().enumerate() {
            let sorted = dir.join(format!("track{}.part{}.sorted{}.{}", track, n, t, ext_of(part)));
            write_joined(&sorted, init, std::slice::from_ref(part), timeline).map_err(failed)?;
            inputs.push(sorted);
        }
    }
    reencode(ffmpeg, &inputs, dir, track)
}

/// Joins `inputs`, files of one track alike but for their timestamps, one after another into
/// `out` without encoding them again: ffmpeg's concat demuxer starts each where the one before
/// ends. Any error fails it (`-xerror`).
fn concat_copy(ffmpeg: &Path, inputs: &[PathBuf], out: &Path) -> Result<(), String> {
    let list = out.with_extension("txt");
    // Quoted: a quote in a path closes the quotes, is escaped, and opens them again.
    let quoted = |input: &PathBuf| std::path::absolute(input).unwrap_or_else(|_| input.clone()).to_string_lossy().replace('\'', r"'\''");
    let text: String = inputs.iter().map(|input| format!("file '{}'\n", quoted(input))).collect();
    std::fs::write(&list, text).map_err(|e| format!("Cannot write {}: {}", list.display(), e))?;
    let mut args: Vec<OsString> = ["-y", "-nostdin", "-hide_banner", "-loglevel", "error", "-xerror", "-f", "concat", "-safe", "0", "-i"].map(OsString::from).into();
    args.extend([list.clone().into(), "-c".into(), "copy".into(), out.into()]);
    let joined = run_ffmpeg(ffmpeg, args);
    let _ = std::fs::remove_file(&list);
    joined
}

/// How a recorded part is laid out: its init segment, and the media after it as runs of bytes.
/// A fragmented MP4 as MediaSource takes it (ftyp and moov, then moof and mdat fragments) has a
/// run per fragment, keyed by its track and decode time, and its other boxes (sidx, mfra…),
/// whose offsets would be wrong once it is joined, are left out. A WebM's init segment runs up
/// to its first Cluster. Anything else is one run without an init segment.
struct Layout {
    /// The init segment's boxes, as (start, length).
    init: Vec<(u64, u64)>,
    runs: Vec<Run>,
}

/// Bytes of a part to copy: from `start`, `len` of them; `key` is a fragment's track and decode
/// time (see [`fragment_key`]).
struct Run {
    start: u64,
    len: u64,
    key: Option<(u32, u64)>,
}

impl Layout {
    fn of(path: &Path) -> io::Result<Layout> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut magic = [0; 4];
        if file.read_exact(&mut magic).is_ok() && magic == [0x1A, 0x45, 0xDF, 0xA3] {
            let mut head = vec![0; len.min(WEBM_HEAD) as usize];
            file.seek(SeekFrom::Start(0))?;
            file.read_exact(&mut head)?;
            if let Some(init) = webm_init_len(&head).map(|init| init as u64) {
                return Ok(Layout { init: vec![(0, init)], runs: vec![Run { start: init, len: len - init, key: None }] });
            }
        } else if let Some(layout) = fmp4_layout(&mut file, len)? {
            return Ok(layout);
        }
        Ok(Layout { init: Vec::new(), runs: vec![Run { start: 0, len, key: None }] })
    }

    fn init_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let mut init = Vec::new();
        for &(start, len) in &self.init {
            file.seek(SeekFrom::Start(start))?;
            (&mut file).take(len).read_to_end(&mut init)?;
        }
        Ok(init)
    }

    /// Its init segment starts it (see [`fmp4_layout`]) and its runs are one timeline in the order
    /// [`timelines`] puts them: nothing to change.
    fn in_order(&self) -> bool {
        let timelines = timelines(std::slice::from_ref(self));
        self.init.first().is_none_or(|&(start, _)| start == 0)
            && timelines.len() <= 1
            && timelines.concat().iter().map(|(_, run)| run.start).eq(self.runs.iter().map(|run| run.start))
    }
}

/// The runs of `layouts` (as the layout's index and the run) in the order to write them, by
/// timeline: as they are, one timeline, but for fragmented MP4 fragments, all of one track, which
/// go in decode-time order. A seek during the recording makes the player fetch fragments again
/// (the same bytes, which go), or out of order. A fragment that starts no later than the first of
/// its timeline starts a new one: a player that starts the clock again at a discontinuity
/// (sequence mode, a timestamp offset) appends such. Each timeline is put in order on its own,
/// without a second fragment at one decode time.
fn timelines(layouts: &[Layout]) -> Vec<Vec<(usize, &Run)>> {
    let runs: Vec<(usize, &Run)> = layouts.iter().enumerate().flat_map(|(n, layout)| layout.runs.iter().map(move |run| (n, run))).collect();
    let keys: Option<Vec<(u32, u64)>> = runs.iter().map(|(_, run)| run.key).collect();
    if !keys.is_some_and(|keys| keys.iter().all(|&(track, _)| track == keys[0].0)) {
        return vec![runs];
    }
    let mut seen = HashSet::new();
    let mut timelines: Vec<Vec<(usize, &Run)>> = Vec::new();
    for (n, run) in runs {
        if !seen.insert((run.key, run.len)) {
            continue;
        }
        match timelines.last_mut() {
            Some(timeline) if run.key > timeline[0].1.key => timeline.push((n, run)),
            _ => timelines.push(vec![(n, run)]),
        }
    }
    for timeline in &mut timelines {
        timeline.sort_by_key(|(_, run)| run.key);
        timeline.dedup_by_key(|(_, run)| run.key);
    }
    timelines
}

/// Writes `init`, then `runs` of `parts` (each as its part's index and the run, see
/// [`timelines`]), to `out`.
fn write_joined(out: &Path, init: &[u8], parts: &[PathBuf], runs: &[(usize, &Run)]) -> io::Result<()> {
    let files = parts.iter().map(File::open).collect::<io::Result<Vec<_>>>()?;
    let mut writer = BufWriter::new(File::create(out)?);
    writer.write_all(init)?;
    for &(n, run) in runs {
        let mut file = &files[n];
        file.seek(SeekFrom::Start(run.start))?;
        if io::copy(&mut file.take(run.len), &mut writer)? != run.len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    writer.flush()
}

/// The layout of `file` (`len` bytes) when it is a fragmented MP4 as MediaSource takes it: an
/// init segment (ftyp, moov), then fragments (moof, then its mdat). None for anything else,
/// such as a plain MP4 or samples outside any fragment. Each fragment's run spans its moof to its
/// last mdat, so the offsets in the moof stay right. A box cut short at the end (an append that
/// never finished) is left out, as is a moof without its mdat. Of init segments appended one after
/// another before any fragment (a quality switch before any media of the first), only the last,
/// the one the fragments belong to, is the init segment.
fn fmp4_layout(file: &mut File, len: u64) -> io::Result<Option<Layout>> {
    let (mut init, mut runs, mut pending) = (Vec::new(), Vec::<Run>::new(), None);
    let (mut at, mut init_has_moov) = (0, false);
    while let Some((kind, header, size)) = box_at(file, at, len)? {
        match &kind {
            _ if at == 0 && !matches!(&kind, b"ftyp" | b"styp" | b"moov" | b"moof" | b"sidx") => return Ok(None),
            b"ftyp" | b"moov" if runs.is_empty() && pending.is_none() => {
                if &kind == b"ftyp" || init_has_moov {
                    init.clear();
                }
                init_has_moov = &kind == b"moov";
                init.push((at, size));
            }
            b"moof" if size <= MAX_MOOF => {
                let mut moof = vec![0; (size - header) as usize];
                file.seek(SeekFrom::Start(at + header))?;
                file.read_exact(&mut moof)?;
                pending = Some(Run { start: at, len: size, key: fragment_key(&moof) });
            }
            b"mdat" => match (pending.take(), runs.last_mut()) {
                (Some(run), _) => runs.push(Run { len: at + size - run.start, ..run }),
                (None, Some(run)) => run.len = at + size - run.start,
                (None, None) => return Ok(None),
            },
            _ => {}
        }
        at += size;
    }
    Ok((!init.is_empty() && !runs.is_empty()).then_some(Layout { init, runs }))
}

/// The box at `at` of `file` (`len` bytes): its type, header size and size; None when no whole
/// box is left there.
fn box_at(file: &mut File, at: u64, len: u64) -> io::Result<Option<([u8; 4], u64, u64)>> {
    let left = len.saturating_sub(at);
    if left < 8 {
        return Ok(None);
    }
    let mut head = [0; 16];
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(&mut head[..8])?;
    let kind = [head[4], head[5], head[6], head[7]];
    let (header, size) = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
        0 => (8, left),
        1 if left >= 16 => {
            file.read_exact(&mut head[8..])?;
            (16, u64::from_be_bytes([head[8], head[9], head[10], head[11], head[12], head[13], head[14], head[15]]))
        }
        1 => return Ok(None),
        size => (8, u64::from(size)),
    };
    Ok((header <= size && size <= left).then_some((kind, header, size)))
}

/// The track and decode time (tfdt baseMediaDecodeTime, version 0 or 1) of the first track
/// fragment in `moof`, a moof box's content.
fn fragment_key(moof: &[u8]) -> Option<(u32, u64)> {
    let traf = children(moof).find(|(kind, _)| kind == b"traf")?.1;
    let tfhd = children(traf).find(|(kind, _)| kind == b"tfhd")?.1;
    let tfdt = children(traf).find(|(kind, _)| kind == b"tfdt")?.1;
    let track = u32::from_be_bytes(tfhd.get(4..8)?.try_into().ok()?);
    let time = match tfdt.first()? {
        0 => u32::from_be_bytes(tfdt.get(4..8)?.try_into().ok()?).into(),
        1 => u64::from_be_bytes(tfdt.get(4..12)?.try_into().ok()?),
        _ => return None,
    };
    Some((track, time))
}

/// The boxes in `data`, as their type and content, up to one cut short.
fn children(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let rest = data.get(at..)?;
        let kind: [u8; 4] = rest.get(4..8)?.try_into().ok()?;
        let (header, size) = match u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) {
            0 => (8, rest.len()),
            1 => (16, usize::try_from(u64::from_be_bytes(rest.get(8..16)?.try_into().ok()?)).ok()?),
            size => (8, size as usize),
        };
        let content = rest.get(header..size)?;
        at += size;
        Some((kind, content))
    })
}

/// Where the first Cluster of the WebM starting with `head` begins: the length of its init
/// segment (EBML header, then the Segment's elements before it). None when it is not found in
/// `head`, or an element before it has an unknown size.
fn webm_init_len(head: &[u8]) -> Option<usize> {
    const EBML: u32 = 0x1A45_DFA3;
    const SEGMENT: u32 = 0x1853_8067;
    const CLUSTER: u32 = 0x1F43_B675;
    let (id, size, content) = ebml_element(head, 0)?;
    if id != EBML {
        return None;
    }
    let (id, _, mut at) = ebml_element(head, content + usize::try_from(size?).ok()?)?;
    if id != SEGMENT {
        return None;
    }
    loop {
        let (id, size, content) = ebml_element(head, at)?;
        if id == CLUSTER {
            return Some(at);
        }
        at = content.checked_add(usize::try_from(size?).ok()?)?;
    }
}

/// The EBML element at `at` of `data`: its ID (with its length marker), its size (None when
/// unknown) and where its content starts.
fn ebml_element(data: &[u8], at: usize) -> Option<(u32, Option<u64>, usize)> {
    let first = *data.get(at)?;
    let id_len = first.leading_zeros() as usize + 1;
    if id_len > 4 {
        return None;
    }
    let id = data.get(at..at + id_len)?.iter().fold(0u32, |id, &b| id << 8 | u32::from(b));
    let size_at = at + id_len;
    let first = *data.get(size_at)?;
    let size_len = first.leading_zeros() as usize + 1;
    if size_len > 8 {
        return None;
    }
    let bytes = data.get(size_at..size_at + size_len)?;
    // The length marker goes; all ones left means unknown.
    let size = bytes[1..].iter().fold(u64::from(first) & (0xFF >> size_len), |size, &b| size << 8 | u64::from(b));
    let unknown = (1u64 << (7 * size_len)) - 1;
    Some((id, (size != unknown).then_some(size), size_at + size_len))
}

/// Encodes `inputs`, the parts of track `track`, again into one file in `dir`,
/// `track<K>.joined.mp4` (`.m4a` without video): joined with ffmpeg's concat filter, the video
/// scaled to the largest part's size, padded to keep its shape, as H.264, the audio as AAC. Only
/// the kinds of stream every part has are kept.
fn reencode(ffmpeg: &Path, inputs: &[PathBuf], dir: &Path, track: u8) -> Result<PathBuf, String> {
    let (mut size, mut video, mut audio) = ((0, 0), true, true);
    for input in inputs {
        let (part_video, part_audio) = stream_info(&probe(ffmpeg, input)?);
        video &= part_video.is_some();
        audio &= part_audio;
        if let Some((width, height)) = part_video.filter(|&(w, h)| w * h > size.0 * size.1) {
            size = (width, height);
        }
    }
    if video && size.0 * size.1 == 0 {
        return Err("ffmpeg did not say the size of the video".to_string());
    }
    let graph = concat_graph(inputs.len(), video.then_some(size), audio).ok_or("the parts have no video or audio in common")?;
    let out = dir.join(format!("track{}.joined.{}", track, if video { "mp4" } else { "m4a" }));
    let mut args: Vec<OsString> = ["-y", "-nostdin", "-hide_banner", "-loglevel", "error"].map(OsString::from).into();
    for input in inputs {
        args.extend(["-i".into(), input.into()]);
    }
    args.extend(["-filter_complex".into(), graph.into()]);
    if video {
        args.extend(["-map", "[v]", "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p"].map(OsString::from));
    }
    if audio {
        args.extend(["-map", "[a]", "-c:a", "aac", "-b:a", "192k"].map(OsString::from));
    }
    args.push(out.clone().into());
    run_ffmpeg(ffmpeg, args).inspect_err(|_| {
        let _ = std::fs::remove_file(&out);
    })?;
    Ok(out)
}

/// ffmpeg's concat filter over `n` inputs, with their video scaled and padded to `size` (rounded
/// up to even, as H.264 wants) when there is video, and their audio when `audio`; its outputs are
/// `[v]` and `[a]`. None with neither.
fn concat_graph(n: usize, size: Option<(u32, u32)>, audio: bool) -> Option<String> {
    if size.is_none() && !audio {
        return None;
    }
    let mut graph = String::new();
    if let Some((width, height)) = size {
        let (w, h) = (width + width % 2, height + height % 2);
        for i in 0..n {
            graph.push_str(&format!(
                "[{i}:v:0]scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1[v{i}];"
            ));
        }
    }
    for i in 0..n {
        if size.is_some() {
            graph.push_str(&format!("[v{i}]"));
        }
        if audio {
            graph.push_str(&format!("[{i}:a:0]"));
        }
    }
    graph.push_str(&format!("concat=n={}:v={}:a={}", n, u8::from(size.is_some()), u8::from(audio)));
    graph.push_str(match (size.is_some(), audio) {
        (true, true) => "[v][a]",
        (true, false) => "[v]",
        _ => "[a]",
    });
    Some(graph)
}

/// What ffmpeg says of `input` when asked to read it (its streams), on stderr.
fn probe(ffmpeg: &Path, input: &Path) -> Result<String, String> {
    let mut command = ffmpeg_command(ffmpeg);
    command.args(["-hide_banner", "-nostdin", "-i"]).arg(input);
    // It fails for want of an output, having said what it found.
    let output = command.output().map_err(|e| format!("ffmpeg could not run: {}", e))?;
    Ok(String::from_utf8_lossy(&output.stderr).into_owned())
}

/// From what ffmpeg said of a file (see [`probe`]): the size of its first video stream ((0, 0)
/// when it does not say) if it has one, and whether it has audio.
fn stream_info(said: &str) -> (Option<(u32, u32)>, bool) {
    let streams = || said.lines().map(str::trim_start).filter(|line| line.starts_with("Stream #"));
    let video = streams().find(|line| line.contains(": Video:")).map(|line| {
        let mut sizes = line.split([' ', ',']).filter_map(|word| {
            let (w, h) = word.trim_matches(|c: char| !c.is_ascii_digit()).split_once('x')?;
            Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
        });
        // "0x31637661" is a codec tag, not a size.
        sizes.find(|&(w, h)| (1..=16384).contains(&w) && (1..=16384).contains(&h)).unwrap_or((0, 0))
    });
    (video, streams().any(|line| line.contains(": Audio:")))
}

fn ffmpeg_command(ffmpeg: &Path) -> Command {
    let mut command = Command::new(ffmpeg);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashes up over the app.
        command.creation_flags(0x0800_0000);
    }
    command
}

/// Runs `ffmpeg` with `args`; the error says what it said.
fn run_ffmpeg(ffmpeg: &Path, args: Vec<OsString>) -> Result<(), String> {
    let output = ffmpeg_command(ffmpeg).args(args).output().map_err(|e| format!("ffmpeg could not run: {}", e))?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    Err(format!("ffmpeg failed: {}", ingest::truncate_chars(said.trim(), 300)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_are_named_and_joined_by_their_mime() {
        for (mime, ext) in [
            ("video/webm; codecs=\"vp9,opus\"", "webm"),
            ("audio/webm;codecs=opus", "webm"),
            ("audio/mp4; codecs=\"mp4a.40.2\"", "m4a"),
            ("video/mp4;codecs=avc1,mp4a.40.2", "mp4"),
            ("Audio/MPEG", "mp3"),
            ("video/mp2t; codecs=\"avc1.64001f\"", "ts"),
            ("audio/aac", "aac"),
            ("audio/x-aac", "aac"),
            ("", "bin"),
            ("video/x-unknown", "bin"),
        ] {
            assert_eq!(ext_for_mime(mime), ext, "{mime}");
        }
        assert_eq!(container_for(&["webm", "webm"]), "webm");
        assert_eq!(container_for(&["mp4", "m4a"]), "mp4");
        assert_eq!(container_for(&["mp4"]), "mp4");
        assert_eq!(container_for(&["ts", "m4a"]), "mp4");
        assert_eq!(container_for(&["ts", "aac"]), "mp4");
        assert_eq!(container_for(&["ts"]), "mp4");
        assert_eq!(container_for(&["mp4", "webm"]), "mkv");
        assert_eq!(container_for(&["mp3"]), "mkv");
        assert_eq!(container_for(&["ts", "webm"]), "mkv");
    }

    #[test]
    fn ffmpeg_copies_every_track_into_the_output() {
        let args = |inputs: &[&str], out: &str| {
            let inputs: Vec<PathBuf> = inputs.iter().map(PathBuf::from).collect();
            ffmpeg_args(&inputs, Path::new(out)).into_iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
        };
        assert_eq!(
            args(&["t/track0.mp4", "t/track1.m4a"], "d/A.mp4"),
            "-y -nostdin -hide_banner -loglevel error -i t/track0.mp4 -i t/track1.m4a -map 0:v? -map 0:a? -map 1:v? -map 1:a? -c copy -movflags +faststart d/A.mp4"
        );
        assert_eq!(
            args(&["t/track0.webm"], "d/A.webm"),
            "-y -nostdin -hide_banner -loglevel error -i t/track0.webm -map 0:v? -map 0:a? -c copy d/A.webm"
        );
    }

    #[test]
    fn a_taken_or_claimed_name_is_numbered() {
        let dir = tempfile::tempdir().unwrap();
        let free = |ext| free_path(dir.path(), "Talk", ext).unwrap().0;
        assert_eq!(free("mp4"), dir.path().join("Talk.mp4"));
        std::fs::write(dir.path().join("Talk.mp4"), b"").unwrap();
        assert_eq!(free("mp4"), dir.path().join("Talk (1).mp4"));
        // Claimed by a download, or a recording finishing at the same time, but not written yet.
        let (claimed, _claim) = free_path(dir.path(), "Talk", "mp4").unwrap();
        assert_eq!(claimed, dir.path().join("Talk (1).mp4"));
        assert_eq!(free("mp4"), dir.path().join("Talk (2).mp4"));
        assert_eq!(free("webm"), dir.path().join("Talk.webm"));
    }

    fn finished(title: &str, dir: &Path, tracks: Vec<(u8, Vec<PathBuf>)>) -> Finished {
        Finished { title: title.into(), dir: dir.to_path_buf(), tracks, recovered: false }
    }

    /// Without ffmpeg, or when it fails, the tracks are moved into the save folder as they are,
    /// under the cleaned title, and the temporary folder goes.
    #[test]
    fn tracks_ffmpeg_cannot_join_are_saved_as_they_are() {
        let save = tempfile::tempdir().unwrap();
        for ffmpeg in [None, Some(save.path().join("no-such-ffmpeg.exe"))] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("rec");
            std::fs::create_dir(&dir).unwrap();
            let (video, audio) = (dir.join("track0.part0.mp4"), dir.join("track1.part0.m4a"));
            std::fs::write(&video, b"video").unwrap();
            std::fs::write(&audio, b"audio").unwrap();
            let notice = merge(&finished("A: talk?", &dir, vec![(0, vec![video]), (1, vec![audio])]), save.path(), ffmpeg.as_deref()).unwrap_err();
            assert!(notice.contains("ffmpeg is needed to join them"), "{notice}");
            assert!(!dir.exists(), "the temporary folder is deleted");
        }
        let mut names: Vec<String> = std::fs::read_dir(save.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["A_ talk_.track0 (1).mp4", "A_ talk_.track0.mp4", "A_ talk_.track1 (1).m4a", "A_ talk_.track1.m4a"]);
        assert_eq!(std::fs::read(save.path().join("A_ talk_.track1.m4a")).unwrap(), b"audio");

        // No save folder: nothing is lost.
        let temp = tempfile::tempdir().unwrap();
        assert!(merge(&finished("", temp.path(), Vec::new()), Path::new(""), None).unwrap_err().contains("kept in"));
        assert!(temp.path().exists());
    }

    /// Chunks are appended to each part's file in order, the first chunk's mime naming it; a
    /// small recording is deleted when finished, a big one handed over, an aborted one deleted.
    #[tokio::test]
    async fn chunks_are_kept_per_track_and_part_until_the_recording_ends() {
        let root = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(root.path().to_path_buf());
        let id = recordings.start("Talk", "https://page.example/watch").await.unwrap();
        assert!(valid_id(&id), "{id}");
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(root.path().join(&id).join(META)).unwrap()).unwrap();
        assert_eq!((meta["title"].as_str(), meta["page_url"].as_str()), (Some("Talk"), Some("https://page.example/watch")));
        assert!(meta["started"].as_u64().is_some_and(|t| t > 1_700_000_000), "{meta}");
        assert_eq!(recordings.append(&id, 0, 0, "video/webm", b"ab").await, Ok(2));
        assert_eq!(recordings.append(&id, 0, 0, "video/mp4", b"cd").await, Ok(4), "the first mime names the file");
        assert_eq!(recordings.append(&id, 0, 1, "video/mp4", b"efg").await, Ok(7), "the track's parts together");
        assert_eq!(recordings.append(&id, 1, 0, "audio/mp4", &[7; 70 * 1024]).await, Ok(70 * 1024));
        assert_eq!(recordings.append("nope", 0, 0, "video/webm", b"x").await, Err(Refusal::Unknown));
        let finished = recordings.finish(&id).await.unwrap().unwrap();
        assert_eq!(finished.title, "Talk");
        let dir = root.path().join(&id);
        assert_eq!(
            finished.tracks,
            [(0, vec![dir.join("track0.part0.webm"), dir.join("track0.part1.mp4")]), (1, vec![dir.join("track1.part0.m4a")])]
        );
        assert_eq!(std::fs::read(&finished.tracks[0].1[0]).unwrap(), b"abcd");
        assert!(!finished.recovered);
        assert_eq!(recordings.finish(&id).await.unwrap_err(), Refusal::Unknown, "finished once");

        let small = recordings.start("Small", "").await.unwrap();
        recordings.append(&small, 0, 0, "video/webm", b"tiny").await.unwrap();
        assert!(recordings.finish(&small).await.unwrap().is_none());
        assert!(!root.path().join(&small).exists());

        let aborted = recordings.start("Aborted", "").await.unwrap();
        recordings.append(&aborted, 0, 0, "video/webm", b"x").await.unwrap();
        recordings.abort(&aborted).await.unwrap();
        assert!(!root.path().join(&aborted).exists());
        assert_eq!(recordings.abort(&aborted).await, Err(Refusal::Unknown));
    }

    #[tokio::test]
    async fn only_so_many_recordings_are_open_at_once() {
        let root = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(root.path().to_path_buf());
        let mut ids = Vec::new();
        for _ in 0..MAX_OPEN {
            ids.push(recordings.start("t", "").await.unwrap());
        }
        assert_eq!(recordings.start("t", "").await, Err(Refusal::Limit("too many recordings at once")));
        recordings.abort(&ids[0]).await.unwrap();
        assert!(recordings.start("t", "").await.is_ok());
    }

    /// Once too many are open, those a browser left behind make room: what they recorded is
    /// handed over to be joined, not deleted; one too small to keep is deleted.
    #[tokio::test]
    async fn recordings_left_open_are_joined_to_make_room() {
        let root = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(root.path().to_path_buf());
        let mut ids = Vec::new();
        for _ in 0..MAX_OPEN {
            ids.push(recordings.start("t", "").await.unwrap());
        }
        recordings.append(&ids[0], 0, 0, "video/webm", &[1; 70 * 1024]).await.unwrap();
        recordings.append(&ids[1], 0, 0, "video/webm", b"tiny").await.unwrap();
        for id in &ids[..2] {
            recordings.open.lock().await.get_mut(id).unwrap().last_chunk = Instant::now() - STALE;
        }
        let closed = recordings.close_stale().await;
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].dir, root.path().join(&ids[0]));
        assert!(closed[0].dir.exists(), "kept to be joined");
        assert!(!root.path().join(&ids[1]).exists());
        assert!(recordings.close_stale().await.is_empty(), "room was made");
        assert!(recordings.start("t", "").await.is_ok());
    }

    /// What an earlier run left is handed over to be joined, titled by its meta.json, its parts
    /// in order; one too small is deleted; a folder a recording open now owns, one written to
    /// lately (another copy of the app recording), and anything that is no recording's folder,
    /// are left alone.
    #[tokio::test]
    async fn recordings_an_earlier_run_left_are_recovered() {
        let root = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(root.path().to_path_buf());
        let live = recordings.start("Live", "").await.unwrap();
        recordings.append(&live, 0, 0, "video/webm", &[1; 70 * 1024]).await.unwrap();
        let left = root.path().join("0abc");
        std::fs::create_dir(&left).unwrap();
        std::fs::write(left.join(META), br#"{"title":"Left","page_url":"https://p.example/","started":1}"#).unwrap();
        for (name, size) in [("track0.part1.mp4", 40 * 1024), ("track0.part0.mp4", 40 * 1024), ("track1.m4a", 10), ("track0.joined.mp4", 5), ("notes.txt", 5)] {
            std::fs::write(left.join(name), vec![0; size]).unwrap();
        }
        let untitled = root.path().join("1abc");
        std::fs::create_dir(&untitled).unwrap();
        std::fs::write(untitled.join("track2.part0.webm"), vec![0; 70 * 1024]).unwrap();
        let small = root.path().join("2abc");
        std::fs::create_dir(&small).unwrap();
        std::fs::write(small.join("track0.part0.webm"), b"tiny").unwrap();
        let other = root.path().join("not-a-recording");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(root.path().join("3abc"), b"a file").unwrap();
        // Written before STALE, but for the folder another copy of the app records into now.
        let long_ago = SystemTime::now() - 2 * STALE;
        for dir in [&left, &untitled, &small] {
            for entry in std::fs::read_dir(dir).unwrap() {
                File::options().write(true).open(entry.unwrap().path()).unwrap().set_modified(long_ago).unwrap();
            }
        }
        let busy = root.path().join("4abc");
        std::fs::create_dir(&busy).unwrap();
        std::fs::write(busy.join("track0.part0.webm"), vec![0; 70 * 1024]).unwrap();

        let mut found = recordings.recover().await;
        found.sort_by(|a, b| a.dir.cmp(&b.dir));
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|f| f.recovered));
        assert_eq!(found[0].title, "Left");
        assert_eq!(found[0].tracks, [(0, vec![left.join("track0.part0.mp4"), left.join("track0.part1.mp4")]), (1, vec![left.join("track1.m4a")])]);
        assert_eq!((found[1].title.as_str(), &found[1].tracks[..]), ("", &[(2, vec![untitled.join("track2.part0.webm")])][..]));
        assert!(!small.exists() && other.exists() && root.path().join("3abc").exists() && busy.join("track0.part0.webm").exists());
        assert!(root.path().join(&live).exists());
        assert!(recordings.finish(&live).await.unwrap().is_some(), "the open recording is still open");
    }

    #[test]
    fn ids_are_letters_and_digits() {
        assert!(valid_id("a1B2") && valid_id(&"a".repeat(32)));
        for bad in ["", "..", "a/b", "a\\b", "a.b", "é", &"a".repeat(33)] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert_eq!(part_of("track3.part255.webm"), Some((3, 255)));
        assert_eq!(part_of("track1.m4a"), Some((1, 0)));
        for other in ["track16.part0.mp4", "track0.part256.mp4", "track0.joined.mp4", "track0.part1.sorted.mp4", "meta.json", "track0"] {
            assert_eq!(part_of(other), None, "{other}");
        }
    }

    /// A box of `kind` holding `content`.
    fn mp4_box(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        [&(8 + content.len() as u32).to_be_bytes()[..], kind, content].concat()
    }

    /// A moof of track `track` whose tfdt (version 0 or 1) says `time`.
    fn moof(track: u32, time: u64, version: u8) -> Vec<u8> {
        let tfhd = mp4_box(b"tfhd", &[&[0, 2, 0, 0][..], &track.to_be_bytes()].concat());
        let time = if version == 0 { (time as u32).to_be_bytes().to_vec() } else { time.to_be_bytes().to_vec() };
        let tfdt = mp4_box(b"tfdt", &[&[version, 0, 0, 0][..], &time].concat());
        mp4_box(b"moof", &[mp4_box(b"mfhd", &[0; 8]), mp4_box(b"traf", &[tfhd, tfdt, mp4_box(b"trun", &[0; 8])].concat())].concat())
    }

    #[test]
    fn a_fragments_track_and_decode_time_are_read_from_its_moof() {
        assert_eq!(fragment_key(&moof(1, 90_000, 0)[8..]), Some((1, 90_000)));
        assert_eq!(fragment_key(&moof(2, 1 << 40, 1)[8..]), Some((2, 1 << 40)));
        assert_eq!(fragment_key(&mp4_box(b"mfhd", &[0; 8])), None, "no traf");
        let mut cut = moof(1, 5, 1);
        cut.truncate(cut.len() - 12);
        assert_eq!(fragment_key(&cut[8..]), None, "a box cut short");
    }

    /// A fragmented MP4 is read as its init segment and fragments; repeats go and the rest is
    /// put in decode-time order once all are of one track; anything else is left as it is.
    #[test]
    fn fragments_are_put_in_time_order_once_each() {
        let dir = tempfile::tempdir().unwrap();
        let init = [mp4_box(b"ftyp", b"iso6\0\0\0\0"), mp4_box(b"moov", &[1; 20])].concat();
        let fragment = |time: u64| [moof(1, time, 1), mp4_box(b"mdat", &time.to_be_bytes())].concat();
        let file = [init.clone(), fragment(0), mp4_box(b"sidx", &[0; 12]), fragment(2000), fragment(1000), fragment(2000), mp4_box(b"mdat", &[9; 3])].concat();
        let path = dir.path().join("p.mp4");
        std::fs::write(&path, &file).unwrap();
        let layout = Layout::of(&path).unwrap();
        assert_eq!(layout.init_bytes(&path).unwrap(), init);
        assert_eq!(layout.runs.iter().map(|run| run.key).collect::<Vec<_>>(), [0, 2000, 1000, 2000].map(|t| Some((1, t))));
        assert!(!layout.in_order());
        let out = dir.path().join("out.mp4");
        write_joined(&out, &init, std::slice::from_ref(&path), &timelines(std::slice::from_ref(&layout)).concat()).unwrap();
        // The sidx goes; the stray mdat at the end belongs to the repeated fragment, which goes.
        assert_eq!(std::fs::read(&out).unwrap(), [init.clone(), fragment(0), fragment(1000), fragment(2000)].concat());
        assert!(Layout::of(&out).unwrap().in_order());

        // Fragments of two tracks in one file keep their order.
        let two = [init.clone(), fragment(1000), moof(2, 0, 0), mp4_box(b"mdat", b"a")].concat();
        std::fs::write(&path, &two).unwrap();
        assert!(Layout::of(&path).unwrap().in_order());
        // Plain MP4, MPEG-TS, a cut-short box, a moof without mdat.
        for (bytes, runs) in [
            ([init.clone(), mp4_box(b"mdat", &[0; 9])].concat(), 1),
            (vec![0x47; 188 * 2], 1),
            ([init.clone(), fragment(0), vec![0, 0, 1, 0, b'm', b'o', b'o', b'f']].concat(), 1),
            ([init.clone(), fragment(0), moof(1, 7, 1)].concat(), 1),
        ] {
            std::fs::write(&path, &bytes).unwrap();
            let layout = Layout::of(&path).unwrap();
            assert_eq!(layout.runs.len(), runs);
            assert!(layout.in_order());
        }
    }

    /// A player that starts the clock again (a second timeline of fragments from decode time 0)
    /// keeps both timelines, one after the other, each in order; only a fragment fetched again
    /// whole (same time, same size) goes. Such a part is not one to copy as it is.
    #[test]
    fn a_timeline_started_again_is_kept_after_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let init = [mp4_box(b"ftyp", b"iso6\0\0\0\0"), mp4_box(b"moov", &[1; 20])].concat();
        let fragment = |time: u64, size: usize| [moof(1, time, 1), mp4_box(b"mdat", &vec![time as u8; size])].concat();
        let (a0, a1, a2) = (fragment(0, 4), fragment(1000, 4), fragment(2000, 4));
        let (b0, b1) = (fragment(0, 7), fragment(1000, 7));
        let file = [init.clone(), a0.clone(), a2.clone(), a1.clone(), b0.clone(), a1.clone(), b1.clone()].concat();
        let path = dir.path().join("p.mp4");
        std::fs::write(&path, &file).unwrap();
        let layout = Layout::of(&path).unwrap();
        let timelines = timelines(std::slice::from_ref(&layout));
        assert_eq!(timelines.len(), 2);
        let out = dir.path().join("out.mp4");
        write_joined(&out, &init, std::slice::from_ref(&path), &timelines.concat()).unwrap();
        std::fs::write(&path, [init.clone(), a0.clone(), a1.clone(), b0.clone()].concat()).unwrap();
        assert!(!Layout::of(&path).unwrap().in_order());
        assert_eq!(std::fs::read(&out).unwrap(), [init, a0, a1, a2, b0, b1].concat());
    }

    /// Of two init segments appended before any fragment, the second, the fragments' own, is the
    /// part's init segment; the part is rewritten without the first.
    #[test]
    fn only_the_last_init_segment_before_the_fragments_counts() {
        let dir = tempfile::tempdir().unwrap();
        let first = [mp4_box(b"ftyp", b"iso6\0\0\0\0"), mp4_box(b"moov", &[1; 20])].concat();
        let second = [mp4_box(b"ftyp", b"iso6\0\0\0\0"), mp4_box(b"moov", &[2; 30])].concat();
        let fragment = [moof(1, 0, 1), mp4_box(b"mdat", &[5; 4])].concat();
        let path = dir.path().join("p.mp4");
        for (file, init) in [
            ([first.clone(), second.clone(), fragment.clone()].concat(), second.clone()),
            ([first.clone(), mp4_box(b"moov", &[3; 9]), fragment.clone()].concat(), mp4_box(b"moov", &[3; 9])),
        ] {
            std::fs::write(&path, &file).unwrap();
            let layout = Layout::of(&path).unwrap();
            assert_eq!(layout.init_bytes(&path).unwrap(), init);
            assert!(!layout.in_order(), "copied as it is, it would start with the other init");
        }
        let layout = Layout::of(&path).unwrap();
        let out = dir.path().join("out.mp4");
        write_joined(&out, &layout.init_bytes(&path).unwrap(), std::slice::from_ref(&path), &timelines(std::slice::from_ref(&layout)).concat()).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), [mp4_box(b"moov", &[3; 9]), fragment].concat());
    }

    #[test]
    fn webm_init_segments_end_at_the_first_cluster() {
        let element = |id: &[u8], content: &[u8]| [id, &[0x80 | content.len() as u8][..], content].concat();
        let header = element(&[0x1A, 0x45, 0xDF, 0xA3], &element(&[0x42, 0x82], b"webm"));
        let info = element(&[0x15, 0x49, 0xA9, 0x66], &[0; 6]);
        let tracks = element(&[0x16, 0x54, 0xAE, 0x6B], &[0; 10]);
        // The Segment's size is unknown, as MediaRecorder writes it.
        let segment = [&[0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF][..], &info, &tracks].concat();
        let cluster = element(&[0x1F, 0x43, 0xB6, 0x75], &[0; 4]);
        let file = [header.clone(), segment.clone(), cluster.clone()].concat();
        assert_eq!(webm_init_len(&file), Some(header.len() + segment.len()));
        assert_eq!(webm_init_len(&[header.clone(), segment.clone()].concat()), None, "no Cluster yet");
        assert_eq!(webm_init_len(&[0x47; 64]), None);
        // An element of unknown size before the Cluster cannot be skipped.
        let unknown = [&[0x18, 0x53, 0x80, 0x67, 0xFF][..], &[0x15, 0x49, 0xA9, 0x66, 0xFF], &cluster].concat();
        assert_eq!(webm_init_len(&[header, unknown].concat()), None);
    }

    #[test]
    fn the_concat_filter_scales_and_pads_to_one_size() {
        assert_eq!(
            concat_graph(2, Some((853, 480)), true).unwrap(),
            "[0:v:0]scale=854:480:force_original_aspect_ratio=decrease,pad=854:480:(ow-iw)/2:(oh-ih)/2,setsar=1[v0];\
             [1:v:0]scale=854:480:force_original_aspect_ratio=decrease,pad=854:480:(ow-iw)/2:(oh-ih)/2,setsar=1[v1];\
             [v0][0:a:0][v1][1:a:0]concat=n=2:v=1:a=1[v][a]"
        );
        assert_eq!(concat_graph(2, None, true).unwrap(), "[0:a:0][1:a:0]concat=n=2:v=0:a=1[a]");
        assert!(concat_graph(2, Some((2, 2)), false).unwrap().ends_with("[v0][v1]concat=n=2:v=1:a=0[v]"));
        assert_eq!(concat_graph(2, None, false), None);

        let said = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'a.mp4':\n  Duration: 00:00:03.00\n  \
                    Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 640x360 [SAR 1:1 DAR 16:9], 25 fps\n  \
                    Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), 44100 Hz, stereo, fltp\n";
        assert_eq!(stream_info(said), (Some((640, 360)), true));
        assert_eq!(stream_info("  Stream #0:0: Audio: opus, 48000 Hz, stereo"), (None, true));
        assert_eq!(stream_info("  Stream #0:0: Video: h264 (avc1 / 0x31637661)"), (Some((0, 0)), false));
    }

    /// Runs ffmpeg with `args`, panicking with what it said if it fails.
    fn ffmpeg_ok(ffmpeg: &Path, args: &[&str]) {
        let args: Vec<OsString> = ["-y", "-nostdin", "-hide_banner", "-loglevel", "error"].iter().chain(args).map(OsString::from).collect();
        run_ffmpeg(ffmpeg, args).unwrap();
    }

    /// A test video of `seconds` at `size` (and a tone when `audio`), as a fragmented MP4 cut
    /// into one-second fragments, as MediaSource takes it.
    fn fragmented(ffmpeg: &Path, out: &Path, size: &str, seconds: u32, audio: bool) {
        let (video, length) = (format!("testsrc=size={}:rate=25", size), seconds.to_string());
        let mut args = vec!["-f", "lavfi", "-i", &video];
        if audio {
            args.extend(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100"]);
        }
        args.extend(["-t", &length, "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "25", "-c:a", "aac"]);
        args.extend(["-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4", out.to_str().unwrap()]);
        ffmpeg_ok(ffmpeg, &args);
    }

    /// The duration ffmpeg reads in `file`, in seconds, and the size of its video.
    fn duration_and_size(ffmpeg: &Path, file: &Path) -> (f64, Option<(u32, u32)>) {
        let said = probe(ffmpeg, file).unwrap();
        let at = said.find("Duration: ").expect(&said) + "Duration: ".len();
        let hms: Vec<f64> = said[at..at + 11].split(':').map(|n| n.parse().unwrap()).collect();
        (hms[0] * 3600.0 + hms[1] * 60.0 + hms[2], stream_info(&said).0)
    }

    /// Fragments of a real ffmpeg fragmented MP4 recorded out of order and twice (a seek), in
    /// two parts with the same init segment, are joined into the whole video once; a separate
    /// audio track is joined with it into one MP4.
    #[test]
    fn parts_with_one_init_segment_are_joined_in_time_order() {
        let Some(ffmpeg) = hyperfetch_core::media::find_ffmpeg_path() else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let whole = dir.path().join("whole.mp4");
        fragmented(&ffmpeg, &whole, "320x240", 4, false);
        let layout = Layout::of(&whole).unwrap();
        let keys: Vec<_> = layout.runs.iter().map(|run| run.key.unwrap()).collect();
        assert_eq!(keys.len(), 4, "one fragment a second: {keys:?}");
        assert!(keys.windows(2).all(|w| w[0].0 == w[1].0 && w[0].1 < w[1].1), "{keys:?}");
        let bytes = std::fs::read(&whole).unwrap();
        let init = layout.init_bytes(&whole).unwrap();
        let run = |n: usize| &bytes[layout.runs[n].start as usize..(layout.runs[n].start + layout.runs[n].len) as usize];

        let rec = dir.path().join("rec");
        std::fs::create_dir(&rec).unwrap();
        let parts = [rec.join("track0.part0.mp4"), rec.join("track0.part1.mp4")];
        std::fs::write(&parts[0], [&init[..], run(0), run(2), run(3)].concat()).unwrap();
        std::fs::write(&parts[1], [&init[..], run(2), run(1)].concat()).unwrap();
        let audio = rec.join("track1.part0.m4a");
        let tone = dir.path().join("tone.m4a");
        ffmpeg_ok(&ffmpeg, &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100", "-t", "4", "-c:a", "aac", "-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4", tone.to_str().unwrap()]);
        std::fs::copy(&tone, &audio).unwrap();

        let joined = join_track(None, &rec, 0, &parts).unwrap();
        assert_eq!(std::fs::read(&joined).unwrap(), [&init[..], run(0), run(1), run(2), run(3)].concat());
        assert_eq!(join_track(None, &rec, 1, std::slice::from_ref(&audio)).unwrap(), audio, "nothing to change");

        let save = dir.path().join("save");
        let notice = merge(&finished("Talk", &rec, vec![(0, parts.to_vec()), (1, vec![audio])]), &save, Some(&ffmpeg)).unwrap();
        let out = save.join("Talk.mp4");
        assert_eq!(notice, format!("Saved the recording as {}", out.display()));
        let (seconds, size) = duration_and_size(&ffmpeg, &out);
        assert!((3.9..4.2).contains(&seconds), "{seconds}");
        assert_eq!(size, Some((320, 240)));
        assert!(stream_info(&probe(&ffmpeg, &out).unwrap()).1, "with the audio");
        assert!(!rec.exists());
    }

    /// A player that started its clock again (fragments from decode time 0 after others, under
    /// one init segment) has its timelines joined one after the other by ffmpeg: the track plays
    /// as long as both. Needs ffmpeg.
    #[test]
    fn timelines_started_again_play_one_after_the_other() {
        let Some(ffmpeg) = hyperfetch_core::media::find_ffmpeg_path() else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("a.mp4"), dir.path().join("b.mp4"));
        fragmented(&ffmpeg, &first, "320x240", 2, false);
        let flags = ["-t", "2", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "25", "-movflags", "frag_keyframe+empty_moov+default_base_moof"];
        ffmpeg_ok(&ffmpeg, &[&["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25"][..], &flags[..], &["-f", "mp4", second.to_str().unwrap()][..]].concat());
        let (a, b) = (Layout::of(&first).unwrap(), Layout::of(&second).unwrap());
        let init = a.init_bytes(&first).unwrap();
        assert_eq!(init, b.init_bytes(&second).unwrap(), "one init segment");
        let media = |path: &Path, layout: &Layout| std::fs::read(path).unwrap()[layout.runs[0].start as usize..].to_vec();
        let rec = dir.path().join("rec");
        std::fs::create_dir(&rec).unwrap();
        let part = rec.join("track0.part0.mp4");
        std::fs::write(&part, [init, media(&first, &a), media(&second, &b)].concat()).unwrap();
        let joined = join_track(Some(&ffmpeg), &rec, 0, std::slice::from_ref(&part)).unwrap();
        let (seconds, size) = duration_and_size(&ffmpeg, &joined);
        assert!((3.9..4.2).contains(&seconds), "{seconds}");
        assert_eq!(size, Some((320, 240)));
    }

    /// Of two init segments appended back to back, the media's own (the second) is the one its
    /// track is read with: it plays at its size. Needs ffmpeg.
    #[test]
    fn a_part_is_read_with_its_last_init_segment() {
        let Some(ffmpeg) = hyperfetch_core::media::find_ffmpeg_path() else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (small, large) = (dir.path().join("small.mp4"), dir.path().join("large.mp4"));
        fragmented(&ffmpeg, &small, "64x48", 1, false);
        fragmented(&ffmpeg, &large, "320x240", 2, false);
        let (s, l) = (Layout::of(&small).unwrap(), Layout::of(&large).unwrap());
        let media = std::fs::read(&large).unwrap()[l.runs[0].start as usize..].to_vec();
        let rec = dir.path().join("rec");
        std::fs::create_dir(&rec).unwrap();
        let part = rec.join("track0.part0.mp4");
        std::fs::write(&part, [s.init_bytes(&small).unwrap(), l.init_bytes(&large).unwrap(), media].concat()).unwrap();
        let joined = join_track(None, &rec, 0, std::slice::from_ref(&part)).unwrap();
        assert_ne!(joined, part, "rewritten without the first init segment");
        let (seconds, size) = duration_and_size(&ffmpeg, &joined);
        assert_eq!(size, Some((320, 240)));
        assert!((1.9..2.2).contains(&seconds), "{seconds}");
    }

    /// Parts that switched quality are encoded again into one video at the largest part's size;
    /// MPEG-TS parts are joined as they are and remuxed into an MP4.
    #[test]
    fn a_quality_switch_is_joined_by_encoding_again() {
        let Some(ffmpeg) = hyperfetch_core::media::find_ffmpeg_path() else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let rec = dir.path().join("rec");
        std::fs::create_dir(&rec).unwrap();
        let parts = [rec.join("track0.part0.mp4"), rec.join("track0.part1.mp4")];
        fragmented(&ffmpeg, &parts[0], "320x240", 2, true);
        fragmented(&ffmpeg, &parts[1], "640x360", 2, true);
        let joined = join_track(Some(&ffmpeg), &rec, 0, &parts).unwrap();
        assert_eq!(joined, rec.join("track0.joined.mp4"));
        // Each part's AAC runs some 80 ms past its video, which the next part starts after.
        let (seconds, size) = duration_and_size(&ffmpeg, &joined);
        assert!((3.9..4.3).contains(&seconds), "{seconds}");
        assert_eq!(size, Some((640, 360)));
        assert!(stream_info(&probe(&ffmpeg, &joined).unwrap()).1);
        assert_eq!(join_track(None, &rec, 0, &parts).unwrap_err(), "ffmpeg was not found");

        // Without ffmpeg the parts are saved as they are.
        let save = dir.path().join("save");
        let notice = merge(&finished("Switch", &rec, vec![(0, parts.to_vec())]), &save, None).unwrap_err();
        assert!(notice.contains("Switch.track0.part0.mp4, Switch.track0.part1.mp4"), "{notice}");

        let ts = dir.path().join("ts");
        std::fs::create_dir(&ts).unwrap();
        let tone = "sine=frequency=440:sample_rate=44100";
        ffmpeg_ok(&ffmpeg, &["-f", "lavfi", "-i", "testsrc=size=320x240:rate=25", "-f", "lavfi", "-i", tone, "-t", "4", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "25", "-c:a", "aac", "-f", "segment", "-segment_time", "2", ts.join("track0.part%d.ts").to_str().unwrap()]);
        let parts = vec![ts.join("track0.part0.ts"), ts.join("track0.part1.ts")];
        let notice = merge(&finished("Stream", &ts, vec![(0, parts)]), &save, Some(&ffmpeg)).unwrap();
        assert!(notice.ends_with("Stream.mp4"), "{notice}");
        let (seconds, size) = duration_and_size(&ffmpeg, &save.join("Stream.mp4"));
        assert!((3.9..4.2).contains(&seconds), "{seconds}");
        assert_eq!(size, Some((320, 240)));
    }

    /// WebM parts that start with the same header are joined with that header once.
    #[test]
    fn webm_parts_with_one_header_are_joined() {
        let Some(ffmpeg) = hyperfetch_core::media::find_ffmpeg_path() else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let whole = dir.path().join("whole.webm");
        ffmpeg_ok(&ffmpeg, &["-f", "lavfi", "-i", "testsrc=size=320x240:rate=25", "-t", "4", "-c:v", "libvpx", "-g", "25", "-cluster_time_limit", "1000", "-f", "webm", whole.to_str().unwrap()]);
        let bytes = std::fs::read(&whole).unwrap();
        let init = webm_init_len(&bytes).unwrap();
        // The second Cluster: past the first, whose size is known.
        let (_, size, content) = ebml_element(&bytes, init).unwrap();
        let second = content + size.unwrap() as usize;
        assert_eq!(ebml_element(&bytes, second).unwrap().0, 0x1F43_B675);
        let rec = dir.path().join("rec");
        std::fs::create_dir(&rec).unwrap();
        let parts = [rec.join("track0.part0.webm"), rec.join("track0.part1.webm")];
        std::fs::write(&parts[0], &bytes[..second]).unwrap();
        std::fs::write(&parts[1], [&bytes[..init], &bytes[second..]].concat()).unwrap();
        let joined = join_track(None, &rec, 0, &parts).unwrap();
        assert_eq!(std::fs::read(&joined).unwrap(), bytes);
    }
}
