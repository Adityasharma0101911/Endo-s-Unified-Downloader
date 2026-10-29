//! N_m3u8DL-RE utility commands and helper actions (concat, merge, mux, doctor, morehelp).

use std::fs;
use std::path::{Path, PathBuf};
use hyperfetch_core::{
    binary_concat_files, default_config_path, ffmpeg_merge_segments, mux_tracks, parse_config_file,
    DoctorReport, FFmpegConcatMode, MuxAfterDoneOptions, MuxImportTrack, TrackToMux,
};

use crate::cli::{ConcatModeArg, SubCommand};

/// Preprocess CLI arguments to support N_m3u8DL-RE aliases and auto-loading config.conf.
pub fn preprocess_cli_args(args: &[String]) -> Vec<String> {
    if args.is_empty() {
        return Vec::new();
    }

    let program = args[0].clone();
    let mut expanded = Vec::with_capacity(args.len());

    let mut has_no_config = false;
    let mut explicit_config: Option<PathBuf> = None;

    let mut iter = args[1..].iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--no-config" {
            has_no_config = true;
            expanded.push(arg.clone());
        } else if arg == "--config" {
            if let Some(next) = iter.peek() {
                explicit_config = Some(PathBuf::from(next));
            }
            expanded.push(arg.clone());
        } else if arg == "-sv" {
            expanded.push("--select-video".to_string());
        } else if arg == "-sa" {
            expanded.push("--select-audio".to_string());
        } else if arg == "-ss" {
            expanded.push("--select-subtitle".to_string());
        } else if arg == "-dv" {
            expanded.push("--drop-video".to_string());
        } else if arg == "-da" {
            expanded.push("--drop-audio".to_string());
        } else if arg == "-ds" {
            expanded.push("--drop-subtitle".to_string());
        } else if arg == "-mt" {
            expanded.push("--concurrent-download".to_string());
        } else {
            expanded.push(arg.clone());
        }
    }

    // Config file resolution
    let mut config_args = Vec::new();
    if !has_no_config {
        let config_file = explicit_config.or_else(default_config_path);
        if let Some(p) = config_file {
            if p.exists() {
                if let Ok(loaded) = parse_config_file(&p) {
                    for carg in loaded {
                        if carg == "-sv" {
                            config_args.push("--select-video".to_string());
                        } else if carg == "-sa" {
                            config_args.push("--select-audio".to_string());
                        } else if carg == "-ss" {
                            config_args.push("--select-subtitle".to_string());
                        } else if carg == "-mt" {
                            config_args.push("--concurrent-download".to_string());
                        } else {
                            config_args.push(carg);
                        }
                    }
                }
            }
        }
    }

    let mut final_args = Vec::with_capacity(1 + config_args.len() + expanded.len());
    final_args.push(program);
    final_args.extend(config_args);
    final_args.extend(expanded);
    final_args
}

/// Execute a subcommand.
pub async fn execute_subcommand(subcmd: SubCommand) -> i32 {
    match subcmd {
        SubCommand::Concat { inputs, input_dir, pattern, output } => {
            run_concat(inputs, input_dir, &pattern, &output)
        }
        SubCommand::Merge { inputs, input_dir, pattern, output, mode, ffmpeg_binary_path } => {
            run_merge(inputs, input_dir, &pattern, &output, mode, ffmpeg_binary_path.as_deref()).await
        }
        SubCommand::Mux { inputs, output, title, muxer, mux_imports, auto_subtitle_fix: _, ffmpeg_binary_path } => {
            run_mux(inputs, &output, title.as_deref(), &muxer, &mux_imports, ffmpeg_binary_path.as_deref())
        }
        SubCommand::Doctor { json, ffmpeg_binary_path, mkvmerge_binary_path, decryption_binary_path } => {
            run_doctor(json, ffmpeg_binary_path.as_deref(), mkvmerge_binary_path.as_deref(), decryption_binary_path.as_deref())
        }
    }
}

fn run_concat(inputs: Vec<PathBuf>, input_dir: Option<PathBuf>, pattern: &str, output: &Path) -> i32 {
    let files = match collect_input_files(inputs, input_dir, pattern) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    println!("Concatenating {} segment files into {}...", files.len(), output.display());
    match binary_concat_files(&files, output) {
        Ok(bytes) => {
            println!("Done! Wrote {} bytes to {}", bytes, output.display());
            0
        }
        Err(e) => {
            eprintln!("concat failed: {e}");
            1
        }
    }
}

async fn run_merge(
    inputs: Vec<PathBuf>,
    input_dir: Option<PathBuf>,
    pattern: &str,
    output: &Path,
    mode: ConcatModeArg,
    ffmpeg_bin: Option<&Path>,
) -> i32 {
    let files = match collect_input_files(inputs, input_dir, pattern) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let concat_mode = match mode {
        ConcatModeArg::LocalHttp => FFmpegConcatMode::LocalHttp,
        ConcatModeArg::Demuxer => FFmpegConcatMode::Demuxer,
        ConcatModeArg::Protocol => FFmpegConcatMode::Protocol,
    };

    println!("Merging {} segment files into {} (mode: {:?})...", files.len(), output.display(), mode);
    match ffmpeg_merge_segments(ffmpeg_bin, concat_mode, &files, output).await {
        Ok(()) => {
            println!("Done! Merged into {}", output.display());
            0
        }
        Err(e) => {
            eprintln!("merge failed: {e}");
            1
        }
    }
}

fn run_mux(
    inputs: Vec<String>,
    output: &Path,
    title: Option<&str>,
    muxer_str: &str,
    mux_imports: &[String],
    ffmpeg_bin: Option<&Path>,
) -> i32 {
    let mut opts = MuxAfterDoneOptions::default();
    if let Ok(parsed) = MuxAfterDoneOptions::parse(muxer_str) {
        opts = parsed;
    }
    if let Some(bin) = ffmpeg_bin {
        opts.bin_path = Some(bin.to_path_buf());
    }

    let mut tracks = Vec::new();
    for input_str in inputs {
        let p = PathBuf::from(&input_str);
        if p.is_file() {
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
            let media_type = if ext == "srt" || ext == "vtt" {
                "subtitles"
            } else if ext == "m4a" || ext == "mp3" || ext == "aac" || ext == "flac" {
                "audio"
            } else {
                "video"
            };
            tracks.push(TrackToMux {
                path: p,
                media_type,
                language: None,
                title: None,
            });
        } else if p.is_dir() {
            // Read directory segments or files
            if let Ok(entries) = fs::read_dir(&p) {
                for entry in entries.flatten() {
                    let ep = entry.path();
                    if ep.is_file() {
                        tracks.push(TrackToMux {
                            path: ep,
                            media_type: "video",
                            language: None,
                            title: None,
                        });
                    }
                }
            }
        }
    }

    let mut imported = Vec::new();
    for imp in mux_imports {
        if let Ok(parsed_imp) = MuxImportTrack::parse(imp) {
            imported.push(parsed_imp);
        }
    }

    println!("Muxing {} tracks into {}...", tracks.len() + imported.len(), output.display());
    match mux_tracks(&opts, &tracks, &imported, title, output) {
        Ok(()) => {
            println!("Done! Output written to {}", output.display());
            0
        }
        Err(e) => {
            eprintln!("mux failed: {e}");
            1
        }
    }
}

fn run_doctor(
    json: bool,
    ffmpeg_bin: Option<&Path>,
    mkvmerge_bin: Option<&Path>,
    decrypt_bin: Option<&Path>,
) -> i32 {
    let report = DoctorReport::inspect(
        ffmpeg_bin.and_then(|p| p.to_str()),
        mkvmerge_bin.and_then(|p| p.to_str()),
        decrypt_bin.and_then(|p| p.to_str()),
    );

    if json {
        if let Ok(j) = serde_json::to_string_pretty(&report) {
            println!("{j}");
        }
    } else {
        println!("=== Endo's Unified Downloader & N_m3u8DL-RE Doctor ===");
        println!("OS:           {} ({})", report.os, report.arch);
        println!("Config File:  {}", report.config_file.as_deref().unwrap_or("None"));
        println!("--------------------------------------------------");
        println!("{:<16} {:<10} {:<30} {}", "Tool", "Status", "Version", "Path");
        println!("--------------------------------------------------");
        for t in &report.tools {
            println!(
                "{:<16} {:<10} {:<30} {}",
                t.name,
                t.status,
                t.version.as_deref().unwrap_or("-"),
                t.path.as_deref().unwrap_or("-")
            );
        }
        println!("--------------------------------------------------");
    }
    0
}

fn collect_input_files(
    inputs: Vec<PathBuf>,
    input_dir: Option<PathBuf>,
    pattern: &str,
) -> Result<Vec<PathBuf>, String> {
    if !inputs.is_empty() {
        return Ok(inputs);
    }

    let dir = input_dir.ok_or("either -i/--input or --input-dir must be provided")?;
    if !dir.is_dir() {
        return Err(format!("directory '{}' does not exist", dir.display()));
    }

    let mut files = Vec::new();
    let pat_suffix = pattern.trim_start_matches('*');

    for entry in fs::read_dir(&dir).map_err(|e| format!("cannot read dir: {e}"))?.flatten() {
        let p = entry.path();
        if p.is_file() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.ends_with(pat_suffix) {
                files.push(p);
            }
        }
    }

    files.sort();
    if files.is_empty() {
        return Err(format!("no files matching '{pattern}' found in '{}'", dir.display()));
    }
    Ok(files)
}

/// Print detailed help for `--morehelp <OPTION>`.
pub fn print_morehelp(option: &str) {
    match option.to_ascii_lowercase().as_str() {
        "select-video" | "sv" | "select-audio" | "sa" | "select-subtitle" | "ss" => {
            println!(r#"More Help: --select-video / --select-audio / --select-subtitle

Filter options are separated by colons (:):
  id=REGEX         Match stream or group ID
  lang=REGEX       Match language code (e.g. en, ja, zh)
  name=REGEX       Match stream title or channel name
  codecs=REGEX     Match codecs (e.g. hvc1, avc1, mp4a)
  res=REGEX        Match resolution (e.g. ^1920x, ^3840x)
  frame=REGEX      Match frame rate
  channel=REGEX    Match audio channel count (e.g. 2, 6)
  range=REGEX      Match video range (SDR, HDR, PQ)
  url=REGEX        Match segment or stream URL
  period=REGEX     Match DASH period ID
  segsMin=NUM      Keep streams with at least NUM segments
  segsMax=NUM      Keep streams with at most NUM segments
  plistDurMin=HMS  Keep streams with duration >= HMS (e.g. 1h20m)
  plistDurMax=HMS  Keep streams with duration <= HMS
  bwMin=KBPS       Keep streams with bandwidth >= KBPS
  bwMax=KBPS       Keep streams with bandwidth <= KBPS
  role=ROLE        Match DASH role (main, alternate, subtitle, dub)
  for=TARGET       keep 'best', 'worst', 'bestN', 'worstN', or 'all'

Examples:
  -sv best
  -sv res="^3840x":codecs=hvc1:for=best
  -sa lang="ja|en":for=best2
"#);
        }
        "mux-after-done" | "m" => {
            println!(r#"More Help: --mux-after-done

Options are colon-separated:
  format=FORMAT    Container: mkv, mp4, ts
  muxer=MUXER      Muxer tool: ffmpeg, mkvmerge (default: ffmpeg)
  bin_path=PATH    Path to binary executable
  skip_sub=BOOL    Skip downloaded subtitles (default: false)
  keep=BOOL        Keep intermediate tracks after muxing (default: false)

Examples:
  -M mp4
  -M format=mkv:muxer=ffmpeg:keep=true
"#);
        }
        "mux-import" => {
            println!(r#"More Help: --mux-import

Import external media files when muxing:
  path=PATH        Path to external media file (escape colons as \:)
  lang=CODE        Track language code (e.g. eng, jpn)
  name=NAME        Track title / description

Examples:
  --mux-import "path=sub.srt:lang=eng:name=English Subtitles"
"#);
        }
        "custom-range" => {
            println!(r#"More Help: --custom-range

Download only part of the segments:
  0-10             Segments 0 through 10
  10-              From segment 10 to end
  -99              First 100 segments (0-99)
  05:00-20:00      From 5 minutes to 20 minutes
  00:26-           From 26 seconds to end
"#);
        }
        "save-pattern" => {
            println!(r#"More Help: --save-pattern

Variables available in output filename pattern:
  <SaveName>       Base name specified by --save-name
  <Id>             Track stream ID
  <Codecs>         Stream codecs
  <Language>       Language code
  <Resolution>     Video resolution (e.g. 1920x1080)
  <Bandwidth>      Bitrate in bits per second
  <MediaType>      VIDEO, AUDIO, or SUBTITLES
  <Channels>       Audio channels
  <FrameRate>      Video frame rate
  <VideoRange>     Dynamic range (SDR, HDR10)
  <GroupId>        Stream group identifier
  <Name>           Stream or channel name

Examples:
  --save-pattern "<SaveName>_<Resolution>_<Bandwidth>bps"
"#);
        }
        other => {
            println!("No detailed help available for '{other}'. Use --help for general options.");
        }
    }
}
