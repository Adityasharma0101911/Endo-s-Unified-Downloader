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
    /// Every exe replaced, own first.
    pub replaced: Vec<PathBuf>,
    /// Where the new build of the running program now is (its path before the update): what to restart.
    pub own: PathBuf,
    /// The extension folder next to the exes was replaced.
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

/// Downloads, verifies and swaps in `update` (Windows only; elsewhere Err naming update.page): the
/// running program `own`, the other program when it is next to it, and the browser extension's
/// folder there. Nothing is replaced unless every file is verified and written next to its target,
/// and a failure while the programs are swapped puts the old ones back. The old programs stay as
/// `<name>.old.exe` until [`cleanup_old`], as a running one cannot be deleted.
pub async fn install(proxy: Option<&str>, update: &Update, own: Program) -> Result<Installed, String> {
    if !cfg!(windows) {
        return Err(format!("Updating in place works on Windows only; download the new version from {}", update.page));
    }
    let exe = std::env::current_exe().map_err(|e| format!("Cannot find the running program: {e}"))?;
    let key = debug_override("ENDO_UPDATE_KEY").unwrap_or_else(|| PUBLIC_KEY.to_string());
    install_from(&http_client(proxy)?, &releases(), &key, env!("CARGO_PKG_VERSION"), update, own, &exe).await
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

    let release = format!("{releases}/download/{}", update.tag);
    let listing = fetch(client, &format!("{release}/SHA256SUMS"), SUMS_CAP, Duration::from_secs(30)).await?;
    let signature = fetch(client, &format!("{release}/SHA256SUMS.sig"), SUMS_CAP, Duration::from_secs(30)).await?;
    // Nothing of an unsigned listing is read.
    verify_signature(key, &listing, &signature)?;
    let sums = parse_sums(&listing, &update.version)?;
    let mut files = Vec::new();
    for asset in targets.iter().map(|(_, asset)| *asset).chain(extension.as_deref()) {
        let expected = sums.get(asset).ok_or_else(|| format!("The update's SHA256SUMS has no entry for {asset}"))?;
        let body = fetch(client, &format!("{release}/{asset}"), ASSET_CAP, Duration::from_secs(600)).await?;
        let actual = format!("{:x}", Sha256::digest(&body));
        if actual != *expected {
            return Err(format!("Downloaded {asset} failed checksum verification (expected {expected}, got {actual})"));
        }
        files.push(body);
    }

    let zip = if extension.is_some() { files.pop() } else { None };
    let (dir, exes): (_, Vec<PathBuf>) = (dir.to_path_buf(), targets.into_iter().map(|(path, _)| path).collect());
    tokio::task::spawn_blocking(move || put_in_place(&dir, exes, files, zip))
        .await
        .map_err(|e| format!("Update task failed: {e}"))?
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
        .and_then(|()| {
            let listed = quiet_command(&tar).arg("-tf").arg(&archive).stdin(Stdio::null()).output();
            match listed {
                Ok(out) if out.status.success() => match String::from_utf8_lossy(&out.stdout).lines().find(|entry| escapes(entry)) {
                    Some(entry) => Err(format!("The update's browser extension has a file outside its folder: {entry}")),
                    None => Ok(()),
                },
                Ok(out) => Err(format!("Failed to list the browser extension: {}", String::from_utf8_lossy(&out.stderr).trim())),
                Err(e) => Err(format!("Failed to run {}: {e}", tar.display())),
            }
        })
        .and_then(|()| {
            std::fs::create_dir(into)
                .and_then(|()| unpack(&tar, &archive, into, &[]))
                .map_err(|e| format!("Failed to unpack the browser extension: {e}"))
        });
    let _ = std::fs::remove_dir_all(&staging);
    unpacked?;
    manifest_version(into).map(drop).ok_or_else(|| "The update's browser extension has no manifest.json with a version".to_string())
}

/// Whether zip entry `name` would land outside the folder it is unpacked into: absolute, on a
/// drive, through `..`, or with a backslash, which Windows takes for a folder separator.
fn escapes(name: &str) -> bool {
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
/// off midway had moved aside is put back first.
pub fn cleanup_old() {
    if let Some(dir) = app_dir() {
        cleanup_in(&dir);
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

/// `version` of `folder/manifest.json` when it parses as a version. Blocking.
fn manifest_version(folder: &Path) -> Option<String> {
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(folder.join("manifest.json")).ok()?).ok()?;
    let version = manifest["version"].as_str()?;
    version_parts(version).map(|_| version.to_string())
}

/// The running exe's folder.
pub fn app_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::tests::names_of;
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
        use crate::media::tests::serve_files;
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
        // Serves `files` with sums of `summed`, signed by `signer`.
        let serve = |files: &[(&str, &[u8])], summed: &[(&str, &[u8])], signer: &Ed25519KeyPair| {
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
        };
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
