//! Video the browser extension records from a page (the buffers a player feeds its MediaSource,
//! or MediaRecorder's output) arrives through the local API (see `ipc`) in chunks, kept one file
//! per track in a temporary folder, and is joined into the save folder once it is finished.

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
    /// Each track's file and its size, by track index.
    tracks: BTreeMap<u8, (PathBuf, u64)>,
    bytes: u64,
    last_chunk: Instant,
}

/// A recording the extension finished, to be joined into the save folder (see [`merge`]).
#[derive(Debug)]
pub struct Finished {
    pub title: String,
    /// The temporary folder that holds the tracks, deleted once they are saved.
    pub dir: PathBuf,
    /// The track files, by track index.
    pub tracks: Vec<PathBuf>,
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

    /// Opens a recording titled `title` and returns its id.
    pub async fn start(&self, title: &str) -> Result<String, Refusal> {
        let mut open = self.open.lock().await;
        if open.len() >= MAX_OPEN {
            return Err(Refusal::Limit("too many recordings at once"));
        }
        // Unique in this run by the count, and not guessable by the random part.
        let count = self.started.fetch_add(1, Ordering::Relaxed);
        let id = format!("{:x}{:016x}", count, RandomState::new().build_hasher().finish());
        let dir = self.root.join(&id);
        tokio::fs::create_dir_all(&dir).await.map_err(|e| Refusal::Disk(format!("Cannot create {}: {}", dir.display(), e)))?;
        let recording = Recording { title: title.to_string(), dir, tracks: BTreeMap::new(), bytes: 0, last_chunk: Instant::now() };
        open.insert(id.clone(), recording);
        Ok(id)
    }

    /// Appends `data` to track `track` of recording `id` and returns the track's size. The first
    /// chunk of a track names its file for `mime`; later ones go to that file whatever theirs.
    pub async fn append(&self, id: &str, track: u8, mime: &str, data: &[u8]) -> Result<u64, Refusal> {
        let mut open = self.open.lock().await;
        let recording = open.get_mut(id).ok_or(Refusal::Unknown)?;
        let size = data.len() as u64;
        if recording.bytes + size > MAX_BYTES {
            return Err(Refusal::Limit("the recording is too large"));
        }
        let dir = &recording.dir;
        let (path, track_bytes) =
            recording.tracks.entry(track).or_insert_with(|| (dir.join(format!("track{}.{}", track, ext_for_mime(mime))), 0));
        let disk = |e: std::io::Error| Refusal::Disk(format!("Cannot write {}: {}", path.display(), e));
        let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await.map_err(disk)?;
        file.write_all(data).await.map_err(disk)?;
        // Done before the next chunk opens the file again.
        file.flush().await.map_err(disk)?;
        *track_bytes += size;
        recording.bytes += size;
        recording.last_chunk = Instant::now();
        Ok(*track_bytes)
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
    /// What it recorded, to be joined, or None when it is too small to keep and was deleted.
    fn close(self) -> Option<Finished> {
        if self.bytes < MIN_BYTES {
            let _ = std::fs::remove_dir_all(&self.dir);
            return None;
        }
        let tracks = self.tracks.into_values().map(|(path, _)| path).collect();
        Some(Finished { title: self.title, dir: self.dir, tracks })
    }
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
    } else {
        "bin"
    }
}

/// The container tracks with the extensions `exts` are joined into.
fn container_for(exts: &[&str]) -> &'static str {
    if exts.iter().all(|&ext| ext == "webm") {
        "webm"
    } else if exts.iter().all(|&ext| matches!(ext, "mp4" | "m4a")) {
        "mp4"
    } else {
        "mkv"
    }
}

/// ffmpeg's arguments to join `inputs` into `out` without encoding them again. A single track is
/// copied all the same: that indexes a fragmented MP4 and gives MediaRecorder's WebM a duration.
fn ffmpeg_args(inputs: &[PathBuf], out: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-y", "-nostdin", "-hide_banner", "-loglevel", "error"].map(OsString::from).into();
    for input in inputs {
        args.extend(["-i".into(), input.into()]);
    }
    for n in 0..inputs.len() {
        args.extend(["-map".into(), n.to_string().into()]);
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

/// Joins the tracks of `finished` into `<title>.<container>` in `save_dir` with `ffmpeg`, or,
/// without ffmpeg or when it fails, moves them there as they are (`<title>.track<k>.<ext>`); then
/// deletes the temporary folder. Returns the notice to show: an error when the tracks were saved
/// unjoined, or could not be saved (they are kept in the temporary folder then).
pub fn merge(finished: &Finished, save_dir: &Path, ffmpeg: Option<&Path>) -> Result<String, String> {
    let kept = || format!("the recording is kept in {}", finished.dir.display());
    if save_dir.as_os_str().is_empty() {
        return Err(format!("Choose a folder to save downloads to; {}", kept()));
    }
    std::fs::create_dir_all(save_dir).map_err(|e| format!("Cannot create {}: {}; {}", save_dir.display(), e, kept()))?;
    let stem = Some(engine::sanitize_filename(&finished.title)).filter(|s| !s.is_empty()).unwrap_or_else(|| "recording".to_string());
    let exts: Vec<&str> = finished.tracks.iter().map(|track| track.extension().and_then(|e| e.to_str()).unwrap_or("bin")).collect();
    // Recordings finishing together, and downloads, never write the same file.
    let (out, _claim) = free_path(save_dir, &stem, container_for(&exts)).map_err(|e| format!("{}; {}", e, kept()))?;
    let why = match ffmpeg {
        None => "ffmpeg was not found".to_string(),
        Some(ffmpeg) => match join(ffmpeg, &finished.tracks, &out) {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(&finished.dir);
                return Ok(format!("Saved the recording as {}", out.display()));
            }
            Err(e) => {
                // What ffmpeg left of the file it could not finish.
                let _ = std::fs::remove_file(&out);
                e
            }
        },
    };
    let mut saved = Vec::new();
    for (track, ext) in finished.tracks.iter().zip(&exts) {
        let name = track.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default();
        let (to, _claim) = free_path(save_dir, &format!("{}.{}", stem, name), ext).map_err(|e| format!("{}; {}", e, kept()))?;
        // A rename cannot cross drives; a copy can, and the temporary folder goes anyway.
        std::fs::rename(track, &to)
            .or_else(|_| std::fs::copy(track, &to).map(drop))
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

/// Runs `ffmpeg` to join `tracks` into `out`; the error says what it said.
fn join(ffmpeg: &Path, tracks: &[PathBuf], out: &Path) -> Result<(), String> {
    let mut command = Command::new(ffmpeg);
    command.args(ffmpeg_args(tracks, out)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashes up over the app.
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(|e| format!("ffmpeg could not run: {}", e))?;
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
            ("", "bin"),
            ("video/x-unknown", "bin"),
        ] {
            assert_eq!(ext_for_mime(mime), ext, "{mime}");
        }
        assert_eq!(container_for(&["webm", "webm"]), "webm");
        assert_eq!(container_for(&["mp4", "m4a"]), "mp4");
        assert_eq!(container_for(&["mp4"]), "mp4");
        assert_eq!(container_for(&["mp4", "webm"]), "mkv");
        assert_eq!(container_for(&["mp3"]), "mkv");
        assert_eq!(container_for(&["ts", "m4a"]), "mkv");
    }

    #[test]
    fn ffmpeg_copies_every_track_into_the_output() {
        let args = |inputs: &[&str], out: &str| {
            let inputs: Vec<PathBuf> = inputs.iter().map(PathBuf::from).collect();
            ffmpeg_args(&inputs, Path::new(out)).into_iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
        };
        assert_eq!(
            args(&["t/track0.mp4", "t/track1.m4a"], "d/A.mp4"),
            "-y -nostdin -hide_banner -loglevel error -i t/track0.mp4 -i t/track1.m4a -map 0 -map 1 -c copy -movflags +faststart d/A.mp4"
        );
        assert_eq!(args(&["t/track0.webm"], "d/A.webm"), "-y -nostdin -hide_banner -loglevel error -i t/track0.webm -map 0 -c copy d/A.webm");
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

    /// Without ffmpeg, or when it fails, the tracks are moved into the save folder as they are,
    /// under the cleaned title, and the temporary folder goes.
    #[test]
    fn tracks_ffmpeg_cannot_join_are_saved_as_they_are() {
        let save = tempfile::tempdir().unwrap();
        for ffmpeg in [None, Some(save.path().join("no-such-ffmpeg.exe"))] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("rec");
            std::fs::create_dir(&dir).unwrap();
            let tracks = vec![dir.join("track0.mp4"), dir.join("track1.m4a")];
            std::fs::write(&tracks[0], b"video").unwrap();
            std::fs::write(&tracks[1], b"audio").unwrap();
            let finished = Finished { title: "A: talk?".into(), dir: dir.clone(), tracks };
            let notice = merge(&finished, save.path(), ffmpeg.as_deref()).unwrap_err();
            assert!(notice.contains("ffmpeg is needed to join them"), "{notice}");
            assert!(!dir.exists(), "the temporary folder is deleted");
        }
        let mut names: Vec<String> = std::fs::read_dir(save.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["A_ talk_.track0 (1).mp4", "A_ talk_.track0.mp4", "A_ talk_.track1 (1).m4a", "A_ talk_.track1.m4a"]);
        assert_eq!(std::fs::read(save.path().join("A_ talk_.track1.m4a")).unwrap(), b"audio");

        // No save folder: nothing is lost.
        let temp = tempfile::tempdir().unwrap();
        let finished = Finished { title: String::new(), dir: temp.path().to_path_buf(), tracks: Vec::new() };
        assert!(merge(&finished, Path::new(""), None).unwrap_err().contains("kept in"));
        assert!(temp.path().exists());
    }

    /// Chunks are appended to each track's file in order, the first chunk's mime naming it; a
    /// small recording is deleted when finished, a big one handed over, an aborted one deleted.
    #[tokio::test]
    async fn chunks_are_kept_per_track_until_the_recording_ends() {
        let root = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(root.path().to_path_buf());
        let id = recordings.start("Talk").await.unwrap();
        assert!(valid_id(&id), "{id}");
        assert_eq!(recordings.append(&id, 0, "video/webm", b"ab").await, Ok(2));
        assert_eq!(recordings.append(&id, 0, "video/mp4", b"cd").await, Ok(4), "the first mime names the file");
        assert_eq!(recordings.append(&id, 1, "audio/mp4", &[7; 70 * 1024]).await, Ok(70 * 1024));
        assert_eq!(recordings.append("nope", 0, "video/webm", b"x").await, Err(Refusal::Unknown));
        let finished = recordings.finish(&id).await.unwrap().unwrap();
        assert_eq!(finished.title, "Talk");
        assert_eq!(finished.tracks, [root.path().join(&id).join("track0.webm"), root.path().join(&id).join("track1.m4a")]);
        assert_eq!(std::fs::read(&finished.tracks[0]).unwrap(), b"abcd");
        assert_eq!(recordings.finish(&id).await.unwrap_err(), Refusal::Unknown, "finished once");

        let small = recordings.start("Small").await.unwrap();
        recordings.append(&small, 0, "video/webm", b"tiny").await.unwrap();
        assert!(recordings.finish(&small).await.unwrap().is_none());
        assert!(!root.path().join(&small).exists());

        let aborted = recordings.start("Aborted").await.unwrap();
        recordings.append(&aborted, 0, "video/webm", b"x").await.unwrap();
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
            ids.push(recordings.start("t").await.unwrap());
        }
        assert_eq!(recordings.start("t").await, Err(Refusal::Limit("too many recordings at once")));
        recordings.abort(&ids[0]).await.unwrap();
        assert!(recordings.start("t").await.is_ok());
    }

    /// Once too many are open, those a browser left behind make room: what they recorded is
    /// handed over to be joined, not deleted; one too small to keep is deleted.
    #[tokio::test]
    async fn recordings_left_open_are_joined_to_make_room() {
        let root = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(root.path().to_path_buf());
        let mut ids = Vec::new();
        for _ in 0..MAX_OPEN {
            ids.push(recordings.start("t").await.unwrap());
        }
        recordings.append(&ids[0], 0, "video/webm", &[1; 70 * 1024]).await.unwrap();
        recordings.append(&ids[1], 0, "video/webm", b"tiny").await.unwrap();
        for id in &ids[..2] {
            recordings.open.lock().await.get_mut(id).unwrap().last_chunk = Instant::now() - STALE;
        }
        let closed = recordings.close_stale().await;
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].dir, root.path().join(&ids[0]));
        assert!(closed[0].dir.exists(), "kept to be joined");
        assert!(!root.path().join(&ids[1]).exists());
        assert!(recordings.close_stale().await.is_empty(), "room was made");
        assert!(recordings.start("t").await.is_ok());
    }

    #[test]
    fn ids_are_letters_and_digits() {
        assert!(valid_id("a1B2") && valid_id(&"a".repeat(32)));
        for bad in ["", "..", "a/b", "a\\b", "a.b", "é", &"a".repeat(33)] {
            assert!(!valid_id(bad), "{bad}");
        }
    }
}
