//! A new link for a stopped download whose link expired (a CDN's signed link, `?expires=`): the
//! file it names is checked against what is downloaded already, then the download resumes from its
//! partial file through it instead of starting over.

use std::path::PathBuf;

use url::Url;

use crate::engine::{part_path, size_text, DownloadEngine, DownloadOptions};
use crate::range::{merge_ranges, ByteRange};
use crate::state::DownloadState;

/// What a server tells of the file a link names (see `DownloadEngine::probe`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemoteFile {
    /// None when it does not tell.
    pub size: Option<u64>,
    pub accepts_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Whether a download that failed with `error` was refused by its server with 401, 403, 404 or 410,
/// as a signed link past its time is answered. Only one that had started has an expired link (see
/// `QueueItem::link_expired`): one refused at once has a bad link.
pub fn refused(error: &str) -> bool {
    error.match_indices("HTTP ").any(|(at, _)| {
        let code = &error[at + 5..];
        ["401", "403", "404", "410"].iter().any(|status| code.starts_with(status)) && !code[3..].starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Why `remote` is not the file that a download of `size` bytes (0: not known) with the resume
/// state `state` was downloading, if it is not: another size, or none to compare; another
/// Last-Modified than the one it started with; or, with bytes downloaded, a server that cannot
/// resume (takes no ranges). An ETag is not compared: a CDN's edges give one file different ones.
pub fn check(size: u64, state: Option<&DownloadState>, remote: &RemoteFile) -> Result<(), String> {
    let known = state.map(|state| state.file_size).filter(|&size| size > 0).unwrap_or(size);
    match remote.size {
        _ if known == 0 => {}
        None => return Err(format!("This link doesn't say how big its file is, so it can't be checked against the {} one", size_text(known))),
        Some(new) if new != known => {
            let (new_text, known_text) = (size_text(new), size_text(known));
            let (new_text, known_text) = if new_text == known_text { (format!("{new} bytes"), format!("{known} bytes")) } else { (new_text, known_text) };
            return Err(format!("This link is for a different file: {new_text}, not {known_text}"));
        }
        Some(_) => {}
    }
    let Some(state) = state else { return Ok(()) };
    if let (Some(old), Some(new)) = (&state.last_modified, &remote.last_modified) {
        if old != new {
            return Err(format!("This link's file has changed: it was last modified {new}, not {old}"));
        }
    }
    let done: u64 = merge_ranges(state.completed_ranges.clone()).iter().map(ByteRange::len).sum();
    if done > 0 && !remote.accepts_ranges {
        return Err(format!("This link's server can't resume a download, so the {} already downloaded would be lost", size_text(done)));
    }
    Ok(())
}

/// Checks `urls` (links to one file) as the new links of the stopped download of `size` bytes saved
/// as `target` (None: nothing is on disk yet), asking their server with `options` (see [`check`]).
/// When they name that file, its resume state takes them, so that the next run resumes its partial
/// file through them; and forgets its ETag when the new server gives another, as a CDN's edges do,
/// for the resume not to take the file for a new one. Else why not, and nothing is changed.
pub async fn relink(target: Option<PathBuf>, size: u64, urls: Vec<Url>, options: DownloadOptions) -> Result<(), String> {
    if urls.is_empty() || !urls.iter().all(|url| matches!(url.scheme(), "http" | "https")) {
        return Err("A new link must be an http or https link".to_string());
    }
    // As the engine records its links (see `DownloadEngine::new`).
    let mirrors: Vec<String> = urls.iter().map(|url| crate::resolver::unwrap_redirect(url).unwrap_or_else(|| url.clone()).to_string()).collect();
    let path = target.map(|target| DownloadState::state_file_path(&part_path(&target)));
    let read = path.clone();
    // Building the engine may read the cookies file and TLS roots.
    let (engine, state) = tokio::task::spawn_blocking(move || {
        let state = read.as_deref().map(DownloadState::load_from_path).transpose();
        (DownloadEngine::new(urls, options), state)
    })
    .await
    .map_err(|e| e.to_string())?;
    let state = state.map_err(|e| format!("The download's resume state can't be read: {e}"))?.flatten();
    let remote = engine.probe().await.map_err(|e| format!("The new link can't be checked: {e}"))?;
    check(size, state.as_ref(), &remote)?;
    if let (Some(mut state), Some(path)) = (state, path) {
        for mirror in mirrors {
            if !state.mirrors.contains(&mirror) {
                state.mirrors.push(mirror);
            }
        }
        if state.etag != remote.etag {
            state.etag = None;
        }
        tokio::task::spawn_blocking(move || state.save_atomic(&path))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("The download's resume state can't be saved: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(size: u64, done: u64) -> DownloadState {
        let mut state = DownloadState::new("a.bin".into(), size, 256 << 10, vec!["https://cdn.example/old/a.bin?sig=1".into()]);
        if done > 0 {
            state.completed_ranges.push(ByteRange::new(0, done - 1).unwrap());
        }
        state
    }

    fn remote(size: Option<u64>) -> RemoteFile {
        RemoteFile { size, accepts_ranges: true, ..Default::default() }
    }

    /// Only a refusal (401, 403, 404, 410) counts, wherever the engine's error names it, not
    /// another status or a number that merely starts so.
    #[test]
    fn a_refused_link_is_told_from_other_failures() {
        for error in [
            "https://cdn.example/a.bin?sig=1: HTTP 403 Forbidden",
            "Download failed: every mirror failed; last error: HTTP 410 Gone",
            "HTTP 401 Unauthorized",
            "https://cdn.example/a.bin: HTTP 404 Not Found",
        ] {
            assert!(refused(error), "{error}");
        }
        for error in ["HTTP 500 Internal Server Error", "HTTP 4030", "HTTP 429 Too Many Requests", "connection reset", "403", "HTTP 40", ""] {
            assert!(!refused(error), "{error}");
        }
    }

    /// The same size is the file; another size, or none, is refused with both sizes. Without a
    /// size known, any is taken.
    #[test]
    fn a_new_link_must_have_the_files_size() {
        let started = state(640 << 20, 100 << 20);
        assert_eq!(check(0, Some(&started), &remote(Some(640 << 20))), Ok(()));
        assert_eq!(check(640 << 20, None, &remote(Some(640 << 20))), Ok(()), "the queue's size when no state is on disk");
        assert_eq!(check(0, Some(&started), &remote(Some(812 << 20))), Err("This link is for a different file: 812.0 MiB, not 640.0 MiB".into()));
        assert_eq!(check(0, Some(&state(1000, 10)), &remote(Some(1001))), Err("This link is for a different file: 1001 bytes, not 1000 bytes".into()));
        let close = check(0, Some(&state(640 << 20, 1)), &remote(Some((640 << 20) + 1))).unwrap_err();
        assert_eq!(close, format!("This link is for a different file: {} bytes, not {} bytes", (640 << 20) + 1, 640 << 20), "sizes that read alike are told in bytes");
        let no_size = check(0, Some(&started), &remote(None)).unwrap_err();
        assert!(no_size.starts_with("This link doesn't say how big its file is"), "{no_size}");
        assert_eq!(check(0, None, &remote(None)), Ok(()), "nothing known to compare");
        assert_eq!(check(0, Some(&state(0, 0)), &remote(Some(5))), Ok(()));
    }

    /// A Last-Modified both sides give must agree; an ETag need not (a CDN's edges differ). A
    /// server that cannot resume is refused only when bytes are downloaded.
    #[test]
    fn validators_and_ranges_are_checked_as_far_as_they_go() {
        let mut started = state(1000, 500);
        started.etag = Some("\"edge-a\"".into());
        started.last_modified = Some("Mon, 05 Oct 2026 10:00:00 GMT".into());
        let mut new = remote(Some(1000));
        new.etag = Some("\"edge-b\"".into());
        assert_eq!(check(0, Some(&started), &new), Ok(()), "another ETag");
        new.last_modified = Some("Mon, 05 Oct 2026 10:00:00 GMT".into());
        assert_eq!(check(0, Some(&started), &new), Ok(()));
        new.last_modified = Some("Tue, 06 Oct 2026 08:00:00 GMT".into());
        let changed = check(0, Some(&started), &new).unwrap_err();
        assert!(changed.starts_with("This link's file has changed"), "{changed}");

        let mut no_ranges = remote(Some(1000));
        no_ranges.accepts_ranges = false;
        assert_eq!(check(0, Some(&started), &no_ranges), Err("This link's server can't resume a download, so the 500 bytes already downloaded would be lost".into()));
        assert_eq!(check(0, Some(&state(1000, 0)), &no_ranges), Ok(()), "nothing downloaded: nothing to lose");
    }

    /// A link that is no http(s) link is refused before anything is asked.
    #[tokio::test]
    async fn only_http_links_are_taken() {
        let answer = relink(None, 0, vec![Url::parse("ftp://h.example/a.bin").unwrap()], DownloadOptions::default()).await;
        assert_eq!(answer, Err("A new link must be an http or https link".to_string()));
        assert!(relink(None, 0, Vec::new(), DownloadOptions::default()).await.is_err());
    }
}
