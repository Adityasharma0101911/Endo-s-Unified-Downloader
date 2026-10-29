//! Stream merging and post-muxing engine matching N_m3u8DL-RE `--mux-after-done` and `--mux-import`.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::concat_server::ConcatInputServer;

/// Concat mode for FFmpeg segment merging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FFmpegConcatMode {
    #[default]
    LocalHttp,
    Demuxer,
    Protocol,
}

impl FFmpegConcatMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "LOCAL_HTTP" | "LOCALHTTP" | "HTTP" => Some(Self::LocalHttp),
            "DEMUXER" => Some(Self::Demuxer),
            "PROTOCOL" => Some(Self::Protocol),
            _ => None,
        }
    }
}

/// Target container format for muxing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MuxContainerFormat {
    #[default]
    Mkv,
    Mp4,
    Ts,
}

impl MuxContainerFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mkv" => Some(Self::Mkv),
            "mp4" => Some(Self::Mp4),
            "ts" => Some(Self::Ts),
            _ => None,
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            Self::Mkv => "mkv",
            Self::Mp4 => "mp4",
            Self::Ts => "ts",
        }
    }
}

/// Muxer tool selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MuxerTool {
    #[default]
    FFmpeg,
    MkvMerge,
}

impl MuxerTool {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "ffmpeg" => Some(Self::FFmpeg),
            "mkvmerge" => Some(Self::MkvMerge),
            _ => None,
        }
    }
}

/// Parsed options of `--mux-after-done`.
#[derive(Debug, Clone, Default)]
pub struct MuxAfterDoneOptions {
    pub format: MuxContainerFormat,
    pub muxer: MuxerTool,
    pub bin_path: Option<PathBuf>,
    pub skip_sub: bool,
    pub keep_tracks: bool,
}

impl MuxAfterDoneOptions {
    /// Parse colon-separated `--mux-after-done` options, e.g.
    /// `format=mp4:muxer=ffmpeg:keep=true` or bare `mp4`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let trimmed = s.trim();
        let mut opts = Self::default();

        if let Some(fmt) = MuxContainerFormat::parse(trimmed) {
            opts.format = fmt;
            return Ok(opts);
        }

        for token in trimmed.split(':') {
            let t = token.trim();
            if let Some((k, v)) = t.split_once('=') {
                let key = k.trim().to_ascii_lowercase();
                let val = v.trim();
                match key.as_str() {
                    "format" => {
                        opts.format = MuxContainerFormat::parse(val)
                            .ok_or_else(|| format!("unknown mux format '{val}'"))?;
                    }
                    "muxer" => {
                        opts.muxer = MuxerTool::parse(val)
                            .ok_or_else(|| format!("unknown muxer '{val}'"))?;
                    }
                    "bin_path" => {
                        opts.bin_path = Some(PathBuf::from(val));
                    }
                    "skip_sub" => {
                        opts.skip_sub = val.eq_ignore_ascii_case("true") || val == "1";
                    }
                    "keep" => {
                        opts.keep_tracks = val.eq_ignore_ascii_case("true") || val == "1";
                    }
                    _ => return Err(format!("unknown mux-after-done option: {key}")),
                }
            } else if let Some(fmt) = MuxContainerFormat::parse(t) {
                opts.format = fmt;
            }
        }

        Ok(opts)
    }
}

/// External track imported via `--mux-import`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxImportTrack {
    pub path: PathBuf,
    pub language: Option<String>,
    pub name: Option<String>,
}

impl MuxImportTrack {
    /// Parse `--mux-import` string:
    /// e.g. `path=en-US.srt:lang=en-US:name=English` or direct path `subtitle.srt`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let trimmed = s.trim();
        if !trimmed.contains('=') {
            return Ok(Self {
                path: PathBuf::from(trimmed),
                language: None,
                name: None,
            });
        }

        let mut path = None;
        let mut lang = None;
        let mut name = None;

        // Note: handle escaped colons `\:`
        for part in split_escaped_colon(trimmed) {
            if let Some((k, v)) = part.split_once('=') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "path" => path = Some(PathBuf::from(v.trim())),
                    "lang" | "language" => lang = Some(v.trim().to_string()),
                    "name" | "title" => name = Some(v.trim().to_string()),
                    _ => {}
                }
            }
        }

        let p = path.ok_or("missing path in --mux-import")?;
        Ok(Self { path: p, language: lang, name })
    }
}

fn split_escaped_colon(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&':') = chars.peek() {
                chars.next();
                cur.push(':');
                continue;
            }
            cur.push('\\');
        } else if c == ':' {
            if !cur.trim().is_empty() {
                parts.push(cur.trim().to_string());
            }
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts
}

/// Binary concatenation of multiple segment files into one output file.
pub fn binary_concat_files(files: &[PathBuf], output: &Path) -> io::Result<u64> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }

    let out_file = File::create(output)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, out_file);
    let mut total_bytes = 0u64;
    let mut buf = vec![0u8; 128 * 1024];

    for f_path in files {
        let mut in_file = File::open(f_path)?;
        loop {
            let n = in_file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
            total_bytes += n as u64;
        }
    }

    writer.flush()?;
    Ok(total_bytes)
}

/// Merge TS segments into output file using FFmpeg with chosen concat mode.
pub async fn ffmpeg_merge_segments(
    ffmpeg_bin: Option<&Path>,
    mode: FFmpegConcatMode,
    files: &[PathBuf],
    output: &Path,
) -> Result<(), String> {
    if files.is_empty() {
        return Err("no files to merge".to_string());
    }

    let bin = ffmpeg_bin.unwrap_or(Path::new("ffmpeg"));

    match mode {
        FFmpegConcatMode::LocalHttp => {
            let server = ConcatInputServer::start(files).await?;
            let mut cmd = Command::new(bin);
            cmd.arg("-y")
                .arg("-i")
                .arg(server.url())
                .arg("-c")
                .arg("copy")
                .arg(output);

            let status = cmd.status().map_err(|e| format!("failed to run ffmpeg: {e}"))?;
            server.stop();

            if !status.success() {
                return Err(format!("ffmpeg merge via LOCAL_HTTP failed: {status}"));
            }
        }
        FFmpegConcatMode::Demuxer => {
            let list_file = output.with_extension("concat.txt");
            let mut list_content = String::from("ffconcat version 1.0\n");
            for f in files {
                let abs = fs::canonicalize(f).unwrap_or_else(|_| f.clone());
                list_content.push_str(&format!("file '{}'\n", abs.display().to_string().replace('\'', "'\\''")));
            }
            fs::write(&list_file, list_content).map_err(|e| format!("failed to write demuxer file: {e}"))?;

            let mut cmd = Command::new(bin);
            cmd.arg("-y")
                .arg("-f")
                .arg("concat")
                .arg("-safe")
                .arg("0")
                .arg("-i")
                .arg(&list_file)
                .arg("-c")
                .arg("copy")
                .arg(output);

            let status = cmd.status().map_err(|e| format!("failed to run ffmpeg demuxer: {e}"))?;
            let _ = fs::remove_file(list_file);

            if !status.success() {
                return Err(format!("ffmpeg merge via DEMUXER failed: {status}"));
            }
        }
        FFmpegConcatMode::Protocol => {
            let mut concat_str = String::from("concat:");
            let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
            concat_str.push_str(&paths.join("|"));

            let mut cmd = Command::new(bin);
            cmd.arg("-y")
                .arg("-i")
                .arg(concat_str)
                .arg("-c")
                .arg("copy")
                .arg(output);

            let status = cmd.status().map_err(|e| format!("failed to run ffmpeg protocol: {e}"))?;
            if !status.success() {
                return Err(format!("ffmpeg merge via PROTOCOL failed: {status}"));
            }
        }
    }

    Ok(())
}

/// Track info for post-download muxing.
#[derive(Debug, Clone)]
pub struct TrackToMux {
    pub path: PathBuf,
    pub media_type: &'static str,
    pub language: Option<String>,
    pub title: Option<String>,
}

/// Mux audio, video, subtitle, and imported tracks together into final container.
pub fn mux_tracks(
    opts: &MuxAfterDoneOptions,
    tracks: &[TrackToMux],
    imported_tracks: &[MuxImportTrack],
    title: Option<&str>,
    output_path: &Path,
) -> Result<(), String> {
    if tracks.is_empty() && imported_tracks.is_empty() {
        return Err("no tracks to mux".to_string());
    }

    match opts.muxer {
        MuxerTool::FFmpeg => {
            let bin = opts.bin_path.as_deref().unwrap_or(Path::new("ffmpeg"));
            let mut cmd = Command::new(bin);
            cmd.arg("-y");

            let mut input_idx = 0;
            // Add downloaded tracks
            for t in tracks {
                if opts.skip_sub && t.media_type == "subtitles" {
                    continue;
                }
                cmd.arg("-i").arg(&t.path);
                input_idx += 1;
            }

            // Add external imported tracks
            for imp in imported_tracks {
                cmd.arg("-i").arg(&imp.path);
                input_idx += 1;
            }

            // Map all inputs
            for i in 0..input_idx {
                cmd.arg("-map").arg(i.to_string());
            }

            // Metadata: language and title
            let mut cur_idx = 0;
            for t in tracks {
                if opts.skip_sub && t.media_type == "subtitles" {
                    continue;
                }
                if let Some(ref lang) = t.language {
                    cmd.arg(format!("-metadata:s:{cur_idx}")).arg(format!("language={lang}"));
                }
                if let Some(ref name) = t.title {
                    cmd.arg(format!("-metadata:s:{cur_idx}")).arg(format!("title={name}"));
                }
                cur_idx += 1;
            }
            for imp in imported_tracks {
                if let Some(ref lang) = imp.language {
                    cmd.arg(format!("-metadata:s:{cur_idx}")).arg(format!("language={lang}"));
                }
                if let Some(ref name) = imp.name {
                    cmd.arg(format!("-metadata:s:{cur_idx}")).arg(format!("title={name}"));
                }
                cur_idx += 1;
            }

            if let Some(t) = title {
                cmd.arg("-metadata").arg(format!("title={t}"));
            }

            cmd.arg("-c").arg("copy");
            cmd.arg(output_path);

            let status = cmd.status().map_err(|e| format!("failed to invoke ffmpeg mux: {e}"))?;
            if !status.success() {
                return Err(format!("ffmpeg muxing exited with error: {status}"));
            }
        }
        MuxerTool::MkvMerge => {
            let bin = opts.bin_path.as_deref().unwrap_or(Path::new("mkvmerge"));
            let mut cmd = Command::new(bin);
            cmd.arg("-o").arg(output_path);

            if let Some(t) = title {
                cmd.arg("--title").arg(t);
            }

            for t in tracks {
                if opts.skip_sub && t.media_type == "subtitles" {
                    continue;
                }
                if let Some(ref lang) = t.language {
                    cmd.arg("--language").arg(format!("0:{lang}"));
                }
                if let Some(ref name) = t.title {
                    cmd.arg("--track-name").arg(format!("0:{name}"));
                }
                cmd.arg(&t.path);
            }

            for imp in imported_tracks {
                if let Some(ref lang) = imp.language {
                    cmd.arg("--language").arg(format!("0:{lang}"));
                }
                if let Some(ref name) = imp.name {
                    cmd.arg("--track-name").arg(format!("0:{name}"));
                }
                cmd.arg(&imp.path);
            }

            let status = cmd.status().map_err(|e| format!("failed to invoke mkvmerge: {e}"))?;
            if !status.success() {
                return Err(format!("mkvmerge muxing exited with error: {status}"));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_mux_after_done() {
        let opts = MuxAfterDoneOptions::parse("format=mp4:muxer=ffmpeg:keep=true").unwrap();
        assert_eq!(opts.format, MuxContainerFormat::Mp4);
        assert_eq!(opts.muxer, MuxerTool::FFmpeg);
        assert!(opts.keep_tracks);

        let opts_mkv = MuxAfterDoneOptions::parse("mkv").unwrap();
        assert_eq!(opts_mkv.format, MuxContainerFormat::Mkv);
    }

    #[test]
    fn test_parse_mux_import() {
        let imp = MuxImportTrack::parse("path=audio.m4a:lang=eng:name=English Audio").unwrap();
        assert_eq!(imp.path, PathBuf::from("audio.m4a"));
        assert_eq!(imp.language, Some("eng".into()));
        assert_eq!(imp.name, Some("English Audio".into()));

        let direct = MuxImportTrack::parse("sub.srt").unwrap();
        assert_eq!(direct.path, PathBuf::from("sub.srt"));
    }

    #[test]
    fn test_binary_concat() {
        let mut f1 = NamedTempFile::new().unwrap();
        f1.write_all(b"Part 1; ").unwrap();
        f1.flush().unwrap();

        let mut f2 = NamedTempFile::new().unwrap();
        f2.write_all(b"Part 2; ").unwrap();
        f2.flush().unwrap();

        let out = NamedTempFile::new().unwrap();
        let total = binary_concat_files(&[f1.path().to_path_buf(), f2.path().to_path_buf()], out.path()).unwrap();
        assert_eq!(total, 16);

        let read_back = fs::read_to_string(out.path()).unwrap();
        assert_eq!(read_back, "Part 1; Part 2; ");
    }
}
