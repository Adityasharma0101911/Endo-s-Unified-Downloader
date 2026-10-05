//! Updates the app from its GitHub releases. Only what the maintainer signed is installed: a
//! release's `SHA256SUMS` must carry an Ed25519 signature made with the key whose public half is
//! [`PUBLIC_KEY`], and every file installed must match its sum there. Whoever takes over the GitHub
//! account or its downloads can therefore neither make an install run their code nor roll users
//! back: the sums name the version they are for, which must be the newer one offered.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::media::{http_client, http_get, quiet_command, rename_patiently, system_tar, unique_suffix, unpack, version_parts};

/// This app's GitHub releases. `/latest` redirects to `/tag/<tag>`; the files of a release are
/// under `/download/<tag>/`.
pub const RELEASES: &str = "https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader/releases";
/// The GUI, as named in a release and next to the CLI.
pub const GUI_EXE: &str = "Endos-Unified-Downloader.exe";
/// The command-line tool, as named in a release and next to the GUI.
pub const CLI_EXE: &str = "Endos-Unified-Downloader-CLI.exe";
/// The app's bundle on macOS, as a release's `.app.tar.gz` holds it (see [`bundle_asset`]); its
/// `Contents/MacOS` holds the two programs, named as above without `.exe`.
const BUNDLE: &str = "Endo's Unified Downloader.app";
/// What an update on macOS cannot do without: a bundle the user can write over.
const MOVE_THE_APP: &str = "Move the app to Applications, open it once, then update";
/// The maintainer's Ed25519 public key (raw, base64) a release's `SHA256SUMS.sig` must verify with.
/// The private half never leaves the maintainer's machine (see scripts/sign-release.mjs).
const PUBLIC_KEY: &str = "T0QVGQ63fXhyznv19veDUihSZBpBRxjGVOmPVoh85ms=";

/// Most a release's `SHA256SUMS`, or its signature, may be.
const SUMS_CAP: usize = 64 << 10;
/// Most any other release file may be.
const ASSET_CAP: usize = 300 << 20;

/// A release newer than this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// "1.4.0".
    pub version: String,
    /// "v1.4.0".
    pub tag: String,
    /// The release's page, to read what is new or download it by hand.
    pub page: String,
}

/// One of the two programs of a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    Gui,
    Cli,
}

/// What [`install`] replaced.
#[derive(Debug, Clone, Default)]
pub struct Installed {
    /// Every exe replaced, own first; on macOS the bundle.
    pub replaced: Vec<PathBuf>,
    /// Where the new build of the running program now is (its path before the update): what to
    /// restart (see [`restart`]).
    pub own: PathBuf,
    /// The extension folder next to the exes was replaced (on macOS [`refresh_extension`] copies
    /// the new bundle's at the next start).
    pub extension: bool,
}

/// `var`, in a debug build only: it points the updater at a test release and key.
fn debug_override(var: &str) -> Option<String> {
    if cfg!(debug_assertions) {
        std::env::var(var).ok()
    } else {
        None
    }
}

fn releases() -> String {
    debug_override("ENDO_UPDATE_RELEASES").unwrap_or_else(|| RELEASES.to_string())
}

/// Newest release when it is newer than this build (CARGO_PKG_VERSION), else None.
pub async fn check(proxy: Option<&str>) -> Result<Option<Update>, String> {
    let releases = releases();
    let resp = http_client(proxy)?
        .head(format!("{releases}/latest"))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("Failed to check for updates: {e}"))?;
    newer_release(&releases, resp.url().as_str(), env!("CARGO_PKG_VERSION"))
}

/// The release `latest` (where `{releases}/latest` led) when it is newer than `current`.
fn newer_release(releases: &str, latest: &str, current: &str) -> Result<Option<Update>, String> {
    let tag = latest.strip_prefix(&format!("{releases}/tag/")).ok_or_else(|| format!("Unexpected release URL {latest}"))?;
    // The tag goes into download URLs.
    let version = tag
        .strip_prefix('v')
        .filter(|version| version_parts(version).is_some_and(|parts| parts.len() == 3))
        .ok_or_else(|| format!("Unexpected release tag {tag}"))?;
    Ok(newer(version, current).then(|| Update { version: version.to_string(), tag: tag.to_string(), page: format!("{releases}/tag/{tag}") }))
}

/// Downloads, verifies and swaps in `update` (Windows and macOS; elsewhere Err naming
/// update.page). On Windows: the running program `own`, the other program when it is next to it,
/// and the browser extension's folder there. Nothing is replaced unless every file is verified and
/// written next to its target, and a failure while the programs are swapped puts the old ones
/// back. The old programs stay as `<name>.old.exe` until [`cleanup_old`], as a running one cannot
/// be deleted. On macOS the whole bundle the program runs from (see [`install_bundle_from`]).
pub async fn install(proxy: Option<&str>, update: &Update, own: Program) -> Result<Installed, String> {
    if !cfg!(any(windows, target_os = "macos")) {
        return Err(format!("Updating in place works on Windows and macOS only; download the new version from {}", update.page));
    }
    let exe = running_exe().map_err(|e| format!("Cannot find the running program: {e}"))?;
    let key = debug_override("ENDO_UPDATE_KEY").unwrap_or_else(|| PUBLIC_KEY.to_string());
    let (client, current) = (http_client(proxy)?, env!("CARGO_PKG_VERSION"));
    if cfg!(target_os = "macos") {
        return install_bundle_from(&client, &releases(), &key, current, update, &exe, cfg!(target_arch = "aarch64")).await;
    }
    install_from(&client, &releases(), &key, current, update, own, &exe).await
}

/// [`install`] of `update` from `releases`, signed with `key`, over `exe`, the running program
/// `own` of version `current`.
async fn install_from(
    client: &reqwest::Client,
    releases: &str,
    key: &str,
    current: &str,
    update: &Update,
    own: Program,
    exe: &Path,
) -> Result<Installed, String> {
    // check offers only newer releases; no other caller rolls back either.
    if !newer(&update.version, current) {
        return Err(format!("Version {} is not newer than this one ({current})", update.version));
    }
    let dir = exe.parent().ok_or("The running program has no folder")?;
    // The other program may have installed a release since this one started; the extension's
    // version, which a release shares, says which.
    if let Some(installed) = extension_version(dir).filter(|installed| !newer(&update.version, installed)) {
        return Err(format!("Version {installed} is already installed; restart the app to use it"));
    }
    let (own_asset, other_asset) = match own {
        Program::Gui => (GUI_EXE, CLI_EXE),
        Program::Cli => (CLI_EXE, GUI_EXE),
    };
    let mut targets = vec![(exe.to_path_buf(), own_asset)];
    if dir.join(other_asset).is_file() {
        targets.push((dir.join(other_asset), other_asset));
    }
    let extension = dir
        .join("extension")
        .join("manifest.json")
        .is_file()
        .then(|| format!("Endos-Unified-Downloader-Extension-{}.zip", update.tag));

    let assets: Vec<&str> = targets.iter().map(|(_, asset)| *asset).chain(extension.as_deref()).collect();
    let mut files = fetch_verified(client, releases, key, update, &assets).await?;

    let zip = if extension.is_some() { files.pop() } else { None };
    let (dir, exes): (_, Vec<PathBuf>) = (dir.to_path_buf(), targets.into_iter().map(|(path, _)| path).collect());
    tokio::task::spawn_blocking(move || put_in_place(&dir, exes, files, zip))
        .await
        .map_err(|e| format!("Update task failed: {e}"))?
}

/// The files `assets` of `update` from `releases`, each checked against the release's
/// `SHA256SUMS`, which must carry `key`'s signature and be for `update`'s version.
async fn fetch_verified(client: &reqwest::Client, releases: &str, key: &str, update: &Update, assets: &[&str]) -> Result<Vec<Vec<u8>>, String> {
    let release = format!("{releases}/download/{}", update.tag);
    let listing = fetch(client, &format!("{release}/SHA256SUMS"), SUMS_CAP, Duration::from_secs(30)).await?;
    let signature = fetch(client, &format!("{release}/SHA256SUMS.sig"), SUMS_CAP, Duration::from_secs(30)).await?;
    // Nothing of an unsigned listing is read.
    verify_signature(key, &listing, &signature)?;
    let sums = parse_sums(&listing, &update.version)?;
    let mut files = Vec::new();
    for asset in assets {
        let expected = sums.get(asset).ok_or_else(|| format!("The update's SHA256SUMS has no entry for {asset}"))?;
        let body = fetch(client, &format!("{release}/{asset}"), ASSET_CAP, Duration::from_secs(600)).await?;
        let actual = format!("{:x}", Sha256::digest(&body));
        if actual != *expected {
            return Err(format!("Downloaded {asset} failed checksum verification (expected {expected}, got {actual})"));
        }
        files.push(body);
    }
    Ok(files)
}

/// The macOS release file of the app's bundle for a Mac with an Apple Silicon processor (`arm`)
/// or an Intel one.
fn bundle_asset(arm: bool) -> &'static str {
    if arm {
        "Endos-Unified-Downloader-macos-arm64.app.tar.gz"
    } else {
        "Endos-Unified-Downloader-macos-x64.app.tar.gz"
    }
}

/// The `.app` bundle the program `exe` is in, when it is a bundle's `Contents/MacOS/<program>`.
pub fn bundle_of(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let inside = macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && bundle.extension()? == "app";
    inside.then_some(bundle)
}

/// Whether macOS runs `path` from a read-only copy of where it was downloaded (App Translocation,
/// for an app never moved out of Downloads or a disk image), which cannot be updated.
fn translocated(path: &Path) -> bool {
    path.to_string_lossy().contains("/AppTranslocation/")
}

/// `CFBundleShortVersionString` of an Info.plist in XML, when it parses as a version.
fn plist_version(plist: &str) -> Option<String> {
    let (_, after) = plist.split_once("<key>CFBundleShortVersionString</key>")?;
    let version = after.trim_start().strip_prefix("<string>")?.split_once("</string>")?.0.trim();
    version_parts(version).map(|_| version.to_string())
}

/// The version of the bundle `bundle` (see [`plist_version`]). Blocking.
fn bundle_version(bundle: &Path) -> Option<String> {
    plist_version(&std::fs::read_to_string(bundle.join("Contents").join("Info.plist")).ok()?)
}

/// [`install`] on macOS of `update` from `releases`, signed with `key`, over the bundle the
/// program `exe` of version `current` runs from: the release's bundle for this Mac (`arm`: Apple
/// Silicon, see [`bundle_asset`]) is checked like the Windows programs, unpacked in a folder beside
/// the bundle (the same volume) and swapped in whole (see [`put_bundle_in_place`]). The GUI and
/// the CLI, both in the bundle, are updated together.
async fn install_bundle_from(
    client: &reqwest::Client,
    releases: &str,
    key: &str,
    current: &str,
    update: &Update,
    exe: &Path,
    arm: bool,
) -> Result<Installed, String> {
    if !newer(&update.version, current) {
        return Err(format!("Version {} is not newer than this one ({current})", update.version));
    }
    let bundle = bundle_of(exe).ok_or_else(|| format!("This copy of the app is not in its .app bundle; download the new version from {}", update.page))?;
    if translocated(bundle) {
        return Err(format!("macOS runs this copy of the app from a read-only place. {MOVE_THE_APP}"));
    }
    // The other program may have installed a release since this one started.
    if let Some(installed) = bundle_version(bundle).filter(|installed| !newer(&update.version, installed)) {
        return Err(format!("Version {installed} is already installed; restart the app to use it"));
    }
    // Before anything is downloaded: also whether the bundle's folder can be written.
    let parent = bundle.parent().ok_or("The app's bundle has no folder")?;
    let staging = parent.join(format!(".endo-update-{}.tmp", unique_suffix()));
    std::fs::create_dir(&staging).map_err(|e| format!("Cannot write next to {}: {e}. {MOVE_THE_APP}", bundle.display()))?;
    let fetched = fetch_verified(client, releases, key, update, &[bundle_asset(arm)]).await;
    let installed = match fetched {
        Ok(mut files) => {
            let own = exe.file_name().unwrap_or_default().to_os_string();
            let (bundle, staging, version) = (bundle.to_path_buf(), staging.clone(), update.version.clone());
            tokio::task::spawn_blocking(move || put_bundle_in_place(&bundle, &staging, &files.remove(0), &version, &own))
                .await
                .map_err(|e| format!("Update task failed: {e}"))
                .and_then(|done| done)
        }
        Err(e) => Err(e),
    };
    // The old bundle, moved aside next to it, still runs: it goes at the next start (see cleanup_old).
    let _ = std::fs::remove_dir_all(&staging);
    installed.map(|()| Installed { replaced: vec![bundle.to_path_buf()], own: exe.to_path_buf(), extension: false })
}

/// Unpacks `archive`, a release's `.app.tar.gz` (see [`bundle_asset`]), in `staging`, a new folder
/// beside `bundle`, and swaps the [`BUNDLE`] it holds in for `bundle` (see [`swap_bundle`]) once it
/// is whole: no entry of it would land outside `staging` (see [`escapes`]) nor is a link, it has
/// the program `own` and its Info.plist is of `version`. Blocking.
fn put_bundle_in_place(bundle: &Path, staging: &Path, archive: &[u8], version: &str, own: &std::ffi::OsStr) -> Result<(), String> {
    let (tar, file, unpacked) = (system_tar(), staging.join("app.tar.gz"), staging.join("app"));
    std::fs::write(&file, archive).map_err(|e| format!("Failed to write {}: {e}", file.display()))?;
    entries_stay_inside(&tar, &file, "app")?;
    std::fs::create_dir(&unpacked)
        .and_then(|()| unpack(&tar, &file, &unpacked, &[]))
        .map_err(|e| format!("Failed to unpack the update: {e}"))?;
    if crate::postprocess::has_links(&unpacked) {
        return Err("The update's app holds links, which could lead outside it".to_string());
    }
    let new = unpacked.join(BUNDLE);
    if !new.join("Contents").join("MacOS").join(own).is_file() {
        return Err(format!("The update's app has no {}", own.to_string_lossy()));
    }
    match bundle_version(&new) {
        Some(found) if found == version => swap_bundle(bundle, &new),
        found => Err(format!("The update's app is version {}, not {version}", found.as_deref().unwrap_or("unknown"))),
    }
}

/// Where `bundle` is moved to make way for its new build: `.<name>.old-<unique>` next to it,
/// hidden, until [`cleanup_old`].
fn old_bundle(bundle: &Path) -> PathBuf {
    let name = bundle.file_name().unwrap_or_default().to_string_lossy();
    bundle.with_file_name(format!(".{name}.old-{}", unique_suffix()))
}

/// Moves `bundle` aside (see [`old_bundle`]) and `new` into its place, putting it back when that
/// fails. A running program goes on from the bundle moved aside. Blocking.
fn swap_bundle(bundle: &Path, new: &Path) -> Result<(), String> {
    let old = old_bundle(bundle);
    std::fs::rename(bundle, &old).map_err(|e| format!("Failed to move {} aside: {e}. {MOVE_THE_APP}", bundle.display()))?;
    std::fs::rename(new, bundle).or_else(|e| {
        if let Err(e) = std::fs::rename(&old, bundle) {
            tracing::error!("Could not put {} back from {}: {e}", bundle.display(), old.display());
        }
        Err(format!("Failed to replace {}: {e}", bundle.display()))
    })
}

/// Starts the updated program `exe` (see [`Installed::own`]) once this one closes: on macOS its
/// bundle, as a new instance of the app (`open -n`), else `exe` itself.
pub fn restart(exe: &Path) -> std::io::Result<()> {
    let mut command = match bundle_of(exe) {
        Some(bundle) if cfg!(target_os = "macos") => {
            let mut open = std::process::Command::new("/usr/bin/open");
            open.arg("-n").arg(bundle);
            open
        }
        _ => std::process::Command::new(exe),
    };
    command.spawn().map(drop)
}

/// Body of `url`, refused once it grows past `cap` bytes.
async fn fetch(client: &reqwest::Client, url: &str, cap: usize, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut resp = http_get(client, url, timeout).await?;
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("Failed to download {url}: {e}"))? {
        if body.len() + chunk.len() > cap {
            return Err(format!("{url} is larger than an update file may be"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Checks `signature` (`SHA256SUMS.sig`: padded base64, whitespace around it ignored) is `key`'s
/// Ed25519 signature of exactly `sums`.
fn verify_signature(key: &str, sums: &[u8], signature: &[u8]) -> Result<(), String> {
    let base64 = base64::engine::general_purpose::STANDARD;
    let key = base64.decode(key).map_err(|e| format!("Invalid update key: {e}"))?;
    let signature = std::str::from_utf8(signature)
        .ok()
        .and_then(|signature| base64.decode(signature.trim()).ok())
        .ok_or("The update's signature is malformed")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(sums, &signature)
        .map_err(|_| "The update's signature does not match: it was not made with this app's key".to_string())
}

/// The sums of a release's `SHA256SUMS` by file name. The whole file is refused unless it is
/// exactly `# version <version>` and then one `<64 lower-case hex>  <file name>` line per file,
/// each ending in `\n`, no name twice.
fn parse_sums<'a>(sums: &'a [u8], version: &str) -> Result<HashMap<&'a str, &'a str>, String> {
    let malformed = || "The update's SHA256SUMS is malformed".to_string();
    let text = std::str::from_utf8(sums).ok().and_then(|text| text.strip_suffix('\n')).ok_or_else(malformed)?;
    let mut lines = text.split('\n');
    let signed = lines.next().and_then(|line| line.strip_prefix("# version ")).ok_or_else(malformed)?;
    // An older signed release offered as this one.
    if signed != version {
        return Err(format!("The update's SHA256SUMS is for version {signed}, not {version}"));
    }
    let mut hashes = HashMap::new();
    for line in lines {
        let (hash, name) = line.split_once("  ").ok_or_else(malformed)?;
        let hex = hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        let named = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if !hex || !named || hashes.insert(name, hash).is_some() {
            return Err(malformed());
        }
    }
    Ok(hashes)
}

/// Writes each of `exes`' new build (`builds`, in the same order) next to it (see [`new_name`]) and
/// unpacks the extension's `zip` next to its folder, then swaps the programs in (see [`swap`]) and
/// the extension (see [`replace_extension`]). Nothing is swapped when anything before fails, and
/// nothing written is left. Blocking.
fn put_in_place(dir: &Path, exes: Vec<PathBuf>, builds: Vec<Vec<u8>>, zip: Option<Vec<u8>>) -> Result<Installed, String> {
    let unpacked = dir.join(format!(".extension-{}.tmp", unique_suffix()));
    let ready = exes
        .iter()
        .zip(&builds)
        .try_for_each(|(exe, build)| {
            let new = new_name(exe);
            // On disk before it is swapped in, so cleanup_old can finish a swap cut off even by a power loss.
            std::fs::File::create(&new)
                .and_then(|mut file| file.write_all(build).and_then(|()| file.sync_all()))
                .map_err(|e| format!("Failed to write {}: {e}", new.display()))
        })
        .and_then(|()| zip.as_deref().map_or(Ok(()), |zip| unpack_extension(zip, &unpacked)))
        .and_then(|()| swap(&exes));
    if let Err(e) = ready {
        for exe in &exes {
            let _ = std::fs::remove_file(new_name(exe));
        }
        let _ = std::fs::remove_dir_all(&unpacked);
        return Err(e);
    }
    let extension = zip.is_some() && replace_extension(dir, &unpacked);
    Ok(Installed { own: exes[0].clone(), replaced: exes, extension })
}

/// Where `exe`'s new build is written before it is swapped in: `<name>.new`.
fn new_name(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".new");
    name.into()
}

/// Where `exe` is moved to make way for its new build: `<stem>.old.exe`, deleted first, or
/// `<stem>.old-<unique>.exe` when it cannot be (a program an earlier update replaced still runs).
/// Blocking.
fn old_name(exe: &Path) -> PathBuf {
    let stem = exe.file_stem().unwrap_or_default().to_string_lossy();
    let old = exe.with_file_name(format!("{stem}.old.exe"));
    match std::fs::remove_file(&old) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => exe.with_file_name(format!("{stem}.old-{}.exe", unique_suffix())),
        _ => old,
    }
}

/// Moves each of `exes` aside (see [`old_name`]) and its new build (see [`new_name`]) into its
/// place: Windows lets a running program be moved, not overwritten. On a failure the ones already
/// moved aside are put back. Blocking.
fn swap(exes: &[PathBuf]) -> Result<(), String> {
    let mut moved = Vec::new();
    for exe in exes {
        let old = old_name(exe);
        let mut swapped = rename_patiently(exe, &old);
        if swapped.is_ok() {
            moved.push((exe, old));
            swapped = rename_patiently(&new_name(exe), exe);
        }
        if let Err(e) = swapped {
            for (exe, old) in moved.iter().rev() {
                if let Err(e) = std::fs::rename(old, exe) {
                    tracing::error!("Could not put {} back from {}: {e}", exe.display(), old.display());
                }
            }
            return Err(format!("Failed to replace {}: {e}", exe.display()));
        }
    }
    Ok(())
}

/// Unpacks the extension's release `zip` into the new folder `into`, unless an entry of it would
/// land outside that folder, and checks it is an extension (see [`manifest_version`]). Blocking.
fn unpack_extension(zip: &[u8], into: &Path) -> Result<(), String> {
    // In a folder next to `into` that cleanup_old deletes, not in %TEMP%: there, when the app runs
    // elevated, another program of the user's could swap the zip after its sum was checked.
    let (tar, staging) = (system_tar(), into.with_extension("zip.tmp"));
    let archive = staging.join("extension.zip");
    let unpacked = std::fs::create_dir(&staging)
        .and_then(|()| std::fs::write(&archive, zip))
        .map_err(|e| format!("Failed to write {}: {e}", archive.display()))
        .and_then(|()| entries_stay_inside(&tar, &archive, "browser extension"))
        .and_then(|()| {
            std::fs::create_dir(into)
                .and_then(|()| unpack(&tar, &archive, into, &[]))
                .map_err(|e| format!("Failed to unpack the browser extension: {e}"))
        });
    let _ = std::fs::remove_dir_all(&staging);
    unpacked?;
    manifest_version(into).map(drop).ok_or_else(|| "The update's browser extension has no manifest.json with a version".to_string())
}

/// Fails unless every entry of `archive`, as `tar` lists them, stays inside the folder it is
/// unpacked into (see [`escapes`]); `what` names the archive. Blocking.
fn entries_stay_inside(tar: &Path, archive: &Path, what: &str) -> Result<(), String> {
    match quiet_command(tar).arg("-tf").arg(archive).stdin(Stdio::null()).output() {
        Ok(out) if out.status.success() => match String::from_utf8_lossy(&out.stdout).lines().find(|entry| escapes(entry)) {
            Some(entry) => Err(format!("The update's {what} has a file outside its folder: {entry}")),
            None => Ok(()),
        },
        Ok(out) => Err(format!("Failed to list the {what}: {}", String::from_utf8_lossy(&out.stderr).trim())),
        Err(e) => Err(format!("Failed to run {}: {e}", tar.display())),
    }
}

/// Whether zip entry `name` would land outside the folder it is unpacked into: absolute, on a
/// drive, through `..`, or with a backslash, which Windows takes for a folder separator.
pub(crate) fn escapes(name: &str) -> bool {
    name.starts_with('/') || name.contains(['\\', ':']) || name.split('/').any(|part| part == "..")
}

/// Swaps the extension unpacked in `unpacked` (see [`unpack_extension`]) in for `dir/extension`,
/// putting the old folder back on a failure. Best effort: the browser keeps running the extension
/// it loaded. Returns whether it was replaced. Blocking.
fn replace_extension(dir: &Path, unpacked: &Path) -> bool {
    let (current, old) = (dir.join("extension"), dir.join(format!(".extension-old-{}", unique_suffix())));
    let replaced = rename_patiently(&current, &old).and_then(|()| {
        rename_patiently(unpacked, &current).inspect_err(|_| {
            let _ = std::fs::rename(&old, &current);
        })
    });
    if let Err(e) = replaced {
        tracing::warn!("Could not replace the browser extension in {}: {e}", current.display());
        let _ = std::fs::remove_dir_all(unpacked);
        return false;
    }
    // One the browser holds a file of open stays for cleanup_old.
    let _ = std::fs::remove_dir_all(&old);
    true
}

/// Best effort, blocking: deletes what an earlier update left next to the running exe: the
/// programs it replaced, once they no longer run, and what it did not get to delete. What one cut
/// off midway had moved aside is put back first. On macOS, next to the running bundle (see
/// [`cleanup_beside`]).
pub fn cleanup_old() {
    let Ok(exe) = running_exe() else { return };
    match bundle_of(&exe) {
        Some(bundle) if cfg!(target_os = "macos") => cleanup_beside(bundle),
        _ => {
            if let Some(dir) = exe.parent() {
                cleanup_in(dir);
            }
        }
    }
}

/// [`cleanup_old`] of the bundle `bundle`, while it is there: deletes the bundles updates moved
/// aside next to it (see [`old_bundle`]) and the folders they unpacked in (`.endo-update-*.tmp`).
fn cleanup_beside(bundle: &Path) {
    let (Some(parent), Some(name)) = (bundle.parent(), bundle.file_name()) else { return };
    if !bundle.is_dir() {
        return;
    }
    let moved = format!(".{}.old-", name.to_string_lossy());
    for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let left = name.starts_with(&moved) || (name.starts_with(".endo-update-") && name.ends_with(".tmp"));
        if left && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// [`cleanup_old`] in `dir`: only `<stem>.old.exe`, `<stem>.old-*.exe` and `<stem>.exe.new` of the
/// two programs, and the folders `.extension-old-*` and `.extension-*.tmp`. An update cut off
/// between moving a program aside and its new build in (see [`swap`]) is finished first, and one
/// cut off between moving the extension's folder aside and the new one in (see
/// [`replace_extension`]) is undone; a program or extension still missing keeps all it left.
fn cleanup_in(dir: &Path) {
    for exe in [GUI_EXE, CLI_EXE] {
        let exe = dir.join(exe);
        if !exe.exists() {
            let _ = std::fs::rename(new_name(&exe), &exe);
        }
    }
    let extension = dir.join("extension");
    if !extension.exists() {
        // The one moved aside last, as its name starts with the time (see unique_suffix): an
        // earlier one may be half deleted.
        let moved = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().starts_with(".extension-old-"))
            .max();
        if let Some(moved) = moved {
            let _ = std::fs::rename(dir.join(moved), &extension);
        }
    }
    let present: Vec<&str> = [GUI_EXE, CLI_EXE].into_iter().filter(|exe| dir.join(exe).exists()).collect();
    let extension_present = extension.exists();
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let (name, Ok(kind)) = (entry.file_name().to_string_lossy().into_owned(), entry.file_type()) else { continue };
        let left_exe = present.iter().any(|exe| {
            let stem = exe.trim_end_matches(".exe");
            name == format!("{exe}.new")
                || name == format!("{stem}.old.exe")
                || name.strip_prefix(&format!("{stem}.old-")).is_some_and(|rest| rest.ends_with(".exe"))
        });
        let left_dir = extension_present
            && (name.starts_with(".extension-old-") || (name.starts_with(".extension-") && name.ends_with(".tmp")));
        // A program still running cannot be deleted: it goes next time.
        if kind.is_file() && left_exe {
            let _ = std::fs::remove_file(entry.path());
        } else if kind.is_dir() && left_dir {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Whether version `candidate` is newer than `current`, by their numbers; an unparsable one never is.
pub fn newer(candidate: &str, current: &str) -> bool {
    matches!((version_parts(candidate), version_parts(current)), (Some(candidate), Some(current)) if candidate > current)
}

/// `version` of `<dir>/extension/manifest.json` when it parses as a version.
pub fn extension_version(dir: &Path) -> Option<String> {
    manifest_version(&dir.join("extension"))
}

/// The browser extension's folder, the one a browser loads it from ("Load unpacked"): next to the
/// programs, except on macOS, where it is in the app's data folder, as a bundle must stay as it
/// was signed; [`refresh_extension`] copies the bundle's there.
pub fn extension_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        crate::media::app_data_dir().map(|dir| dir.join("extension"))
    } else {
        app_dir().map(|dir| dir.join("extension"))
    }
}

/// On macOS, puts the browser extension of the running bundle (`Contents/Resources/extension`)
/// into [`extension_dir`] when that has none or an older one (see [`copy_extension`]). Nothing
/// elsewhere, or outside a bundle. Best effort, blocking.
pub fn refresh_extension() {
    let (Ok(exe), Some(to)) = (running_exe(), extension_dir()) else { return };
    let Some(bundle) = bundle_of(&exe).filter(|_| cfg!(target_os = "macos")) else { return };
    if let Err(e) = copy_extension(&bundle.join("Contents").join("Resources").join("extension"), &to) {
        tracing::warn!("Could not put the browser extension into {}: {e}", to.display());
    }
}

/// Copies the extension folder `from` to `to`, a folder named `extension`, when `to` has no
/// version or an older one than `from`: into a folder beside it first, which then takes its place
/// (see [`replace_extension`]). What an earlier copy left there goes first (see [`cleanup_in`]).
/// Returns whether it copied. Blocking.
fn copy_extension(from: &Path, to: &Path) -> std::io::Result<bool> {
    let dir = to.parent().ok_or_else(|| std::io::Error::other("the extension folder has no parent"))?;
    cleanup_in(dir);
    let Some(version) = manifest_version(from) else { return Ok(false) };
    if manifest_version(to).is_some_and(|have| !newer(&version, &have)) {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    let staged = dir.join(format!(".extension-{}.tmp", unique_suffix()));
    if let Err(e) = copy_dir(from, &staged) {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e);
    }
    if !to.exists() {
        return std::fs::rename(&staged, to).map(|()| true);
    }
    Ok(replace_extension(dir, &staged))
}

/// Copies the folder `from` into the new folder `to`, all the way down; links are left out. Blocking.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (kind, target) = (entry.file_type()?, to.join(entry.file_name()));
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// `version` of `folder/manifest.json` when it parses as a version. Blocking.
pub fn manifest_version(folder: &Path) -> Option<String> {
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(folder.join("manifest.json")).ok()?).ok()?;
    let version = manifest["version"].as_str()?;
    version_parts(version).map(|_| version.to_string())
}

/// The running program; on macOS where links lead, so that a link to the CLI (in /usr/local/bin,
/// say) finds the bundle it is in.
fn running_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(if cfg!(target_os = "macos") { std::fs::canonicalize(&exe).unwrap_or(exe) } else { exe })
}

/// The running exe's folder.
pub fn app_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::tests::{names_of, serve_files};
    use ring::signature::{Ed25519KeyPair, KeyPair};

    /// A new key pair, with its public half as [`PUBLIC_KEY`] holds one.
    fn test_key() -> (Ed25519KeyPair, String) {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public = base64::engine::general_purpose::STANDARD.encode(pair.public_key());
        (pair, public)
    }

    /// `SHA256SUMS.sig` of `sums`, as scripts/sign-release.mjs writes it.
    fn sign(pair: &Ed25519KeyPair, sums: &[u8]) -> Vec<u8> {
        format!("{}\n", base64::engine::general_purpose::STANDARD.encode(pair.sign(sums))).into_bytes()
    }

    /// Serves `files` as release v1.4.0 does, with the `SHA256SUMS` of `summed`, signed by
    /// `signer`. Resolves to the base URL of the releases.
    fn serve_release(files: &[(&str, &[u8])], summed: &[(&str, &[u8])], signer: &Ed25519KeyPair) -> impl std::future::Future<Output = String> {
        let mut sums = String::from("# version 1.4.0\n");
        let mut summed = summed.to_vec();
        summed.sort();
        for (name, body) in summed {
            sums += &format!("{:x}  {name}\n", Sha256::digest(body));
        }
        let at = |name: &str| format!("/release/download/v1.4.0/{name}");
        let mut served: Vec<(String, Vec<u8>)> = files.iter().map(|(name, body)| (at(name), body.to_vec())).collect();
        served.push((at("SHA256SUMS.sig"), sign(signer, sums.as_bytes())));
        served.push((at("SHA256SUMS"), sums.into_bytes()));
        serve_files(served, None)
    }

    /// A bundle at `bundle` of `version`, whose two programs hold `build`.
    fn make_bundle(bundle: &Path, version: &str, build: &str) {
        let programs = bundle.join("Contents").join("MacOS");
        std::fs::create_dir_all(&programs).unwrap();
        for exe in [GUI_EXE, CLI_EXE] {
            std::fs::write(programs.join(exe.trim_end_matches(".exe")), build).unwrap();
        }
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>CFBundleVersion</key>\n\t<string>{version}</string>\n\t<key>CFBundleShortVersionString</key>\n\t<string>{version}</string>\n</dict>\n</plist>\n"
        );
        std::fs::write(bundle.join("Contents").join("Info.plist"), plist).unwrap();
    }

    /// A release's `.app.tar.gz` named `name` in `dir`, of a new [`BUNDLE`] of `version`, with the
    /// entries `extra` too (relative to the folder the bundle is in).
    fn bundle_archive(dir: &Path, name: &str, version: &str, extra: &[&str]) -> Vec<u8> {
        let src = dir.join(format!("{name}-src"));
        make_bundle(&src.join(BUNDLE), version, "new");
        let archive = dir.join(name);
        let status = std::process::Command::new(system_tar()).arg("-czf").arg(&archive).arg("-C").arg(&src).arg(BUNDLE).args(extra).status().unwrap();
        assert!(status.success());
        std::fs::read(archive).unwrap()
    }

    #[test]
    fn the_bundle_and_its_release_file_are_found() {
        let app = Path::new("/Applications/Endo's Unified Downloader.app");
        assert_eq!(bundle_of(&app.join("Contents").join("MacOS").join("Endos-Unified-Downloader")), Some(app));
        let renamed = Path::new("/Users/u/Apps/Renamed.app");
        assert_eq!(bundle_of(&renamed.join("Contents/MacOS/Endos-Unified-Downloader-CLI")), Some(renamed));
        for loose in [
            "/usr/local/bin/Endos-Unified-Downloader-CLI",
            "/x/Endo.app/Contents/Resources/x",
            "/x/Endo/Contents/MacOS/x",
            "/x/Endo.app/MacOS/x",
            "/Contents/MacOS/x",
            "x",
            "",
        ] {
            assert_eq!(bundle_of(Path::new(loose)), None, "{loose}");
        }
        assert_eq!(bundle_asset(true), "Endos-Unified-Downloader-macos-arm64.app.tar.gz");
        assert_eq!(bundle_asset(false), "Endos-Unified-Downloader-macos-x64.app.tar.gz");
        assert!(translocated(Path::new("/private/var/folders/ab/T/AppTranslocation/1F2E-77/d/Endo's Unified Downloader.app")));
        assert!(!translocated(app));
    }

    #[test]
    fn the_version_is_read_from_the_info_plist() {
        let dir = tempfile::tempdir().unwrap();
        make_bundle(dir.path(), "1.4.0", "x");
        assert_eq!(bundle_version(dir.path()).as_deref(), Some("1.4.0"));
        let key = "<key>CFBundleShortVersionString</key>";
        assert_eq!(plist_version(&format!("{key}<string> 2.0.1 </string>")).as_deref(), Some("2.0.1"));
        for bad in [String::new(), format!("{key}<integer>1</integer>"), format!("{key}\n<string>1.4</string>"), format!("{key}<string>1.4.0")] {
            assert_eq!(plist_version(&bad), None, "{bad}");
        }
    }

    /// A swap that cannot move the new bundle in puts the old one back.
    #[test]
    fn a_failed_bundle_swap_puts_the_old_one_back() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join(BUNDLE);
        make_bundle(&bundle, "1.3.0", "old");
        let err = swap_bundle(&bundle, &dir.path().join("missing.app")).unwrap_err();
        assert!(err.contains("Failed to replace"), "{err}");
        assert_eq!(names_of(dir.path()), [BUNDLE]);
        assert_eq!(bundle_version(&bundle).as_deref(), Some("1.3.0"));
    }

    /// Only what updates left beside the bundle goes, and only while the bundle is there.
    #[test]
    fn cleanup_deletes_the_bundles_updates_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path();
        let bundle = apps.join(BUNDLE);
        let leftovers = [format!(".{BUNDLE}.old-1"), ".endo-update-2.tmp".to_string()];
        for folder in leftovers.iter().map(String::as_str).chain([".endo-update-keep", "Other.app", ".Other.app.old-3"]) {
            std::fs::create_dir(apps.join(folder)).unwrap();
        }
        cleanup_beside(&bundle);
        assert_eq!(names_of(apps).len(), 5, "nothing goes while the bundle is missing");
        std::fs::create_dir(&bundle).unwrap();
        cleanup_beside(&bundle);
        assert_eq!(names_of(apps), [".Other.app.old-3", ".endo-update-keep", BUNDLE, "Other.app"]);
    }

    /// The bundle's extension is copied to the data folder when that has none or an older one.
    #[test]
    fn the_bundles_extension_is_copied_when_newer() {
        let dir = tempfile::tempdir().unwrap();
        let (from, data) = (dir.path().join("Resources").join("extension"), dir.path().join("data"));
        let to = data.join("extension");
        std::fs::create_dir_all(from.join("lib")).unwrap();
        std::fs::write(from.join("manifest.json"), r#"{"version":"1.4.0"}"#).unwrap();
        std::fs::write(from.join("lib").join("detect.js"), "new").unwrap();
        assert!(copy_extension(&from, &to).unwrap(), "none yet");
        assert_eq!(names_of(&to), ["lib", "manifest.json"]);
        assert_eq!(std::fs::read_to_string(to.join("lib").join("detect.js")).unwrap(), "new");
        assert!(!copy_extension(&from, &to).unwrap(), "the same version stays");

        std::fs::write(to.join("manifest.json"), r#"{"version":"1.3.0"}"#).unwrap();
        std::fs::write(to.join("gone.js"), "old").unwrap();
        assert!(copy_extension(&from, &to).unwrap(), "an older one is replaced");
        assert_eq!(names_of(&to), ["lib", "manifest.json"]);
        assert_eq!(names_of(&data), ["extension"]);
        std::fs::write(to.join("manifest.json"), r#"{"version":"1.5.0"}"#).unwrap();
        assert!(!copy_extension(&from, &to).unwrap(), "a newer one stays");
        assert!(!copy_extension(&dir.path().join("none"), &to).unwrap(), "a bundle without one changes nothing");
        assert_eq!(manifest_version(&to).as_deref(), Some("1.5.0"));
    }

    /// The macOS update against a release served from here: each way a release can be wrong
    /// changes nothing beside the bundle, a right one swaps the whole bundle in, the old one kept
    /// beside it until cleanup.
    #[tokio::test]
    async fn install_swaps_in_only_a_signed_matching_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("Applications");
        let bundle = apps.join(BUNDLE);
        make_bundle(&bundle, "1.3.0", "old");
        let exe = bundle.join("Contents").join("MacOS").join(CLI_EXE.trim_end_matches(".exe"));
        std::fs::write(dir.path().join("escape.txt"), "x").unwrap();
        let asset = bundle_asset(false);
        let good = bundle_archive(dir.path(), "good.tar.gz", "1.4.0", &[]);
        let stale = bundle_archive(dir.path(), "stale.tar.gz", "1.3.5", &[]);
        let evil = bundle_archive(dir.path(), "evil.tar.gz", "1.4.0", &["../escape.txt"]);
        let ((pair, key), (other_pair, _)) = (test_key(), test_key());
        let client = reqwest::Client::new();
        let update = Update { version: "1.4.0".into(), tag: "v1.4.0".into(), page: "https://example.com/tag/v1.4.0".into() };
        let install = |releases: String, current: &'static str, exe: PathBuf| {
            let (client, key, update) = (&client, &key, &update);
            async move { install_bundle_from(client, &releases, key, current, update, &exe, false).await }
        };
        let unchanged = || {
            assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
            assert_eq!(bundle_version(&bundle).as_deref(), Some("1.3.0"));
            assert_eq!(names_of(&apps), [BUNDLE]);
        };
        let files: Vec<(&str, &[u8])> = vec![(asset, &good[..])];

        let err = install(serve_release(&files, &files, &other_pair).await, "1.3.0", exe.clone()).await.unwrap_err();
        assert!(err.contains("signature does not match"), "{err}");
        unchanged();
        let err = install(serve_release(&[(asset, &stale[..])], &files, &pair).await, "1.3.0", exe.clone()).await.unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        unchanged();
        let err = install(serve_release(&files, &[], &pair).await, "1.3.0", exe.clone()).await.unwrap_err();
        assert!(err.contains(&format!("no entry for {asset}")), "{err}");
        unchanged();
        for (archive, why) in [(&stale, "version 1.3.5, not 1.4.0"), (&evil, "outside its folder: ../escape.txt")] {
            let served: Vec<(&str, &[u8])> = vec![(asset, &archive[..])];
            let err = install(serve_release(&served, &served, &pair).await, "1.3.0", exe.clone()).await.unwrap_err();
            assert!(err.contains(why), "{err}");
            unchanged();
        }
        let err = install(serve_release(&files, &files, &pair).await, "1.4.0", exe.clone()).await.unwrap_err();
        assert!(err.contains("not newer"), "{err}");
        // The GUI installed a newer release while this CLI, still 1.3.0, ran.
        make_bundle(&bundle, "1.5.0", "old");
        let err = install(serve_release(&files, &files, &pair).await, "1.3.0", exe.clone()).await.unwrap_err();
        assert!(err.contains("1.5.0 is already installed"), "{err}");
        make_bundle(&bundle, "1.3.0", "old");
        // A build run from outside a bundle.
        let loose = dir.path().join("Endos-Unified-Downloader-CLI");
        let err = install(serve_release(&files, &files, &pair).await, "1.3.0", loose).await.unwrap_err();
        assert!(err.contains("not in its .app bundle") && err.contains(&update.page), "{err}");
        unchanged();

        let installed = install(serve_release(&files, &files, &pair).await, "1.3.0", exe.clone()).await.unwrap();
        assert_eq!((installed.replaced, &installed.own, installed.extension), (vec![bundle.clone()], &exe, false));
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(bundle_version(&bundle).as_deref(), Some("1.4.0"));
        let names = names_of(&apps);
        assert!(names.len() == 2 && names[0].starts_with(&format!(".{BUNDLE}.old-")) && names[1] == BUNDLE, "{names:?}");
        let old = apps.join(&names[0]).join("Contents").join("MacOS").join(exe.file_name().unwrap());
        assert_eq!(std::fs::read_to_string(old).unwrap(), "old");
        cleanup_beside(&bundle);
        assert_eq!(names_of(&apps), [BUNDLE]);
    }

    /// On macOS the app's data, and the browser extension's folder with it, are in Application Support.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_keeps_the_extension_with_the_data_in_application_support() {
        let data = crate::media::app_data_dir().unwrap();
        assert!(data.ends_with("Library/Application Support/EndosUnifiedDownloader"), "{}", data.display());
        assert_eq!(extension_dir(), Some(data.join("extension")));
    }

    /// A bundle that holds a link is refused, as one could lead outside it.
    #[cfg(unix)]
    #[test]
    fn a_bundle_with_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        make_bundle(&src.join(BUNDLE), "1.4.0", "new");
        std::os::unix::fs::symlink("/etc", src.join(BUNDLE).join("Contents").join("Resources")).unwrap();
        let archive = dir.path().join("linked.tar.gz");
        let status = std::process::Command::new(system_tar()).arg("-czf").arg(&archive).arg("-C").arg(&src).arg(BUNDLE).status().unwrap();
        assert!(status.success());
        let (apps, own) = (dir.path().join("apps"), std::ffi::OsStr::new("Endos-Unified-Downloader"));
        let (bundle, staging) = (apps.join(BUNDLE), apps.join(".endo-update-1.tmp"));
        make_bundle(&bundle, "1.3.0", "old");
        std::fs::create_dir(&staging).unwrap();
        let err = put_bundle_in_place(&bundle, &staging, &std::fs::read(&archive).unwrap(), "1.4.0", own).unwrap_err();
        assert!(err.contains("holds links"), "{err}");
        assert_eq!(bundle_version(&bundle).as_deref(), Some("1.3.0"));
    }

    #[test]
    fn newer_compares_the_numbers() {
        assert!(newer("1.4.0", "1.3.0"));
        assert!(newer("1.10.0", "1.9.9"));
        assert!(newer("2.0.0", "1.99.99"));
        assert!(newer("1.3.0.1", "1.3.0"));
        assert!(!newer("1.3.0", "1.3.0"));
        assert!(!newer("1.2.9", "1.3.0"));
        for unparsable in ["1.4", "v1.4.0", "1.4.0-beta", "", "1..4.0", "1.4.x"] {
            assert!(!newer(unparsable, "1.3.0"), "{unparsable}");
            assert!(!newer("1.3.0", unparsable), "{unparsable}");
        }
    }

    #[test]
    fn only_a_newer_three_part_release_is_offered() {
        let releases = "https://example.com/o/r/releases";
        let update = newer_release(releases, &format!("{releases}/tag/v1.4.0"), "1.3.0").unwrap().unwrap();
        assert_eq!(update, Update { version: "1.4.0".into(), tag: "v1.4.0".into(), page: format!("{releases}/tag/v1.4.0") });
        assert_eq!(newer_release(releases, &format!("{releases}/tag/v1.3.0"), "1.3.0"), Ok(None));
        assert_eq!(newer_release(releases, &format!("{releases}/tag/v1.2.0"), "1.3.0"), Ok(None));
        for latest in [
            format!("{releases}/tag/1.4.0"),
            format!("{releases}/tag/v1.4"),
            format!("{releases}/tag/v1.4.0.1"),
            format!("{releases}/tag/v1.4.0/x"),
            format!("{releases}/latest"),
            "https://example.com/other/releases/tag/v9.0.0".to_string(),
        ] {
            assert!(newer_release(releases, &latest, "1.3.0").is_err(), "{latest}");
        }
    }

    #[test]
    fn only_the_keys_signature_of_exactly_these_sums_verifies() {
        let ((pair, key), (_, other_key)) = (test_key(), test_key());
        let sums = b"# version 1.4.0\n".as_slice();
        let signature = sign(&pair, sums);
        assert_eq!(verify_signature(&key, sums, &signature), Ok(()));
        let spaced = format!("  {}  \r\n", String::from_utf8_lossy(&signature).trim());
        assert_eq!(verify_signature(&key, sums, spaced.as_bytes()), Ok(()));

        assert!(verify_signature(&other_key, sums, &signature).unwrap_err().contains("does not match"));
        assert!(verify_signature(&key, b"# version 1.4.1\n", &signature).is_err());
        assert!(verify_signature(&key, b"# version 1.4.0\r\n", &signature).is_err());
        assert!(verify_signature(&key, sums, b"not base64!\n").unwrap_err().contains("malformed"));
        assert!(verify_signature(&key, sums, b"").is_err());
        let base64 = base64::engine::general_purpose::STANDARD;
        let short = base64.encode(&pair.sign(sums).as_ref()[..63]);
        assert!(verify_signature(&key, sums, short.as_bytes()).is_err());
        let unpadded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(pair.sign(sums));
        assert!(verify_signature(&key, sums, unpadded.as_bytes()).is_err());
        assert!(verify_signature(&base64.encode([7u8; 31]), sums, &signature).is_err());
    }

    #[test]
    fn sums_are_read_strictly() {
        let a = "a".repeat(64);
        let good = format!("# version 1.4.0\n{a}  {CLI_EXE}\n{}  {GUI_EXE}\n", "0123456789abcdef".repeat(4));
        let sums = parse_sums(good.as_bytes(), "1.4.0").unwrap();
        assert_eq!(sums.get(CLI_EXE), Some(&a.as_str()));
        assert_eq!(sums.len(), 2);
        assert!(parse_sums(good.as_bytes(), "1.5.0").unwrap_err().contains("for version 1.4.0, not 1.5.0"));
        assert_eq!(parse_sums(b"# version 1.4.0\n", "1.4.0"), Ok(HashMap::new()));
        for bad in [
            format!("{a}  {GUI_EXE}\n"),
            format!("# version 1.4.0\n{a}  {GUI_EXE}"),
            format!("# version 1.4.0\r\n{a}  {GUI_EXE}\r\n"),
            format!("# version 1.4.0\n{a}  {GUI_EXE}\r\n"),
            format!("# version 1.4.0\n{}  {GUI_EXE}\n", "A".repeat(64)),
            format!("# version 1.4.0\n{}  {GUI_EXE}\n", "a".repeat(63)),
            format!("# version 1.4.0\n{a} {GUI_EXE}\n"),
            format!("# version 1.4.0\n{a}   {GUI_EXE}\n"),
            format!("# version 1.4.0\n{a}  *{GUI_EXE}\n"),
            format!("# version 1.4.0\n{a}  ../{GUI_EXE}\n"),
            format!("# version 1.4.0\n{a}  \n"),
            format!("# version 1.4.0\n{a}  {GUI_EXE}\n\n"),
            format!("# version 1.4.0\n{a}  {GUI_EXE}\n{a}  {GUI_EXE}\n"),
            "# version 1.4.0\n# comment\n".to_string(),
            "\n".to_string(),
            String::new(),
        ] {
            assert!(parse_sums(bad.as_bytes(), "1.4.0").is_err(), "{bad:?}");
        }
        assert!(parse_sums(b"# version 1.4.0\n\xff  x\n", "1.4.0").is_err());
    }

    #[test]
    fn zip_entries_outside_the_folder_are_caught() {
        for fine in ["manifest.json", "lib/detect.js", "icons/", "..icon.png", "a..b/c.js"] {
            assert!(!escapes(fine), "{fine}");
        }
        for bad in ["/etc/passwd", "../x", "lib/../../x", "..", "lib/..", "C:/Windows/x", "C:x", "lib\\..\\..\\x", "\\x", "manifest.json:stream"] {
            assert!(escapes(bad), "{bad}");
        }
    }

    /// The second rename of the second program (its new build into place) fails: the first
    /// program, already swapped, and the second are both put back.
    #[test]
    fn a_failed_swap_puts_the_old_programs_back() {
        let dir = tempfile::tempdir().unwrap();
        let (gui, cli) = (dir.path().join(GUI_EXE), dir.path().join(CLI_EXE));
        std::fs::write(&gui, "old gui").unwrap();
        std::fs::write(&cli, "old cli").unwrap();
        std::fs::write(new_name(&gui), "new gui").unwrap();
        let err = swap(&[gui.clone(), cli.clone()]).unwrap_err();
        assert!(err.contains(CLI_EXE), "{err}");
        assert_eq!(std::fs::read_to_string(&gui).unwrap(), "old gui");
        assert_eq!(std::fs::read_to_string(&cli).unwrap(), "old cli");
        assert_eq!(names_of(dir.path()), [CLI_EXE, GUI_EXE]);
    }

    /// An update cut off mid-swap left the GUI only as its old and new builds and the extension
    /// only moved aside, and one whose putting back failed left the CLI only as its old build:
    /// cleanup finishes the GUI's swap, puts the extension back and deletes nothing of the CLI.
    #[test]
    fn cleanup_puts_back_what_a_cut_off_update_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path();
        std::fs::write(app.join("Endos-Unified-Downloader.old.exe"), "old gui").unwrap();
        std::fs::write(app.join("Endos-Unified-Downloader.exe.new"), "new gui").unwrap();
        std::fs::write(app.join("Endos-Unified-Downloader-CLI.old.exe"), "old cli").unwrap();
        for folder in [".extension-old-1", ".extension-old-2", ".extension-3.tmp"] {
            std::fs::create_dir(app.join(folder)).unwrap();
            std::fs::write(app.join(folder).join("manifest.json"), folder).unwrap();
        }
        cleanup_in(app);
        assert_eq!(names_of(app), ["Endos-Unified-Downloader-CLI.old.exe", GUI_EXE, "extension"]);
        assert_eq!(std::fs::read_to_string(app.join(GUI_EXE)).unwrap(), "new gui");
        assert_eq!(std::fs::read_to_string(app.join("extension").join("manifest.json")).unwrap(), ".extension-old-2");
    }

    /// The whole update against a release served from here: each way a release can be wrong
    /// changes nothing, a right one replaces both programs and the extension, and cleanup then
    /// deletes what the update left and nothing else.
    #[cfg(windows)]
    #[tokio::test]
    async fn install_swaps_in_only_a_signed_matching_release() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let (gui, cli, ext) = (app.join(GUI_EXE), app.join(CLI_EXE), app.join("extension"));
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(&gui, "old gui").unwrap();
        std::fs::write(&cli, "old cli").unwrap();
        std::fs::write(ext.join("manifest.json"), r#"{"version":"1.3.0"}"#).unwrap();
        std::fs::write(ext.join("gone.js"), "old").unwrap();
        // What an earlier update left, replaced by this one.
        std::fs::write(app.join("Endos-Unified-Downloader.old.exe"), "older gui").unwrap();

        // The extension's zips: its files at the root, and the same with one outside its folder.
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("manifest.json"), r#"{"version":"1.4.0"}"#).unwrap();
        std::fs::write(src.join("background.js"), "new").unwrap();
        let zip = |name: &str, entries: &[&str]| {
            let path = dir.path().join(name);
            let status = std::process::Command::new(system_tar()).arg("-a").arg("-cf").arg(&path).arg("-C").arg(&src).args(entries).status().unwrap();
            assert!(status.success());
            std::fs::read(path).unwrap()
        };
        let good_zip = zip("good.zip", &["manifest.json", "background.js"]);
        let evil_zip = zip("evil.zip", &["manifest.json", "../src/background.js"]);

        let ((pair, key), (other_pair, _)) = (test_key(), test_key());
        let ext_asset = "Endos-Unified-Downloader-Extension-v1.4.0.zip";
        let files: Vec<(&str, &[u8])> = vec![(GUI_EXE, &b"new gui"[..]), (CLI_EXE, &b"new cli"[..]), (ext_asset, &good_zip[..])];
        let serve = serve_release;
        let client = reqwest::Client::new();
        let update = Update { version: "1.4.0".into(), tag: "v1.4.0".into(), page: String::new() };
        let install = |releases: String, current: &'static str| {
            let (client, key, update, gui) = (&client, &key, &update, &gui);
            async move { install_from(client, &releases, key, current, update, Program::Gui, gui).await }
        };
        let unchanged = || {
            assert_eq!(std::fs::read_to_string(&gui).unwrap(), "old gui");
            assert_eq!(std::fs::read_to_string(&cli).unwrap(), "old cli");
            assert_eq!(extension_version(&app).as_deref(), Some("1.3.0"));
            assert!(ext.join("gone.js").is_file());
            assert_eq!(names_of(&app), [CLI_EXE, GUI_EXE, "Endos-Unified-Downloader.old.exe", "extension"]);
        };

        let err = install(serve(&files, &files, &other_pair).await, "1.3.0").await.unwrap_err();
        assert!(err.contains("signature does not match"), "{err}");
        unchanged();
        let tampered: Vec<(&str, &[u8])> = vec![(GUI_EXE, &b"evil gui"[..]), (CLI_EXE, &b"new cli"[..]), (ext_asset, &good_zip[..])];
        let err = install(serve(&tampered, &files, &pair).await, "1.3.0").await.unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        unchanged();
        let err = install(serve(&files, &[files[0], files[2]], &pair).await, "1.3.0").await.unwrap_err();
        assert!(err.contains(&format!("no entry for {CLI_EXE}")), "{err}");
        unchanged();
        let evil: Vec<(&str, &[u8])> = vec![(GUI_EXE, &b"new gui"[..]), (CLI_EXE, &b"new cli"[..]), (ext_asset, &evil_zip[..])];
        let err = install(serve(&evil, &evil, &pair).await, "1.3.0").await.unwrap_err();
        assert!(err.contains("outside its folder: ../src/background.js"), "{err}");
        unchanged();
        let err = install(serve(&files, &files, &pair).await, "1.4.0").await.unwrap_err();
        assert!(err.contains("not newer"), "{err}");
        unchanged();
        // The CLI installed a newer release while this GUI, still 1.3.0, ran.
        std::fs::write(ext.join("manifest.json"), r#"{"version":"1.5.0"}"#).unwrap();
        let err = install(serve(&files, &files, &pair).await, "1.3.0").await.unwrap_err();
        assert!(err.contains("1.5.0 is already installed"), "{err}");
        std::fs::write(ext.join("manifest.json"), r#"{"version":"1.3.0"}"#).unwrap();
        unchanged();

        // The CLI an earlier update replaced still runs: this one moves the CLI to another name.
        std::fs::write(app.join("Endos-Unified-Downloader-CLI.old.exe"), "older cli").unwrap();
        let running = std::fs::OpenOptions::new().read(true).share_mode(0).open(app.join("Endos-Unified-Downloader-CLI.old.exe")).unwrap();
        let installed = install(serve(&files, &files, &pair).await, "1.3.0").await.unwrap();
        assert_eq!(installed.replaced, [gui.clone(), cli.clone()]);
        assert_eq!(installed.own, gui);
        assert!(installed.extension);
        assert_eq!(std::fs::read_to_string(&gui).unwrap(), "new gui");
        assert_eq!(std::fs::read_to_string(&cli).unwrap(), "new cli");
        assert_eq!(std::fs::read_to_string(app.join("Endos-Unified-Downloader.old.exe")).unwrap(), "old gui");
        assert_eq!(extension_version(&app).as_deref(), Some("1.4.0"));
        assert_eq!(names_of(&ext), ["background.js", "manifest.json"]);
        let names = names_of(&app);
        let moved: Vec<&String> = names.iter().filter(|name| name.starts_with("Endos-Unified-Downloader-CLI.old-")).collect();
        assert_eq!(moved.len(), 1, "{names:?}");
        assert_eq!(std::fs::read_to_string(app.join(moved[0])).unwrap(), "old cli");
        assert_eq!(names.len(), 6, "{names:?}");

        // What cleanup deletes, next to what it keeps.
        drop(running);
        for leftover in [".extension-old-1", ".extension-2.tmp", ".extension-keep"] {
            std::fs::create_dir(app.join(leftover)).unwrap();
        }
        for leftover in ["Endos-Unified-Downloader.exe.new", "Endos-Unified-Downloader-CLI.exe.new", "Other.old.exe", "Endos-Unified-Downloader.old.txt", "notes.txt"] {
            std::fs::write(app.join(leftover), "x").unwrap();
        }
        cleanup_in(&app);
        assert_eq!(
            names_of(&app),
            [".extension-keep", CLI_EXE, GUI_EXE, "Endos-Unified-Downloader.old.txt", "Other.old.exe", "extension", "notes.txt"]
        );
    }
}
