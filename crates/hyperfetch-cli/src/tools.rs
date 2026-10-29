//! The CLI's tool commands (concat, merge, mux, doctor) and the two-letter option names it takes.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use hyperfetch_core::{
    binary_concat_files, ffmpeg_merge_segments, mux_tracks, DoctorReport, FFmpegConcatMode, MuxAfterDoneOptions,
    MuxImportTrack, TrackToMux,
};

use crate::cli::{ConcatModeArg, SubCommand};

/// `args` with the two-letter names clap cannot take as short options (-sv, -sa, -ss) spelled out.
pub fn preprocess_cli_args(args: Vec<OsString>) -> Vec<OsString> {
    args.into_iter()
        .map(|arg| match arg.to_str() {
            Some("-sv") => "--video".into(),
            Some("-sa") => "--audio".into(),
            Some("-ss") => "--subs".into(),
            _ => arg,
        })
        .collect()
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
        SubCommand::Mux { inputs, output, title, muxer, mux_imports, ffmpeg_binary_path } => {
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
        println!("=== Endo's Unified Downloader Doctor ===");
        println!("OS:           {} ({})", report.os, report.arch);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Args;
    use clap::Parser;

    #[test]
    fn other_option_names_reach_the_options_that_do_their_job() {
        let argv = ["x", "-sv", "1080p", "--select-audio", "en", "--save-dir", "out", "--live-record-limit", "60", "https://a.test/v.m3u8"];
        let args = Args::try_parse_from(preprocess_cli_args(argv.map(OsString::from).to_vec())).unwrap();
        assert_eq!(args.video.as_deref(), Some("1080p"));
        assert_eq!(args.audio.as_deref(), Some("en"));
        assert_eq!(args.dir, Some(PathBuf::from("out")));
        assert_eq!(args.record_seconds, Some(60));
    }
}
