//! Steam depots through DepotDownloader (GPL-2.0, SteamKit2): the games of the user's own Steam
//! account, and free apps and dedicated servers anonymously. Sign-in is Steam's QR flow: the user
//! scans the code with the Steam app on their phone and confirms there; DepotDownloader then keeps
//! a remembered sign-in (never the password, which the app never asks for), and the app keeps the
//! account's name to ask for it again. DepotDownloader downloads only what Steam gives the account
//! (depot keys and manifests come from Steam for what it owns); nothing here gets around that.
//! Links: `steam://app/<app>[/depot/<depot>[/manifest/<manifest>]]` and
//! `steam://workshop/<app>/<item>`, with `?branch=`, `os=`, `arch=`, `language=`,
//! `all-platforms=1`, `all-languages=1`, `validate=1`. A store page's link is no such link: it
//! may come from any web page (through the browser extension), and never starts a download with
//! the user's account by itself. DepotDownloader has no proxy setting: under a proxy (the app's,
//! or one the environment names) it is not run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{DownloadOptions, EngineSnapshot};
use crate::helpers::{self, Progress, Proxy, Release, Tool};
use crate::media::ProcessTree;

/// DepotDownloader 3.4.0: its Windows builds, with the SHA-256 sums of winget's manifest of that
/// release (the project publishes none; for other systems put it on PATH yourself).
pub static DEPOT_DOWNLOADER: Tool = Tool {
    program: "DepotDownloader",
    licence: "GPL-2.0",
    home: "https://github.com/SteamRE/DepotDownloader",
    version_args: &["--version"],
    releases: &[
        Release {
            target: "windows-x86_64",
            version: "3.4.0",
            url: "https://github.com/SteamRE/DepotDownloader/releases/download/DepotDownloader_3.4.0/DepotDownloader-windows-x64.zip",
            sha256: "41c9e9f0df54b3ad02e67a11726756e5c73283bd7c2e1b04acfa5ae4c2ed3767",
            size: 33_474_005,
        },
        Release {
            target: "windows-aarch64",
            version: "3.4.0",
            url: "https://github.com/SteamRE/DepotDownloader/releases/download/DepotDownloader_3.4.0/DepotDownloader-windows-arm64.zip",
            sha256: "1449ba47775e9974036e615bedf00d72bef747cc5b93a78048ed4e0b2c63b2b3",
            size: 32_428_618,
        },
    ],
};

/// What to know of Steam downloads on this system.
pub const NOTE: &str = if cfg!(windows) {
    "DepotDownloader keeps its remembered Steam sign-in in .NET's storage for your Windows user (%LOCALAPPDATA%\\IsolatedStorage), \
     not in the app's folder, shared with any DepotDownloader you run yourself. Signing out here forgets the account's name but \
     leaves that sign-in: revoke it with Deauthorize all other devices in Steam Guard's settings. Steam downloads do not go \
     through a proxy. Add a game as steam://app/<id> (a store page's link downloads as a web page)."
} else {
    "Put DepotDownloader on PATH (no build is pinned for this system: the project publishes no checksums). \
     Its remembered Steam sign-in is kept in the app's folder, and signing out deletes it. Steam downloads do not go through \
     a proxy. Add a game as steam://app/<id> (a store page's link downloads as a web page)."
};

/// What signing out leaves behind, to say so: on Windows, DepotDownloader's remembered sign-in,
/// which .NET keeps for the Windows user in one store shared with every DepotDownloader run there,
/// so the app cannot take out its own alone.
pub const LEFT_AFTER_SIGN_OUT: Option<&str> = if cfg!(windows) {
    Some(
        "DepotDownloader's remembered Steam sign-in stays in .NET's storage for your Windows user (%LOCALAPPDATA%\\IsolatedStorage): \
         revoke it with Deauthorize all other devices in Steam Guard's settings",
    )
} else {
    None
};

/// A free app every account may read: the sign-in downloads nothing but its manifest.
const SIGN_IN_APP: &str = "228980";

fn account_file() -> PathBuf {
    helpers::account_dir("steam").join("account")
}

/// The name of the Steam account the app signs in with, if any. Blocking (a small file).
pub fn account() -> Option<String> {
    let name = std::fs::read_to_string(account_file()).ok()?;
    Some(name.trim().to_string()).filter(|n| account_name(n))
}

/// Whether `name` may be a Steam account's name.
fn account_name(name: &str) -> bool {
    (2..=64).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Forgets the account, and its remembered sign-in where the app's folder holds it (not on
/// Windows: see [`LEFT_AFTER_SIGN_OUT`]).
pub fn sign_out() -> Result<(), String> {
    let _ = std::fs::remove_file(account_file());
    let home = helpers::account_dir("steam").join("home");
    match std::fs::remove_dir_all(&home) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Cannot delete {}: {e}", home.display())),
        _ => Ok(()),
    }
}

/// DepotDownloader with `args`, its remembered sign-in in the app's folder where .NET lets it be
/// put there (see [`NOTE`]). `proxy` is one the accounts module checked it can follow: none.
fn command(bin: &Path, args: &[String], proxy: Proxy<'_>) -> Result<Command, String> {
    let dir = helpers::own_dir("steam")?;
    #[allow(unused_mut)]
    let mut cmd = helpers::command(bin, args, proxy, &dir);
    #[cfg(unix)]
    {
        let home = dir.join("home");
        std::fs::create_dir_all(home.join("share")).map_err(|e| format!("Cannot create {}: {e}", home.display()))?;
        cmd.env("HOME", &home).env("XDG_DATA_HOME", home.join("share"));
    }
    Ok(cmd)
}

/// A row of DepotDownloader's QR code as printed (two characters a module, whatever the console's
/// code page made of its blocks), in blocks and spaces.
fn qr_row(line: &str) -> String {
    line.chars().map(|c| if c == ' ' { ' ' } else { '█' }).collect()
}

/// Signs in to the Steam account `answer` names with Steam's QR flow: `show` gets each QR code to
/// scan (5 minutes at most). Returns the account's name.
pub async fn sign_in(answer: &str, proxy: Proxy<'_>, mut show: impl FnMut(String) + Send) -> Result<String, String> {
    let typed = answer.trim();
    if !account_name(typed) {
        return Err("Give the name you sign in to Steam with (letters, digits and _), never the password".to_string());
    }
    let bin = helpers::ensure(&DEPOT_DOWNLOADER, proxy).await?;
    let check = helpers::own_dir("steam")?.join("check");
    let args: Vec<String> = ["-qr", "-remember-password", "-app", SIGN_IN_APP, "-manifest-only", "-dir"].iter().map(|a| a.to_string()).chain([check.to_string_lossy().into_owned()]).collect();
    let signed = qr_sign_in(command(&bin, &args, proxy)?, &mut show, Duration::from_secs(300)).await;
    let _ = tokio::fs::remove_dir_all(&check).await;
    // Steam's own name for the account, when DepotDownloader tells it.
    let name = signed?.unwrap_or_else(|| typed.to_string());
    helpers::write_private(&account_file(), name.as_bytes())?;
    Ok(name)
}

/// Runs DepotDownloader's QR sign-in `cmd` until it has signed in (it has remembered the sign-in
/// by then), then stops it. Returns the account name it says, if it does.
async fn qr_sign_in(mut cmd: Command, show: &mut (impl FnMut(String) + Send), limit: Duration) -> Result<Option<String>, String> {
    let mut child = cmd.stdin(Stdio::null()).spawn().map_err(|e| format!("Failed to run DepotDownloader: {e}"))?;
    helpers::lower_priority(&child);
    let mut tree = ProcessTree::attach(&child);
    let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    let outputs: [Option<Box<dyn AsyncRead + Unpin + Send>>; 2] =
        [child.stdout.take().map(|o| Box::new(o) as _), child.stderr.take().map(|e| Box::new(e) as _)];
    for out in outputs.into_iter().flatten() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let (mut reader, mut line) = (BufReader::new(out), Vec::new());
            while matches!(reader.read_until(b'\n', &mut line).await, Ok(n) if n > 0) {
                let text = String::from_utf8_lossy(&line);
                let _ = tx.send(text.trim_end_matches(['\r', '\n']).to_string());
                line.clear();
            }
        });
    }
    drop(tx);
    let read = async {
        let (mut art, mut name, mut last) = (None::<Vec<String>>, None, String::new());
        while let Some(line) = lines.recv().await {
            if let Some(rows) = art.as_mut() {
                rows.push(qr_row(&line));
                let size = rows[0].chars().count() / 2;
                if rows.len() >= size || size > 200 {
                    show(rows.join("\n"));
                    art = None;
                }
                continue;
            }
            if line.contains("sign in with this QR code:") {
                art = Some(Vec::new());
            } else if let Some(rest) = line.split("login with -username ").nth(1) {
                name = rest.split_whitespace().next().filter(|n| account_name(n)).map(str::to_string);
            } else if line.contains("licenses for account") {
                return Ok(name);
            } else if line.contains("Failed to authenticate") || line.contains("Unable to login") {
                return Err(format!("Steam did not sign in: {}", line.trim()));
            } else if !line.trim().is_empty() {
                last = line;
            }
        }
        Err(format!("DepotDownloader stopped: {}", last.trim()))
    };
    let result = tokio::time::timeout(limit, read).await.unwrap_or_else(|_| Err(format!("The Steam sign-in was not finished within {} minutes", limit.as_secs() / 60)));
    tree.kill_and_reap(&mut child).await;
    result
}

/// DepotDownloader's arguments for what a `steam://` link names, and the folder of the download
/// folder it goes in.
fn plan(link: &Url) -> Result<(Vec<String>, String), String> {
    let bad = || format!("{link} names no Steam app (steam://app/<id>, steam://workshop/<app>/<item>)");
    let parts: Vec<&str> = link.path().split('/').filter(|s| !s.is_empty()).collect();
    let number = |s: &&str| s.parse::<u64>().ok().map(|n| n.to_string());
    let (mut args, folder) = match (link.host_str(), parts.as_slice()) {
        (Some("app" | "install"), [app, rest @ ..]) => {
            let app = number(app).ok_or_else(bad)?;
            let mut args = vec!["-app".to_string(), app.clone()];
            match rest {
                [] => {}
                ["depot", depot] => args.extend(["-depot".to_string(), number(depot).ok_or_else(bad)?]),
                ["depot", depot, "manifest", manifest] => {
                    args.extend(["-depot".to_string(), number(depot).ok_or_else(bad)?, "-manifest".to_string(), number(manifest).ok_or_else(bad)?])
                }
                _ => return Err(bad()),
            }
            (args, format!("Steam {app}"))
        }
        (Some("workshop"), [app, item]) => {
            let (app, item) = (number(app).ok_or_else(bad)?, number(item).ok_or_else(bad)?);
            (vec!["-app".to_string(), app, "-pubfile".to_string(), item.clone()], format!("Steam workshop {item}"))
        }
        _ => return Err(bad()),
    };
    let query: HashMap<String, String> = link.query_pairs().map(|(k, v)| (k.to_ascii_lowercase(), v.into_owned())).collect();
    // Never a flag of DepotDownloader's: it takes the first of a flag given twice.
    let word = |v: &String| !v.is_empty() && !v.starts_with('-') && v.len() <= 64 && v.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
    for (key, flag) in [("branch", "-branch"), ("os", "-os"), ("arch", "-osarch"), ("language", "-language")] {
        if let Some(value) = query.get(key) {
            if !word(value) {
                return Err(format!("{key}={value:?} is no Steam {key}"));
            }
            args.extend([flag.to_string(), value.clone()]);
        }
    }
    for key in ["all-platforms", "all-languages", "validate"] {
        if query.get(key).is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes")) {
            args.push(format!("-{key}"));
        }
    }
    Ok((args, folder))
}

/// Reads one line of DepotDownloader's output into `progress`: its share done ("` 12.34% path`",
/// in whatever decimal sign its locale has), and at the end the bytes it downloaded, which it
/// returns.
fn parse(line: &str, progress: &mut Progress) -> Option<u64> {
    if let Some(rest) = line.trim_start().strip_prefix("Total downloaded: ") {
        let bytes: u64 = rest.split_whitespace().next()?.parse().ok()?;
        (progress.done, progress.total, progress.ratio) = (bytes, bytes, Some(1.0));
        return Some(bytes);
    }
    let (head, path) = line.split_once("% ")?;
    let number = head.rsplit(|c: char| !(c.is_ascii_digit() || c == '.' || c == ',')).next()?;
    let percent: f64 = number.replace(',', ".").parse().ok()?;
    if (0.0..=100.0).contains(&percent) && !path.trim().is_empty() {
        progress.ratio = Some(percent / 100.0);
    }
    None
}

/// Downloads what the `steam://` link names into its folder in `save_dir`, signed in to the
/// user's account when there is one (anonymously otherwise); returns the folder and whether
/// anything was downloaded now.
pub(crate) async fn download(
    link: &Url,
    options: &DownloadOptions,
    save_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let (mut args, folder) = plan(link)?;
    if options.max_speed.is_some() {
        tracing::warn!("DepotDownloader has no speed limit: {link} downloads at full speed");
    }
    let bin = helpers::ensure(&DEPOT_DOWNLOADER, Proxy::of(options)).await?;
    let dir = save_dir.join(folder);
    tokio::fs::create_dir_all(&dir).await.map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    args.extend(["-dir".to_string(), dir.to_string_lossy().into_owned(), "-max-downloads".to_string(), options.num_connections.clamp(1, 64).to_string()]);
    if let Some(name) = tokio::task::spawn_blocking(account).await.ok().flatten() {
        args.extend(["-username".to_string(), name, "-remember-password".to_string(), "-no-mobile".to_string()]);
    }
    run_download(command(&bin, &args, Proxy::of(options))?, dir, snapshots, cancel).await
}

/// Runs DepotDownloader's download `cmd` into `dir`: done only when it says what it downloaded.
async fn run_download(cmd: Command, dir: PathBuf, snapshots: Option<broadcast::Sender<EngineSnapshot>>, cancel: CancellationToken) -> Result<(PathBuf, bool), String> {
    let (mut total, mut forgotten) = (None, false);
    let ran = helpers::run(
        cmd,
        "DepotDownloader",
        &dir,
        |line, p| {
            forgotten |= line.contains("Enter account password");
            total = parse(line, p).or(total);
        },
        snapshots,
        cancel,
    )
    .await;
    if forgotten {
        return Err("Steam no longer takes the remembered sign-in: sign in to Steam again".to_string());
    }
    ran?;
    match total {
        Some(bytes) => Ok((dir, bytes != 0)),
        None => Err("DepotDownloader ended without saying what it downloaded (no \"Total downloaded\"): the download may be incomplete".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_become_depotdownloaders_arguments() {
        let plan = |s: &str| plan(&Url::parse(s).unwrap());
        assert_eq!(plan("steam://app/440").unwrap(), (vec!["-app".to_string(), "440".to_string()], "Steam 440".to_string()));
        let (args, _) = plan("steam://app/440/depot/441/manifest/123456789?branch=beta&os=linux&all-languages=1").unwrap();
        assert_eq!(args, ["-app", "440", "-depot", "441", "-manifest", "123456789", "-branch", "beta", "-os", "linux", "-all-languages"]);
        assert_eq!(plan("steam://workshop/440/987").unwrap(), (vec!["-app".to_string(), "440".to_string(), "-pubfile".to_string(), "987".to_string()], "Steam workshop 987".to_string()));
        assert!(plan("steam://app/abc").is_err() && plan("steam://run/440").is_err() && plan("steam://app/440?branch=a%20b").is_err());
        let flag = plan("steam://app/440?language=-dir").unwrap_err();
        assert!(flag.contains("language"), "a value is never a flag: {flag}");
        assert!(account_name("gaben_1") && !account_name("a b") && !account_name("x"));

        let mut p = Progress::default();
        assert_eq!(parse(" 12.34% C:\\Games\\Steam 440\\tf\\a.vpk", &mut p), None);
        assert_eq!(p.ratio, Some(0.1234));
        assert_eq!(parse(" 56,78% /games/a.vpk", &mut p), None);
        assert_eq!(p.ratio, Some(0.5678));
        assert_eq!(parse("Encountered 404 for chunk x. Aborting.", &mut p), None);
        assert_eq!(parse("Total downloaded: 1048576 bytes (2097152 bytes uncompressed) from 2 depots", &mut p), Some(1_048_576));
        assert_eq!((p.done, p.total, p.ratio), (1_048_576, 1_048_576, Some(1.0)));
        assert_eq!(qr_row("  \u{2588}\u{2588}??  "), "  ████  ");
    }

    /// A fake DepotDownloader that shows a QR code (two rows), changes it, says who signed in and
    /// then waits as the real one goes on: each code is shown whole, and the sign-in ends at once.
    #[tokio::test]
    async fn the_qr_sign_in_shows_each_code_and_ends_once_signed_in() {
        let dir = tempfile::tempdir().unwrap();
        let bin = helpers::tests::fake_tool(
            dir.path(),
            "DepotDownloader",
            "echo Logging in with QR code...\necho Use the Steam Mobile App to sign in with this QR code:\necho(    \necho(##  \necho The QR code has changed:\necho Use the Steam Mobile App to sign in with this QR code:\necho(##  \necho(  ##\necho Success! Next time you can login with -username gaben_1 -remember-password instead of -qr.\necho Got 3 licenses for account!\nping -n 30 127.0.0.1 >nul",
            "echo 'Logging in with QR code...'\necho 'Use the Steam Mobile App to sign in with this QR code:'\necho '    '\necho '##  '\necho 'The QR code has changed:'\necho 'Use the Steam Mobile App to sign in with this QR code:'\necho '##  '\necho '  ##'\necho 'Success! Next time you can login with -username gaben_1 -remember-password instead of -qr.'\necho 'Got 3 licenses for account!'\nsleep 30",
        );
        let mut shown = Vec::new();
        let started = std::time::Instant::now();
        let cmd = helpers::command(&bin, &[], Proxy::default(), dir.path());
        let name = qr_sign_in(cmd, &mut |art: String| shown.push(art), Duration::from_secs(20)).await.unwrap();
        assert_eq!(name.as_deref(), Some("gaben_1"));
        assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert_eq!(shown[1], "██  \n  ██");

        let failing = helpers::tests::fake_tool(dir.path(), "failing", "echo Failed to authenticate with Steam: Expired>&2\nexit /b 1", "echo 'Failed to authenticate with Steam: Expired' >&2\nexit 1");
        let e = qr_sign_in(helpers::command(&failing, &[], Proxy::default(), dir.path()), &mut |_: String| {}, Duration::from_secs(20)).await.unwrap_err();
        assert!(e.contains("Expired"), "{e}");
    }

    /// A download is done only when DepotDownloader says what it downloaded: one that ends well
    /// without saying so is an error, not a fresh download.
    #[tokio::test]
    async fn a_download_is_done_only_when_depotdownloader_says_what_it_downloaded() {
        let dir = tempfile::tempdir().unwrap();
        let run = |name: &str, windows: &str, unix: &str| {
            let bin = helpers::tests::fake_tool(dir.path(), name, windows, unix);
            run_download(helpers::command(&bin, &[], Proxy::default(), dir.path()), dir.path().to_path_buf(), None, CancellationToken::new())
        };
        let done = run("done", "echo  50.00%% a.vpk\necho Total downloaded: 2048 bytes (4096 bytes uncompressed) from 1 depots", "echo ' 50.00% a.vpk'\necho 'Total downloaded: 2048 bytes (4096 bytes uncompressed) from 1 depots'").await;
        assert_eq!(done.unwrap(), (dir.path().to_path_buf(), true));
        let silent = run("silent", "echo  50.00%% a.vpk", "echo ' 50.00% a.vpk'").await.unwrap_err();
        assert!(silent.contains("Total downloaded"), "{silent}");
    }
}
