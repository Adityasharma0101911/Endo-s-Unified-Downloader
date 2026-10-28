//! --doctor: what this program runs on and with, for a bug report or to see why a download that
//! needs a tool fails. Nothing goes online.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use hyperfetch_core::gallery::find_gallery_dl;
use hyperfetch_core::media::{find_ffmpeg_path, find_ytdlp_path, free_space};
use indicatif::HumanBytes;
use serde_json::{json, Map, Value};

use crate::cli::Args;
use crate::download::{truncate, Item, Ui, EXIT_OK};

/// How long a tool may take to tell its version.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// A tool downloads may run: where it is, and the version it tells (or why it tells none).
struct Tool {
    name: &'static str,
    path: Option<PathBuf>,
    version: Result<String, String>,
}

/// Prints the checkup (see [`checkup`]) of the tools found on this computer. Always succeeds:
/// a missing tool is a finding, not a failure.
pub async fn run(args: &Args, data_folder: Option<PathBuf>, ui: &Ui) -> i32 {
    let found = tokio::task::spawn_blocking(|| [find_ytdlp_path(), find_ffmpeg_path(), find_gallery_dl()]).await;
    let [ytdlp, ffmpeg, gallery] = found.unwrap_or_default();
    // Installed with ffmpeg, where it is.
    let ffprobe = ffmpeg.as_ref().map(|ffmpeg| ffmpeg.with_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" }));
    let wanted = [("yt-dlp", ytdlp, "--version"), ("ffmpeg", ffmpeg, "-version"), ("ffprobe", ffprobe.filter(|p| p.is_file()), "-version"), ("gallery-dl", gallery, "--version")];
    let tools = futures_util::future::join_all(wanted.map(|(name, path, flag)| async move {
        let version = match &path {
            Some(path) => version_of(path, &[flag], PROBE_TIMEOUT).await,
            None => Err("not found".to_string()),
        };
        Tool { name, path, version }
    }))
    .await;
    checkup(args, data_folder, &tools, std::env::vars_os(), ui);
    EXIT_OK
}

/// Prints (with --json as one object) the version, the system, the data folder, the config file,
/// `tools`, the proxy variables among `vars` (the environment's) and the free space where
/// downloads go.
fn checkup(args: &Args, data_folder: Option<PathBuf>, tools: &[Tool], vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>, ui: &Ui) {
    let output = args.dir.clone().unwrap_or_else(|| PathBuf::from("."));
    let output = std::path::absolute(&output).unwrap_or(output);
    // Where downloads go may not be there yet: the disk it would be on.
    let free = output.ancestors().find(|dir| dir.is_dir()).and_then(free_space);
    let proxies = proxies(vars.map(|(name, value)| (name.to_string_lossy().into_owned(), value.to_string_lossy().into_owned())));
    let shown = |path: &Option<PathBuf>| path.as_deref().map(|p| p.display().to_string());
    if ui.json {
        let tools: Map<String, Value> = tools
            .iter()
            .map(|t| (t.name.to_string(), json!({"path": shown(&t.path), "version": t.version.as_ref().ok(), "error": t.version.as_ref().err()})))
            .collect();
        let proxy: Map<String, Value> = proxies.into_iter().map(|(name, value)| (name, value.into())).collect();
        let item: Item = vec![
            ("version", env!("CARGO_PKG_VERSION").into()),
            ("os", std::env::consts::OS.into()),
            ("arch", std::env::consts::ARCH.into()),
            ("data_folder", shown(&data_folder).into()),
            ("config", shown(&args.config_in_use).into()),
            ("tools", tools.into()),
            ("proxy", proxy.into()),
            ("output_folder", output.display().to_string().into()),
            ("free_space", free.into()),
        ];
        return ui.report(item);
    }
    let config = match (&args.config_in_use, &data_folder) {
        (Some(path), _) => path.display().to_string(),
        (None, Some(folder)) => format!("none ({} is read when it is there)", folder.join("cli.conf").display()),
        (None, None) => "none".to_string(),
    };
    let mut lines = vec![
        format!("Endo's Unified Downloader {} on {} {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH),
        format!("Data folder:  {}", shown(&data_folder).unwrap_or_else(|| "unknown".to_string())),
        format!("Config file:  {}", config),
    ];
    for tool in tools {
        let state = match (&tool.path, &tool.version) {
            (None, _) => "not found".to_string(),
            (Some(path), Ok(version)) => format!("{} ({})", path.display(), version),
            (Some(path), Err(e)) => format!("{}: {}", path.display(), e),
        };
        lines.push(format!("{:<13} {}", format!("{}:", tool.name), state));
    }
    let proxy = proxies.iter().map(|(name, value)| format!("{}={}", name, value)).collect::<Vec<_>>().join(", ");
    lines.push(format!("Proxy:        {}", if proxy.is_empty() { "none (no proxy variables are set)" } else { &proxy }));
    let free = free.map_or("unknown".to_string(), |bytes| HumanBytes(bytes).to_string());
    lines.push(format!("Free space:   {} in {}", free, output.display()));
    lines.iter().for_each(|line| ui.print(line));
}

/// The first line `program` prints when started with `args` (its version), waited for at most
/// `limit`; a program still running then is stopped.
async fn version_of(program: &Path, args: &[&str], limit: Duration) -> Result<String, String> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    // No console window of its own.
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let output = match tokio::time::timeout(limit, command.output()).await {
        Ok(output) => output.map_err(|e| format!("cannot run it: {}", e))?,
        Err(_) => return Err(format!("no answer within {} s", limit.as_secs())),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().map(str::trim).find(|line| !line.is_empty());
    // ffmpeg's goes on with its copyright.
    let line = line.map(|line| line.split(" Copyright").next().unwrap_or(line));
    line.map(|line| truncate(line, 80)).ok_or_else(|| format!("it tells no version ({})", output.status))
}

/// The proxy variables (any case) among `vars`, sorted, each with the sign-in of its link masked.
fn proxies(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    const NAMES: [&str; 4] = ["http_proxy", "https_proxy", "all_proxy", "no_proxy"];
    let mut found: Vec<(String, String)> = vars
        .filter(|(name, _)| NAMES.iter().any(|known| name.eq_ignore_ascii_case(known)))
        .map(|(name, value)| (name, masked(&value)))
        .collect();
    found.sort();
    found
}

/// `value` with the `user:password@` of the link in it as `***@`.
fn masked(value: &str) -> String {
    let start = value.find("://").map_or(0, |at| at + 3);
    let host_end = value[start..].find('/').map_or(value.len(), |end| start + end);
    match value[start..host_end].rfind('@') {
        Some(at) => format!("{}***{}", &value[..start], &value[start + at..]),
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn proxy_sign_ins_are_masked() {
        assert_eq!(masked("http://bob:s3cret@proxy.example:8080"), "http://***@proxy.example:8080");
        assert_eq!(masked("bob:pw@10.0.0.1:3128"), "***@10.0.0.1:3128");
        assert_eq!(masked("socks5h://127.0.0.1:9050"), "socks5h://127.0.0.1:9050");
        assert_eq!(masked("http://proxy.example/@path"), "http://proxy.example/@path", "only the host part");
        let vars = [("HTTPS_PROXY", "http://u:p@p.example:1"), ("PATH", "/bin"), ("no_proxy", "localhost"), ("HTTP_PROXY_X", "x")];
        let found = proxies(vars.iter().map(|(n, v)| (n.to_string(), v.to_string())));
        assert_eq!(found, [("HTTPS_PROXY".to_string(), "http://***@p.example:1".to_string()), ("no_proxy".into(), "localhost".into())]);
    }

    /// Run by [`a_tools_first_line_is_its_version_and_one_that_hangs_is_stopped`] as a tool that
    /// does not answer.
    #[test]
    #[ignore = "a stand-in for a tool that hangs"]
    fn hanging_tool() {
        std::thread::sleep(Duration::from_secs(30));
    }

    /// A tool's version is the first line it prints; one that does not answer in time is stopped
    /// and said so, one that cannot run too.
    #[tokio::test]
    async fn a_tools_first_line_is_its_version_and_one_that_hangs_is_stopped() {
        let exe = std::env::current_exe().unwrap();
        let listed = version_of(&exe, &["--list", "--exact", "doctor::tests::hanging_tool"], PROBE_TIMEOUT).await.unwrap();
        assert!(listed.starts_with("doctor::tests::hanging_tool"), "{listed}");
        let started = std::time::Instant::now();
        let hangs = ["--ignored", "--exact", "doctor::tests::hanging_tool"];
        let e = version_of(&exe, &hangs, Duration::from_millis(300)).await.unwrap_err();
        assert!(e.contains("no answer") && started.elapsed() < Duration::from_secs(10), "{e}");
        let e = version_of(Path::new("no-such-program-hf-doctor"), &["--version"], PROBE_TIMEOUT).await.unwrap_err();
        assert!(e.contains("cannot run it"), "{e}");
    }

    /// The --json checkup has every key, missing tools as not found and proxies masked; nothing
    /// goes online and the run succeeds.
    #[test]
    fn the_checkup_tells_what_is_missing() {
        let dir = std::env::temp_dir().join(format!("hf-cli-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("out.log");
        let mut ui = Ui::new(true);
        ui.json = true;
        ui.log = Some(std::sync::Arc::new(std::fs::File::create(&log).unwrap()));
        let args = Args::parse_from(["cli", "--doctor", "-d", dir.join("not-yet").to_str().unwrap()]);
        let tools = [Tool { name: "yt-dlp", path: None, version: Err("not found".into()) }];
        let vars = [(std::ffi::OsString::from("HTTP_PROXY"), std::ffi::OsString::from("http://bob:pw@p.example:3128"))].into_iter();
        checkup(&args, Some(dir.clone()), &tools, vars, &ui);
        drop(ui);
        let report: Value = serde_json::from_str(std::fs::read_to_string(&log).unwrap().trim()).unwrap();
        assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!((report["tools"]["yt-dlp"]["path"].clone(), report["tools"]["yt-dlp"]["error"].clone()), (Value::Null, json!("not found")));
        assert_eq!(report["proxy"]["HTTP_PROXY"], "http://***@p.example:3128");
        assert_eq!(report["config"], Value::Null);
        assert!(report["free_space"].as_u64().is_some_and(|free| free > 0), "the disk of the folder it would make: {report}");
        assert!(!dir.join("not-yet").exists(), "nothing is written");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
