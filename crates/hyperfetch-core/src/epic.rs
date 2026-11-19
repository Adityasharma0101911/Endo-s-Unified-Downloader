//! The user's Epic Games library through legendary (GPL-3.0), run with a config folder of the
//! app's own (`LEGENDARY_CONFIG_PATH`, see `helpers::account_dir`), so a separate legendary setup
//! is never read or changed. Sign-in is legendary's authorization-code flow: the user signs in on
//! Epic's own page (the one legendary's login link leads to) and pastes the code it shows; legendary
//! keeps the sign-in in `user.json` there. `epic://<app name>` links download a game, or a DLC into
//! its installed game, with legendary's own downloader (its chunked manifests give exact sizes),
//! into the download's folder; a stopped one resumes where it was. Nothing is launched or imported.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::accounts::Item;
use crate::engine::{DownloadOptions, EngineSnapshot};
use crate::helpers::{self, Progress, Proxy, Release, Tool};

/// legendary 0.21.1: its release files and their SHA-256 digests as GitHub gives them.
pub static LEGENDARY: Tool = Tool {
    program: "legendary",
    licence: "GPL-3.0",
    home: "https://github.com/derrod/legendary",
    version_args: &["--version"],
    releases: &[
        Release {
            target: "windows-x86_64",
            version: "0.21.1",
            url: "https://github.com/derrod/legendary/releases/download/0.21.1/legendary_windows_x64.exe",
            sha256: "6be77857dc0a6dd33ea7442ce8b5291944ec6da53757f81ca95b57ce794baf5b",
            size: 17_654_404,
        },
        Release {
            target: "windows-aarch64",
            version: "0.21.1",
            url: "https://github.com/derrod/legendary/releases/download/0.21.1/legendary_windows_arm64.exe",
            sha256: "5ca178d75713cb92ee24daa5df1905c75f05b8b403383669ca7aa32a5809fba3",
            size: 17_147_114,
        },
        Release {
            target: "linux-x86_64",
            version: "0.21.1",
            url: "https://github.com/derrod/legendary/releases/download/0.21.1/legendary_linux_x64",
            sha256: "dbed33bbe96031e65858233e4badf0a1d401bb6cb0cc995d1237cf3a0c826a45",
            size: 3_093_622,
        },
        Release {
            target: "linux-aarch64",
            version: "0.21.1",
            url: "https://github.com/derrod/legendary/releases/download/0.21.1/legendary_linux_arm64",
            sha256: "e901cb52ada10d5cef6ffb2a0985099958124457bfb455a571023d1e042143d9",
            size: 2_999_455,
        },
        Release {
            target: "macos-x86_64",
            version: "0.21.1",
            url: "https://github.com/derrod/legendary/releases/download/0.21.1/legendary_macOS_x64",
            sha256: "3dfab50277284b5bb0104f8710e4ff1d9ff00c992e44f5843fd0c977c5cc99c4",
            size: 15_566_864,
        },
        Release {
            target: "macos-aarch64",
            version: "0.21.1",
            url: "https://github.com/derrod/legendary/releases/download/0.21.1/legendary_macOS_arm64",
            sha256: "d87978321dba9cb731fab40c72f0a30bed55baca8b5341ab21024c5733cd837e",
            size: 15_275_984,
        },
    ],
};

/// Epic's sign-in page for the launcher's client, which legendary's login link
/// (legendary.gl/epiclogin) leads to: after the sign-in it shows the code to paste.
pub const SIGN_IN_PAGE: &str =
    "https://www.epicgames.com/id/login?redirectUrl=https%3A//www.epicgames.com/id/api/redirect%3FclientId%3D34a02cf8f4414e29b15921876da36f9a%26responseType%3Dcode";

/// `bin` (legendary) with `args`, its config in the app's folder.
fn command(bin: &Path, args: &[String], proxy: Proxy<'_>) -> Result<Command, String> {
    Ok(command_in(bin, args, proxy, &helpers::own_dir("epic")?))
}

/// `bin` (legendary) with `args`, its config in `dir`.
fn command_in(bin: &Path, args: &[String], proxy: Proxy<'_>, dir: &Path) -> Command {
    let mut cmd = helpers::command(bin, args, proxy, dir);
    cmd.env("LEGENDARY_CONFIG_PATH", dir);
    cmd
}

/// What legendary prints on its output with `args`, within `limit`; its last error line when it
/// fails.
async fn output(args: &[&str], proxy: Proxy<'_>, limit: Duration) -> Result<(Vec<u8>, String), String> {
    let bin = helpers::ensure(&LEGENDARY, proxy).await?;
    output_in(&bin, &helpers::own_dir("epic")?, args, proxy, limit).await
}

/// [`output`] of `bin` with its config in `dir`.
async fn output_in(bin: &Path, dir: &Path, args: &[&str], proxy: Proxy<'_>, limit: Duration) -> Result<(Vec<u8>, String), String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let output = helpers::output(command_in(bin, &args, proxy, dir), "legendary", limit).await?;
    let log = String::from_utf8_lossy(&output.stderr);
    let last = log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no message").trim().to_string();
    if !output.status.success() {
        return Err(format!("legendary failed: {last}"));
    }
    Ok((output.stdout, last))
}

/// Who is signed in to Epic, by the name legendary keeps. Blocking (a small file).
pub fn account() -> Option<String> {
    display_name(&std::fs::read(helpers::account_dir("epic").join("user.json")).ok()?)
}

/// The account's name in legendary's `user.json`, `bytes`.
fn display_name(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes).ok()?.get("displayName")?.as_str().map(str::to_string)
}

/// The code in what Epic's page shows: the code itself, or the whole JSON it is in.
fn auth_code(answer: &str) -> Option<String> {
    let answer = answer.trim();
    let code = match answer.starts_with('{') {
        true => serde_json::from_str::<serde_json::Value>(answer).ok()?.get("authorizationCode")?.as_str()?.to_string(),
        false => answer.trim_matches('"').to_string(),
    };
    (!code.is_empty() && code.len() <= 128 && code.chars().all(|c| c.is_ascii_alphanumeric())).then_some(code)
}

/// Signs in with the code from Epic's page (see [`SIGN_IN_PAGE`]); legendary trades it for its
/// sign-in at once (it is good once, for minutes). Returns the account's name.
pub async fn sign_in(answer: &str, proxy: Proxy<'_>) -> Result<String, String> {
    let code = auth_code(answer).ok_or("Paste the authorizationCode Epic's page shows (or the whole text)")?;
    let bin = helpers::ensure(&LEGENDARY, proxy).await?;
    sign_in_with(&bin, &helpers::own_dir("epic")?, &code, proxy).await
}

/// [`sign_in`] with legendary `bin`, its config in `dir`.
async fn sign_in_with(bin: &Path, dir: &Path, code: &str, proxy: Proxy<'_>) -> Result<String, String> {
    let file = dir.join("user.json");
    let before = tokio::fs::read(&file).await.ok();
    let (_, last) = output_in(bin, dir, &["auth", "--code", code], proxy, Duration::from_secs(120)).await?;
    // legendary ends well even when Epic refused the code: only a sign-in file it wrote now tells
    // (one of an earlier sign-in stays as it was).
    let after = tokio::fs::read(&file).await.ok().filter(|after| Some(after) != before.as_ref());
    after.and_then(|after| display_name(&after)).ok_or_else(|| format!("Epic did not take the code: {last}"))
}

/// Forgets the sign-in (legendary's `user.json` in the app's folder).
pub async fn sign_out() -> Result<(), String> {
    let file = helpers::account_dir("epic").join("user.json");
    match tokio::fs::remove_file(&file).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Cannot delete {}: {e}", file.display())),
        _ => Ok(()),
    }
}

#[derive(serde::Deserialize)]
struct Game {
    app_name: String,
    #[serde(default)]
    app_title: String,
    #[serde(default)]
    dlcs: Vec<Game>,
}

#[derive(serde::Deserialize)]
struct Installed {
    app_name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    install_path: String,
}

/// What legendary has installed through the app, by app name.
async fn installed(proxy: Proxy<'_>) -> Result<HashMap<String, Installed>, String> {
    let (json, _) = output(&["list-installed", "--json"], proxy, Duration::from_secs(120)).await?;
    let list: Vec<Installed> = serde_json::from_slice(&json).map_err(|e| format!("Unreadable list from legendary: {e}"))?;
    Ok(list.into_iter().map(|i| (i.app_name.clone(), i)).collect())
}

/// The games (and DLC) of the signed-in account.
pub async fn library(proxy: Proxy<'_>) -> Result<Vec<Item>, String> {
    let (json, _) = output(&["list", "--json"], proxy, Duration::from_secs(600)).await?;
    let games: Vec<Game> = serde_json::from_slice(&json).map_err(|e| format!("Unreadable library from legendary: {e}"))?;
    Ok(items(&games, &installed(proxy).await.unwrap_or_default()))
}

fn items(games: &[Game], installed: &HashMap<String, Installed>) -> Vec<Item> {
    let item = |game: &Game, title: String, kind: &str| Item {
        link: format!("epic://{}", game.app_name),
        title,
        size: None,
        installed: installed.get(&game.app_name).map(|i| i.version.clone()),
        kind: Some(kind.to_string()),
    };
    games
        .iter()
        .flat_map(|game| {
            std::iter::once(item(game, game.app_title.clone(), "game"))
                .chain(game.dlcs.iter().map(move |dlc| item(dlc, format!("{}: {}", game.app_title, dlc.app_title), "DLC")))
        })
        .collect()
}

/// The app name an `epic://` link names (never one legendary would read as a flag).
fn app_name(link: &Url) -> Option<String> {
    let name = link.host_str().filter(|h| !h.is_empty()).map(str::to_string).or_else(|| link.path().split('/').find(|s| !s.is_empty()).map(str::to_string))?;
    (!name.starts_with('-') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then_some(name)
}

/// legendary's arguments that download the game of `link` into `dir`: its DLC too with
/// `?dlcs=all`, for another system with `?platform=Mac` (or `Win32`), with the download's
/// connections as legendary's workers. legendary has no speed limit. The app name comes last,
/// after `--`: never a flag.
fn install_args(app: &str, link: &Url, options: &DownloadOptions, dir: &Path) -> Vec<String> {
    let query: HashMap<String, String> = link.query_pairs().map(|(k, v)| (k.to_ascii_lowercase(), v.into_owned())).collect();
    let mut args = vec!["-y".to_string(), "install".into(), "--base-path".into(), dir.to_string_lossy().into_owned(), "--skip-sdl".into()];
    args.push(if query.get("dlcs").is_some_and(|v| v == "all") { "--with-dlcs" } else { "--skip-dlcs" }.into());
    if let Some(platform) = query.get("platform").filter(|p| matches!(p.as_str(), "Windows" | "Win32" | "Mac")) {
        args.extend(["--platform".to_string(), platform.clone()]);
    }
    if options.num_connections > 0 {
        args.extend(["--max-workers".to_string(), options.num_connections.min(64).to_string()]);
    }
    args.extend(["--".to_string(), app.to_string()]);
    args
}

/// Reads one line of legendary's log into `progress`; true when it says there is nothing to
/// download (the game is up to date).
fn parse(line: &str, progress: &mut Progress) -> bool {
    let after = |key: &str| line.find(key).map(|at| &line[at + key.len()..]);
    if line.contains("Download size is 0") {
        return true;
    }
    if let Some(rest) = after("Download size: ") {
        progress.total = helpers::parse_size(rest.split(" (").next().unwrap_or(rest)).unwrap_or(progress.total);
    } else if let Some(rest) = after("= Progress: ") {
        if let Some(percent) = rest.split('%').next().and_then(|p| p.trim().parse::<f64>().ok()) {
            progress.ratio = Some(percent / 100.0);
        }
    } else if let Some(rest) = after(" - Downloaded: ") {
        progress.done = helpers::parse_size(rest.split(',').next().unwrap_or(rest)).unwrap_or(progress.done);
    } else if let Some(rest) = after(" + Download") {
        let speed = rest.trim_start_matches(['\t', ' ', '-']).split(" (").next().unwrap_or_default();
        progress.speed = helpers::parse_size(speed).map(|s| s as f64);
    }
    false
}

/// Downloads the game (or DLC) of the `epic://` link with legendary into `save_dir`; returns where
/// it is installed and whether anything was downloaded now.
pub(crate) async fn download(
    link: &Url,
    options: &DownloadOptions,
    save_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let app = app_name(link).ok_or_else(|| format!("{link} names no Epic game"))?;
    let proxy = Proxy::of(options);
    if options.max_speed.is_some() {
        tracing::warn!("legendary has no speed limit: {app} downloads at full speed");
    }
    let bin = helpers::ensure(&LEGENDARY, proxy).await?;
    tokio::fs::create_dir_all(save_dir).await.map_err(|e| format!("Cannot create {}: {e}", save_dir.display()))?;
    let cmd = command(&bin, &install_args(&app, link, options, save_dir), proxy)?;
    let mut nothing = false;
    let tail = helpers::run(cmd, "legendary", save_dir, |line, p| nothing |= parse(line, p), snapshots, cancel).await?;
    let found = installed(proxy).await?.remove(&app);
    let path = found.map(|i| PathBuf::from(i.install_path)).filter(|p| !p.as_os_str().is_empty());
    let path = path.ok_or_else(|| format!("legendary installed nothing: {}", tail.last().map(String::as_str).unwrap_or("no message")))?;
    Ok((path, !nothing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legendarys_log_and_lists_are_read() {
        let mut p = Progress::default();
        let lines = [
            "[cli] INFO: Install size: 2048.00 MiB",
            "[cli] INFO: Download size: 1024.00 MiB (Compression savings: 50.0%)",
            "[DLManager] INFO: = Progress: 12.50% (14/3262), Running for 00:00:05, ETA: 00:01:00",
            "[DLManager] INFO:  - Downloaded: 128.00 MiB, Written: 256.00 MiB",
            "[DLManager] INFO:  + Download\t- 6.75 MiB/s (raw) / 13.50 MiB/s (decompressed)",
        ];
        assert!(!lines.iter().any(|l| parse(l, &mut p)));
        assert_eq!(p, Progress { done: 128 << 20, total: 1024 << 20, ratio: Some(0.125), speed: Some(7_077_888.0) });
        assert!(parse("[cli] INFO: Download size is 0, the game is either already up to date or has not changed. Exiting...", &mut p));

        assert_eq!(auth_code(r#"{"redirectUrl":"https://localhost/launcher/authorized?code=x","authorizationCode":"0123abcd","sid":null}"#).as_deref(), Some("0123abcd"));
        assert_eq!(auth_code(" \"0123abcd\" ").as_deref(), Some("0123abcd"));
        assert!(auth_code("01 23").is_none() && auth_code("{}").is_none());

        let games: Vec<Game> = serde_json::from_str(r#"[{"app_name":"Sugar","app_title":"Sweet","metadata":{},"dlcs":[{"app_name":"SugarDLC","app_title":"More","dlcs":[]}]}]"#).unwrap();
        let installed = HashMap::from([("Sugar".to_string(), Installed { app_name: "Sugar".into(), version: "1.2".into(), install_path: "D:/Games/Sweet".into() })]);
        let items = items(&games, &installed);
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].link.as_str(), items[0].installed.as_deref()), ("epic://Sugar", Some("1.2")));
        assert_eq!((items[1].title.as_str(), items[1].kind.as_deref()), ("Sweet: More", Some("DLC")));

        let link = Url::parse("epic://Sugar?dlcs=all&platform=Mac").unwrap();
        assert_eq!(app_name(&link).as_deref(), Some("Sugar"));
        assert!(app_name(&Url::parse("epic://a%20b").unwrap()).is_none());
        assert!(app_name(&Url::parse("epic://--ignore-free-space").unwrap()).is_none(), "never a flag");
        let args = install_args("Sugar", &link, &DownloadOptions::default(), Path::new("D:/Games"));
        assert_eq!(args, ["-y", "install", "--base-path", "D:/Games", "--skip-sdl", "--with-dlcs", "--platform", "Mac", "--max-workers", "8", "--", "Sugar"]);
    }

    /// A fake legendary that prints its install log, then lists the game as installed: the download
    /// ends where legendary installed it, and a second run that finds nothing to download says so.
    #[tokio::test]
    async fn a_game_downloads_with_a_fake_legendary() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("Sweet");
        let listed = format!("[{{\"app_name\":\"Sugar\",\"version\":\"1\",\"install_path\":{}}}]", serde_json::to_string(&game.to_string_lossy()).unwrap());
        std::fs::write(dir.path().join("installed.json"), &listed).unwrap();
        let bin = helpers::tests::fake_tool(
            dir.path(),
            "legendary",
            &format!(
                "if \"%1\"==\"list-installed\" (type \"{0}\" & exit /b 0)\nif exist \"{1}\" (echo [cli] INFO: Download size is 0, the game is up to date. Exiting...>&2 & exit /b 0)\necho [cli] INFO: Download size: 1.00 MiB (Compression savings: 0.0%%)>&2\necho [DLManager] INFO: = Progress: 100.00%% (1/1), Running for 00:00:01, ETA: 00:00:00>&2\necho [DLManager] INFO:  - Downloaded: 1.00 MiB, Written: 1.00 MiB>&2\nmkdir \"{1}\"",
                dir.path().join("installed.json").display(),
                game.display()
            ),
            &format!(
                "if [ \"$1\" = list-installed ]; then cat '{0}'; exit 0; fi\nif [ -d '{1}' ]; then echo '[cli] INFO: Download size is 0, the game is up to date. Exiting...' >&2; exit 0; fi\necho '[cli] INFO: Download size: 1.00 MiB (Compression savings: 0.0%)' >&2\necho '[DLManager] INFO: = Progress: 100.00% (1/1), Running for 00:00:01, ETA: 00:00:00' >&2\necho '[DLManager] INFO:  - Downloaded: 1.00 MiB, Written: 1.00 MiB' >&2\nmkdir '{1}'",
                dir.path().join("installed.json").display(),
                game.display()
            ),
        );
        let run = |cancel: CancellationToken| {
            let (bin, dir) = (bin.clone(), dir.path().to_path_buf());
            async move {
                let link = Url::parse("epic://Sugar").unwrap();
                let cmd = command(&bin, &install_args("Sugar", &link, &DownloadOptions::default(), &dir), Proxy::default()).unwrap();
                let mut nothing = false;
                helpers::run(cmd, "legendary", &dir, |line, p| nothing |= parse(line, p), None, cancel).await.map(|_| nothing)
            }
        };
        assert!(!run(CancellationToken::new()).await.unwrap(), "the first run downloads");
        assert!(game.is_dir());
        assert!(run(CancellationToken::new()).await.unwrap(), "the second finds it up to date");
        let cmd = command(&bin, &["list-installed".to_string(), "--json".to_string()], Proxy::default()).unwrap();
        let out = helpers::run(cmd, "legendary", dir.path(), |_, _| {}, None, CancellationToken::new()).await.unwrap();
        assert!(out.join("").contains("Sugar"));
    }

    /// A code Epic refused leaves the sign-in file of an earlier sign-in as it was: that is no new
    /// sign-in. One legendary wrote now is.
    #[tokio::test]
    async fn a_refused_code_is_no_sign_in_even_after_an_earlier_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("user.json"), r#"{"displayName":"Old"}"#).unwrap();
        let refusing = helpers::tests::fake_tool(dir.path(), "refusing", "echo [cli] ERROR: Login attempt failed>&2", "echo '[cli] ERROR: Login attempt failed' >&2");
        let e = sign_in_with(&refusing, dir.path(), "0123", Proxy::default()).await.unwrap_err();
        assert!(e.contains("did not take the code") && e.contains("Login attempt failed"), "{e}");
        let taking = helpers::tests::fake_tool(
            dir.path(),
            "taking",
            "echo {\"displayName\":\"New\"}> \"%LEGENDARY_CONFIG_PATH%\\user.json\"",
            "printf '{\"displayName\":\"New\"}' > \"$LEGENDARY_CONFIG_PATH/user.json\"",
        );
        assert_eq!(sign_in_with(&taking, dir.path(), "0123", Proxy::default()).await.unwrap(), "New");
    }
}
