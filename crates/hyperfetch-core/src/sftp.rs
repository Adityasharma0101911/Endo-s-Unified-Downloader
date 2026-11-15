//! SFTP downloads: `sftp://user[:pass]@host[:port]/path` links over SSH, in pure Rust (russh and
//! russh-sftp). The path is from the server's root; `/~/` starts it at the login's home folder.
//!
//! A file downloads over ranges as an FTP one does (see `crate::ftp`): the same chunks, `.part`,
//! resume state, retries and speed limit, with [`work`] in place of the HTTP worker. Each worker
//! keeps one SSH connection for its ranges and reads each at its offset. A folder link adds one
//! download per file under it, subfolders kept (see [`list`]).
//!
//! Sign-in is by the link's password or by a private key file (see [`SshKey`]), nothing else: no
//! agent, no forwarding, no shell. A server's host key is trusted the first time it is seen and
//! kept in the known-hosts file (see [`keep_known_hosts_in`]); any other key for that server is
//! refused every time, until the user removes the kept one from that file.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use percent_encoding::percent_decode_str;
use russh::client::Handle;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use tokio::io::AsyncSeekExt;
use url::{Host, Url};

use crate::chunk::Chunk;
use crate::engine::DownloadOptions;
use crate::ftp::{self, transient, Dialer, Failure};
use crate::hosts::HostSlot;
use crate::ingest::{ListOptions, Task};
use crate::worker::{FailureKind, WorkerEvent, WorkerShared};

/// Tries of a probe while the connection fails.
const PROBE_ATTEMPTS: u32 = 3;
/// Most folders a folder link reads, and its deepest subfolder.
const MAX_FOLDERS: usize = 10_000;
const MAX_DEPTH: usize = 64;
/// Where the passphrase of an encrypted private key is given.
const PASSPHRASE_WHERE: &str = "in the app, Settings > Debrid & hosts > SSH key passphrase; in the CLI, --ssh-key-passphrase";

/// A private key file SFTP servers are signed in to with, and its passphrase when it is
/// encrypted. Never printed with its passphrase, nor saved with the queue.
#[derive(Clone, Default, PartialEq)]
pub struct SshKey {
    pub path: PathBuf,
    pub passphrase: Option<String>,
}

impl std::fmt::Debug for SshKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshKey").field("path", &self.path).field("passphrase", &self.passphrase.as_ref().map(|_| "…")).finish()
    }
}

impl SshKey {
    /// The key, opened with its passphrase.
    fn open(&self) -> Result<Arc<PrivateKey>, String> {
        let shown = self.path.display();
        match russh::keys::load_secret_key(&self.path, self.passphrase.as_deref()) {
            Ok(key) => Ok(Arc::new(key)),
            Err(russh::keys::Error::KeyIsEncrypted) => Err(format!("the private key {shown} is encrypted: give its passphrase ({PASSPHRASE_WHERE})")),
            Err(russh::keys::Error::IO(e)) => Err(format!("cannot read the private key {shown}: {e}")),
            Err(_) if self.passphrase.is_some() => Err(format!("cannot open the private key {shown}: is its passphrase right?")),
            Err(e) => Err(format!("cannot open the private key {shown}: {e}")),
        }
    }
}

/// Whether the private key file at `path` is encrypted, so a front end asks for its passphrase.
pub fn needs_passphrase(path: &Path) -> bool {
    matches!(russh::keys::load_secret_key(path, None), Err(russh::keys::Error::KeyIsEncrypted))
}

static KNOWN_HOSTS: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

/// Keeps SFTP servers' host keys in `file`, a known_hosts file in OpenSSH's format in the app's
/// data folder: front ends call it at start.
pub fn keep_known_hosts_in(file: PathBuf) {
    *KNOWN_HOSTS.lock() = Some(file);
}

fn known_hosts() -> Option<PathBuf> {
    if let Some(file) = KNOWN_HOSTS.lock().clone() {
        return Some(file);
    }
    // Swept with the unit tests' history files (see `history::default_history_path`).
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!("hf-unit-history-{}.known_hosts", std::process::id())));
    }
    crate::media::app_data_dir().map(|dir| dir.join("known_hosts"))
}

/// Whether `key` may be the key of `host` at `port`: the one kept for it in `file`, or the first
/// one it showed, which is kept now. Any other is refused, whatever its kind: it may be another
/// server pretending to be that one.
fn check_host(host: &str, port: u16, key: &PublicKey, file: &Path) -> Result<(), String> {
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let unreadable = |e: russh::keys::Error| format!("cannot read the known hosts file {}: {e}", file.display());
    let kept = russh::keys::known_hosts::known_host_keys_path(host, port, file).map_err(unreadable)?;
    if kept.iter().any(|(_, known)| known.key_data() == key.key_data()) {
        return Ok(());
    }
    let fingerprint = key.fingerprint(HashAlg::Sha256);
    if let Some((line, _)) = kept.first() {
        return Err(format!(
            "REFUSED: the SSH host key of {host} port {port} is not the one kept for it (it shows {fingerprint}): another server may be \
             pretending to be it. If its key was changed on purpose, delete line {line} of {} and try again",
            file.display()
        ));
    }
    russh::keys::known_hosts::learn_known_hosts_path(host, port, key, file).map_err(|e| format!("cannot keep the host key in {}: {e}", file.display()))?;
    tracing::warn!("Trusting the SSH host key of {host} port {port} on first use: {fingerprint}, kept in {}", file.display());
    Ok(())
}

/// The client side of one SSH connection: checks the server's host key.
struct Client {
    host: String,
    port: u16,
    known_hosts: Option<PathBuf>,
}

#[derive(Debug)]
enum SshError {
    Ssh(russh::Error),
    /// The host key was refused, or could not be checked.
    Host(String),
}

impl From<russh::Error> for SshError {
    fn from(e: russh::Error) -> Self {
        Self::Ssh(e)
    }
}

impl russh::client::Handler for Client {
    type Error = SshError;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::new(cert.public_key().clone(), ""),
        };
        let file = self.known_hosts.as_deref().ok_or_else(|| SshError::Host("there is no data folder to keep SSH host keys in".to_string()))?;
        check_host(&self.host, self.port, &key, file).map_err(SshError::Host)?;
        Ok(true)
    }
}

fn ssh_failure(e: SshError) -> Failure {
    match e {
        SshError::Host(why) => (FailureKind::Fatal, why),
        SshError::Ssh(e) => transient(format!("SSH connection failed: {e}")),
    }
}

/// What an SFTP error means for the download: a missing file or a refusal ends it.
fn sftp_failure(e: russh_sftp::client::error::Error) -> Failure {
    use russh_sftp::protocol::StatusCode;
    match &e {
        russh_sftp::client::error::Error::Status(status) if matches!(status.status_code, StatusCode::NoSuchFile | StatusCode::PermissionDenied) => {
            (FailureKind::Denied, format!("SFTP: {e}"))
        }
        _ => transient(format!("SFTP: {e}")),
    }
}

/// An SSH connection signed in, with its SFTP session.
struct Conn {
    /// The `host:port` it is to.
    server: String,
    _ssh: Handle<Client>,
    sftp: SftpSession,
}

/// How a download's SFTP connections are made, with the logins its links carried and the key it
/// signs in with. Never printed: it holds passwords.
pub(crate) struct Net {
    dialer: Dialer,
    /// Longest wait for an answer.
    reply_timeout: Duration,
    config: Arc<russh::client::Config>,
    /// User and password (if any) by server (`host:port`), taken out of the links.
    logins: Vec<(String, (String, Option<String>))>,
    key: Option<Arc<PrivateKey>>,
    known_hosts: Option<PathBuf>,
}

impl Net {
    /// The connections `options` ask for, and the download's sftp:// links without their
    /// `user:pass@`, which is kept here instead: no password reaches the resume state, the history
    /// or the log. Links of other schemes are left out.
    pub(crate) fn new(options: &DownloadOptions, urls: &[Url]) -> Result<(Self, Vec<Url>), String> {
        if options.proxy.is_some() || !options.proxy_pool.is_empty() {
            return Err("SFTP downloads cannot go through a proxy: turn the proxy off for this download".to_string());
        }
        let (mut logins, mut links) = (Vec::new(), Vec::new());
        for url in urls {
            if url.scheme() != "sftp" {
                tracing::warn!("Leaving out {}: an SFTP download takes only sftp:// mirrors", crate::history::redact_url(url.as_str()));
                continue;
            }
            let (login, url) = without_login(url);
            logins.extend(login);
            if !links.contains(&url) {
                links.push(url);
            }
        }
        if links.is_empty() {
            return Err("No sftp:// link to download".to_string());
        }
        let key = options.ssh_key.as_ref().map(SshKey::open).transpose()?;
        Ok((Self::with(Dialer::new(options), options.stall_timeout_secs, logins, key), links))
    }

    fn with(dialer: Dialer, timeout_secs: u64, logins: Vec<(String, (String, Option<String>))>, key: Option<Arc<PrivateKey>>) -> Self {
        let config = russh::client::Config { nodelay: true, ..Default::default() };
        Self { dialer, reply_timeout: Duration::from_secs(timeout_secs.max(1)), config: Arc::new(config), logins, key, known_hosts: known_hosts() }
    }

    /// The size of the file at `url` and its date as an HTTP date, each `None` when the server
    /// does not tell. A failed connection is tried again.
    pub(crate) async fn probe(&self, url: &Url) -> Result<(Option<u64>, Option<String>), String> {
        let path = path_of(url)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let found = async {
                let conn = self.connect(url, 0).await?;
                conn.sftp.metadata(path.as_str()).await.map_err(sftp_failure)
            };
            match found.await {
                Ok(attrs) if attrs.file_type().is_dir() => return Err(format!("{}: a folder, not a file: add its link on its own to download what is in it", crate::history::redact_url(url.as_str()))),
                Ok(attrs) => return Ok((attrs.size, attrs.mtime.map(|secs| crate::engine::http_date(secs.into())))),
                Err((FailureKind::Transient, _)) if attempt < PROBE_ATTEMPTS => tokio::time::sleep(crate::chunk::backoff_delay(attempt)).await,
                Err((_, e)) => return Err(format!("{}: {}", crate::history::redact_url(url.as_str()), e)),
            }
        }
    }

    /// An SSH connection to `url`'s server, its host key checked and signed in, with its SFTP
    /// session, for worker `worker`.
    async fn connect(&self, url: &Url, worker: usize) -> Result<Conn, Failure> {
        let port = url.port().unwrap_or(22);
        let stream = self.dialer.dial(url, port, worker).await?;
        let client = Client { host: host_of(url), port, known_hosts: self.known_hosts.clone() };
        let handshake = async {
            let mut ssh = russh::client::connect_stream(Arc::clone(&self.config), stream, client).await.map_err(ssh_failure)?;
            self.sign_in(&mut ssh, url).await?;
            let opened = async {
                let channel = ssh.channel_open_session().await?;
                channel.request_subsystem(true, "sftp").await?;
                Ok::<_, russh::Error>(channel)
            };
            let channel = opened.await.map_err(|e| transient(format!("the SSH server opens no SFTP session: {e}")))?;
            let sftp = SftpSession::new(channel.into_stream()).await.map_err(sftp_failure)?;
            sftp.set_timeout(self.reply_timeout.as_secs());
            Ok(Conn { server: server(url), _ssh: ssh, sftp })
        };
        match tokio::time::timeout(self.reply_timeout, handshake).await {
            Ok(conn) => conn,
            Err(_) => Err(transient(format!("no answer from the SSH server within {}s", self.reply_timeout.as_secs()))),
        }
    }

    /// Signs in as the link's user: with the private key, else (or when it is refused) with the
    /// link's password. A refusal ends the download: trying again would only lock the account.
    async fn sign_in(&self, ssh: &mut Handle<Client>, url: &Url) -> Result<(), Failure> {
        let fatal = |why: String| (FailureKind::Fatal, why);
        let Some((user, password)) = self.logins.iter().find(|(at, _)| *at == server(url)).map(|(_, login)| login) else {
            return Err(fatal("the link names no user to sign in as: sftp://USER@host/path".to_string()));
        };
        let failed = |e: russh::Error| transient(format!("SSH sign-in failed: {e}"));
        if let Some(key) = &self.key {
            let hash = match key.algorithm().is_rsa() {
                true => ssh.best_supported_rsa_hash().await.map_err(failed)?.flatten(),
                false => None,
            };
            if ssh.authenticate_publickey(user.as_str(), PrivateKeyWithHashAlg::new(Arc::clone(key), hash)).await.map_err(failed)?.success() {
                return Ok(());
            }
        }
        if let Some(password) = password {
            if ssh.authenticate_password(user.as_str(), password.as_str()).await.map_err(failed)?.success() {
                return Ok(());
            }
        }
        Err(fatal(match (&self.key, password) {
            (None, None) => format!("no password or private key to sign in as {user}: give the password in the link (sftp://{user}:PASSWORD@host/path) or a private key (in the app, Settings > Debrid & hosts; in the CLI, --ssh-key)"),
            (Some(_), None) => format!("the SSH server refused the private key for {user}"),
            (None, Some(_)) => format!("the SSH server refused the password of {user}"),
            (Some(_), Some(_)) => format!("the SSH server refused the private key and the password of {user}"),
        }))
    }
}

/// `url`'s user and password by server, and `url` without them.
fn without_login(url: &Url) -> (Option<(String, (String, Option<String>))>, Url) {
    let mut bare = url.clone();
    if url.username().is_empty() {
        return (None, bare);
    }
    let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
    let login = (server(url), (decode(url.username()), url.password().map(decode)));
    let _ = bare.set_password(None);
    let _ = bare.set_username("");
    (Some(login), bare)
}

/// The host as SSH's known_hosts names it: an IPv6 address without its brackets.
fn host_of(url: &Url) -> String {
    match url.host() {
        Some(Host::Ipv6(ip)) => ip.to_string(),
        Some(host) => host.to_string(),
        None => String::new(),
    }
}

/// `host:port` of `url`, which logins are kept by.
fn server(url: &Url) -> String {
    format!("{}:{}", host_of(url), url.port().unwrap_or(22))
}

/// The path on the server `url` names: from the root, or from the home folder after `/~/`.
fn remote_path(url: &Url) -> Result<String, String> {
    let path = percent_decode_str(url.path()).decode_utf8().map_err(|_| format!("{}: the path is not UTF-8", crate::history::redact_url(url.as_str())))?;
    Ok(match path.strip_prefix("/~/") {
        Some(home) => home.to_string(),
        None if path == "/~" => String::new(),
        None => path.into_owned(),
    })
}

/// The file's path on the server (see [`remote_path`]).
fn path_of(url: &Url) -> Result<String, String> {
    let path = remote_path(url)?;
    if path.is_empty() || path.ends_with('/') {
        return Err(format!("{} names a folder, not a file", crate::history::redact_url(url.as_str())));
    }
    Ok(path)
}

/// One connection of an SFTP download (see the module docs): takes ranges as an FTP worker does
/// and reads each at its offset over its SSH connection, opened once and again after a failure,
/// until the download stops it; while its width is below this worker, it waits.
pub(crate) async fn work(worker_id: usize, shared: WorkerShared, net: Arc<Net>) {
    let s = &shared;
    let mut conn = None;
    loop {
        if !s.within_width(worker_id).await {
            return;
        }
        let taken = tokio::select! {
            biased;
            _ = s.cancel.cancelled() => return,
            taken = ftp::take(s, worker_id) => taken,
        };
        let Some((chunk, mirror_id, url, slot)) = taken else {
            tokio::select! {
                _ = s.cancel.cancelled() => return,
                _ = tokio::time::sleep(ftp::IDLE_POLL) => continue,
            }
        };
        let outcome = fetch(s, &net, &mut conn, worker_id, &chunk, mirror_id, &url, &slot).await;
        if outcome.is_err() {
            conn = None;
        }
        if s.cancel.is_cancelled() {
            s.mirrors.lock().release_mirror(mirror_id);
            return;
        }
        let event = ftp::settle(s, worker_id, &chunk, mirror_id, outcome, &slot);
        drop(slot);
        if let Some(event) = event {
            if s.events.send(event).await.is_err() {
                return;
            }
        }
    }
}

/// Reads what is left of `chunk` from `url` over `conn`, which is opened (again) when it is to
/// another server or none: the file opened, read from the chunk's offset up to its end.
#[allow(clippy::too_many_arguments)]
async fn fetch(s: &WorkerShared, net: &Net, conn: &mut Option<Conn>, worker_id: usize, chunk: &Chunk, mirror_id: usize, url: &Url, slot: &HostSlot) -> Result<(), Failure> {
    let start = chunk.current_offset.load(Ordering::SeqCst);
    if start > chunk.end_offset.load(Ordering::SeqCst) {
        return Ok(()); // stolen away entirely before it started
    }
    let path = path_of(url).map_err(|e| (FailureKind::Fatal, e))?;
    let sent_at = Instant::now();
    let opening = async {
        if conn.as_ref().is_none_or(|c| c.server != server(url)) {
            *conn = Some(net.connect(url, worker_id).await?);
        }
        let sftp = &conn.as_ref().expect("connected above").sftp;
        let mut file = sftp.open(path.as_str()).await.map_err(sftp_failure)?;
        file.seek(SeekFrom::Start(start)).await.map_err(|e| transient(format!("SFTP: {e}")))?;
        Ok::<_, Failure>(file)
    };
    let file = tokio::select! {
        biased;
        _ = s.cancel.cancelled() => return Err(ftp::cancelled()),
        _ = chunk.revoked.cancelled() => return Err(ftp::taken_over()),
        opened = opening => opened?,
    };
    slot.accepted();
    let _ = s.events.try_send(WorkerEvent::Ttfb { worker_id, mirror_id, ttfb: sent_at.elapsed() });
    ftp::receive(s, worker_id, chunk, mirror_id, file, start).await
}

/// Whether `url` is for this module.
pub(crate) fn handles(url: &Url) -> bool {
    url.scheme() == "sftp"
}

/// The downloads of a folder link: one per file under it, subfolders kept, in a folder named
/// after it, each link with the folder link's sign-in; symbolic links are left out, so a link to
/// a folder above cannot loop. None for a file's link, which downloads as it is.
pub(crate) async fn list(url: &Url, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    let path = match remote_path(url) {
        Ok(path) => path,
        Err(e) => return Some(Err(e)),
    };
    let key = match options.ssh_key.as_ref().map(SshKey::open).transpose() {
        Ok(key) => key,
        Err(e) => return Some(Err(e)),
    };
    let (login, bare) = without_login(url);
    let net = Net::with(Dialer::new(&DownloadOptions::default()), DownloadOptions::default().stall_timeout_secs, login.into_iter().collect(), key);
    let conn = match net.connect(&bare, 0).await {
        Ok(conn) => conn,
        Err((_, e)) => return Some(Err(e)),
    };
    let top = if path.is_empty() { ".".to_string() } else { path.clone() };
    match conn.sftp.metadata(top.as_str()).await {
        Ok(attrs) if !attrs.file_type().is_dir() => return None,
        Ok(_) => {}
        Err(e) => return Some(Err(sftp_failure(e).1)),
    }
    Some(walk(&conn.sftp, url, &top).await)
}

/// One download per regular file under the folder `top` that `url` names (see [`list`]).
async fn walk(sftp: &SftpSession, url: &Url, top: &str) -> Result<Vec<Task>, String> {
    let name = top.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty() && *n != "." && *n != "~");
    let folder = crate::ingest::clean_path([name.or(url.host_str()).unwrap_or("sftp")])?;
    let (mut folders, mut tasks, mut read) = (std::collections::VecDeque::from([Vec::<String>::new()]), Vec::new(), 0);
    while let Some(dir) = folders.pop_front() {
        read += 1;
        if read > MAX_FOLDERS {
            return Err(format!("the folder holds more than {MAX_FOLDERS} folders: add a subfolder's link instead"));
        }
        let at = match dir.is_empty() {
            true => top.to_string(),
            false => format!("{}/{}", top.trim_end_matches('/'), dir.join("/")),
        };
        let entries = sftp.read_dir(at.as_str()).await.map_err(|e| sftp_failure(e).1)?;
        for entry in entries {
            let name = entry.file_name();
            if name == "." || name == ".." || name.contains('/') {
                continue;
            }
            let meta = entry.metadata();
            let path: Vec<String> = dir.iter().cloned().chain([name]).collect();
            if meta.file_type().is_dir() && path.len() < MAX_DEPTH {
                folders.push_back(path);
            } else if meta.file_type().is_file() {
                let Ok(name) = crate::ingest::clean_path(path.iter().map(String::as_str)) else { continue };
                let mut link = url.clone();
                link.path_segments_mut().map_err(|_| "unusable SFTP link".to_string())?.pop_if_empty().extend(&path);
                tasks.push(Task { urls: vec![link], name: Some(name), folder: Some(folder.clone()), size: meta.size, ..Task::default() });
            }
        }
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::DownloadEngine;
    use russh::keys::{Algorithm, EcdsaCurve};
    use russh::server::{Auth, ChannelOpenHandle, Msg, Session, VerifiedPublicKey};
    use russh::{Channel, ChannelId};
    use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle as SftpHandle, Name, OpenFlags, Status, StatusCode, Version};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::LazyLock;

    /// The host key of every test server but the key-change test's: a port used again by another
    /// test is the same host to the known-hosts file.
    static HOST_KEY: LazyLock<PrivateKey> = LazyLock::new(|| key(Algorithm::Ed25519));

    fn key(algorithm: Algorithm) -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), algorithm).unwrap()
    }

    /// A file's bytes by its path; folders are the paths' parents.
    type Files = Arc<HashMap<String, Vec<u8>>>;

    struct Server {
        files: Files,
        user_key: Option<PublicKey>,
        channels: HashMap<ChannelId, Channel<Msg>>,
    }

    impl russh::server::Handler for Server {
        type Error = russh::Error;

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
            Ok(if (user, password) == ("u", "pw") { Auth::Accept } else { Auth::reject() })
        }

        async fn auth_publickey(&mut self, user: &str, key: VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
            let ours = self.user_key.as_ref().is_some_and(|k| k.key_data() == key.public_key.key_data());
            Ok(if user == "u" && ours { Auth::Accept } else { Auth::reject() })
        }

        async fn channel_open_session(&mut self, channel: Channel<Msg>, reply: ChannelOpenHandle, _: &mut Session) -> Result<(), Self::Error> {
            self.channels.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn subsystem_request(&mut self, id: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
            match self.channels.remove(&id).filter(|_| name == "sftp") {
                Some(channel) => {
                    session.channel_success(id)?;
                    russh_sftp::server::run(channel.into_stream(), Sftp { files: self.files.clone(), listed: Vec::new() }).await;
                }
                None => session.channel_failure(id)?,
            }
            Ok(())
        }
    }

    /// The SFTP side, over `files`.
    struct Sftp {
        files: Files,
        listed: Vec<String>,
    }

    impl Sftp {
        fn attrs(&self, path: &str) -> Option<FileAttributes> {
            let mut attrs = FileAttributes { mtime: Some(1_700_000_000), ..FileAttributes::default() };
            if let Some(bytes) = self.files.get(path) {
                attrs.size = Some(bytes.len() as u64);
                attrs.set_regular(true);
            } else if path == "/link" {
                attrs.set_symlink(true);
            } else if self.files.keys().any(|f| f.starts_with(&format!("{}/", path.trim_end_matches('/')))) {
                attrs.set_dir(true);
            } else {
                return None;
            }
            Some(attrs)
        }
    }

    impl russh_sftp::server::Handler for Sftp {
        type Error = StatusCode;

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, Self::Error> {
            Ok(Version::new())
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            self.attrs(&path).map(|attrs| Attrs { id, attrs }).ok_or(StatusCode::NoSuchFile)
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            self.stat(id, path).await
        }

        async fn open(&mut self, id: u32, path: String, _: OpenFlags, _: FileAttributes) -> Result<SftpHandle, Self::Error> {
            self.files.contains_key(&path).then_some(SftpHandle { id, handle: path }).ok_or(StatusCode::NoSuchFile)
        }

        async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
            let bytes = self.files.get(&handle).ok_or(StatusCode::Failure)?;
            let start = usize::try_from(offset).unwrap_or(usize::MAX).min(bytes.len());
            if start == bytes.len() {
                return Err(StatusCode::Eof);
            }
            Ok(Data { id, data: bytes[start..(start + len as usize).min(bytes.len())].to_vec() })
        }

        async fn close(&mut self, id: u32, _: String) -> Result<Status, Self::Error> {
            Ok(Status { id, status_code: StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() })
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<SftpHandle, Self::Error> {
            self.attrs(&path).filter(|a| a.is_dir()).map(|_| SftpHandle { id, handle: path }).ok_or(StatusCode::NoSuchFile)
        }

        async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
            if self.listed.contains(&handle) {
                return Err(StatusCode::Eof);
            }
            self.listed.push(handle.clone());
            let inside = format!("{}/", handle.trim_end_matches('/'));
            let mut names: Vec<String> = self.files.keys().filter_map(|f| f.strip_prefix(&inside)).map(|rest| rest.split('/').next().unwrap().to_string()).collect();
            if handle == "/" {
                names.push("link".into());
            }
            names.sort();
            names.dedup();
            let files = names.into_iter().map(|n| File::new(n.clone(), self.attrs(&format!("{inside}{n}")).unwrap())).collect();
            Ok(Name { id, files })
        }

        async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
            Ok(Name { id, files: vec![File::dummy(path)] })
        }
    }

    /// An SSH server on a port of its own serving `files`, with `host_key`, that signs in user `u`
    /// with password `pw` or `user_key`.
    async fn server(files: &[(&str, Vec<u8>)], host_key: PrivateKey, user_key: Option<PublicKey>) -> SocketAddr {
        let files: Files = Arc::new(files.iter().map(|(p, b)| (p.to_string(), b.clone())).collect());
        let config = Arc::new(russh::server::Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let handler = Server { files: files.clone(), user_key: user_key.clone(), channels: HashMap::new() };
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, socket, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        addr
    }

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    async fn download(url: &str, dir: &Path, options: DownloadOptions) -> Result<PathBuf, String> {
        let options = DownloadOptions { output_path: Some(dir.join("")), num_connections: 4, max_retries: 1, ..options };
        DownloadEngine::new(vec![Url::parse(url).unwrap()], options).run(None).await
    }

    /// A file over several connections at their offsets, signed in by the link's password, which
    /// stays out of the history; a folder link lists its files, subfolders kept and its symbolic
    /// link left out; a wrong password ends the download at once.
    #[tokio::test]
    async fn files_and_folders_download_over_sftp_by_password() {
        let big = content(3 * 1024 * 1024 + 17);
        let addr = server(&[("/data/f.bin", big.clone()), ("/data/sub/a.txt", b"aaa".to_vec()), ("/top.txt", b"t".to_vec())], HOST_KEY.clone(), None).await;
        let dir = tempfile::tempdir().unwrap();
        let link = format!("sftp://u:pw@{addr}/data/f.bin");
        let path = download(&link, dir.path(), DownloadOptions::default()).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), big);
        assert_eq!(path.file_name().unwrap(), "f.bin");
        let history = std::fs::read_to_string(crate::history::DownloadHistoryManager::default_history_path()).unwrap_or_default();
        assert!(!history.contains(":pw@"), "the password stays out of the history");

        let listed = list(&Url::parse(&format!("sftp://u:pw@{addr}/data/")).unwrap(), &ListOptions::default()).await.unwrap().unwrap();
        let mut found: Vec<(String, String, Option<u64>)> =
            listed.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap().to_string_lossy().replace('\\', "/"), t.size)).collect();
        found.sort();
        let at = |p: &str| format!("sftp://u:pw@{addr}{p}");
        assert_eq!(found, vec![(at("/data/f.bin"), "f.bin".into(), Some(big.len() as u64)), (at("/data/sub/a.txt"), "sub/a.txt".into(), Some(3))]);
        assert_eq!(listed[0].folder.as_deref(), Some(Path::new("data")));
        let root = list(&Url::parse(&format!("sftp://u:pw@{addr}/")).unwrap(), &ListOptions::default()).await.unwrap().unwrap();
        assert_eq!(root.len(), 3, "the symbolic link is left out");
        assert!(list(&Url::parse(&link).unwrap(), &ListOptions::default()).await.is_none(), "a file's link downloads as it is");

        let refused = download(&format!("sftp://u:nope@{addr}/data/f.bin"), dir.path(), DownloadOptions::default()).await.unwrap_err();
        assert!(refused.contains("refused the password of u"), "{refused}");
        let nobody = download(&format!("sftp://{addr}/data/f.bin"), dir.path(), DownloadOptions::default()).await.unwrap_err();
        assert!(nobody.contains("names no user"), "{nobody}");
    }

    /// A private key signs in; an encrypted one says it needs its passphrase, and a wrong one
    /// that it may be wrong.
    #[tokio::test]
    async fn a_private_key_signs_in_with_its_passphrase() {
        let user = key(Algorithm::Ed25519);
        let addr = server(&[("/f.txt", b"by key".to_vec())], HOST_KEY.clone(), Some(user.public_key().clone())).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("id_ed25519");
        let locked = user.encrypt(&mut rand::rng(), "open sesame").unwrap();
        std::fs::write(&file, locked.to_openssh(russh::keys::ssh_key::LineEnding::LF).unwrap().as_bytes()).unwrap();
        assert!(needs_passphrase(&file));
        let with = |passphrase: Option<&str>| DownloadOptions {
            ssh_key: Some(SshKey { path: file.clone(), passphrase: passphrase.map(String::from) }),
            ..Default::default()
        };
        let link = format!("sftp://u@{addr}/f.txt");
        let none = download(&link, dir.path(), with(None)).await.unwrap_err();
        assert!(none.contains("is encrypted: give its passphrase"), "{none}");
        let wrong = download(&link, dir.path(), with(Some("nope"))).await.unwrap_err();
        assert!(wrong.contains("is its passphrase right"), "{wrong}");
        let path = download(&link, dir.path(), with(Some("open sesame"))).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"by key");
        assert!(!format!("{:?}", with(Some("open sesame")).ssh_key).contains("sesame"));
    }

    /// The first key a server shows is kept; the same key again is let in; another, of the same
    /// kind or not, is refused every time and the file is left as it was.
    #[tokio::test]
    async fn host_keys_are_kept_on_first_use_and_a_changed_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("known_hosts");
        let first = key(Algorithm::Ed25519);
        let addr = server(&[("/f.txt", b"x".to_vec())], first.clone(), None).await;
        let url = Url::parse(&format!("sftp://u:pw@{addr}/f.txt")).unwrap();
        let (mut net, links) = Net::new(&DownloadOptions::default(), std::slice::from_ref(&url)).unwrap();
        net.known_hosts = Some(file.clone());
        assert_eq!(net.probe(&links[0]).await.unwrap().0, Some(1));
        let kept = std::fs::read_to_string(&file).unwrap();
        assert!(kept.trim_start().starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 ", addr.port())), "{kept}");
        assert_eq!(net.probe(&links[0]).await.unwrap().0, Some(1), "the kept key lets it in");

        for other in [key(Algorithm::Ed25519), key(Algorithm::Ecdsa { curve: EcdsaCurve::NistP256 })] {
            let impostor = server(&[("/f.txt", b"y".to_vec())], other, None).await;
            // The same host and port in the file as the impostor's.
            let theirs = kept.replace(&format!(":{} ", addr.port()), &format!(":{} ", impostor.port()));
            std::fs::write(&file, &theirs).unwrap();
            let link = Url::parse(&format!("sftp://127.0.0.1:{}/f.txt", impostor.port())).unwrap();
            let (mut net, _) = Net::new(&DownloadOptions::default(), &[Url::parse(&format!("sftp://u:pw@127.0.0.1:{}/f.txt", impostor.port())).unwrap()]).unwrap();
            net.known_hosts = Some(file.clone());
            for _ in 0..2 {
                let refused = net.probe(&link).await.unwrap_err();
                assert!(refused.contains("REFUSED: the SSH host key of 127.0.0.1"), "{refused}");
            }
            assert_eq!(std::fs::read_to_string(&file).unwrap(), theirs, "nothing learned");
        }
    }

    #[test]
    fn paths_start_at_the_root_or_the_home_folder() {
        let path = |link: &str| path_of(&Url::parse(link).unwrap());
        assert_eq!(path("sftp://h/srv/a%20b.iso").unwrap(), "/srv/a b.iso");
        assert_eq!(path("sftp://h/~/notes.txt").unwrap(), "notes.txt");
        assert!(path("sftp://h/srv/").unwrap_err().contains("names a folder"));
        let (login, bare) = without_login(&Url::parse("sftp://al%40ice:p%3Aw@[::1]:2222/f").unwrap());
        assert_eq!(login, Some(("::1:2222".to_string(), ("al@ice".to_string(), Some("p:w".to_string())))));
        assert_eq!(bare.as_str(), "sftp://[::1]:2222/f");
    }
}
