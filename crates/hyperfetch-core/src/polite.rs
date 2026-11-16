//! Requests a listing makes of a site that asks to be asked slowly (the Internet Archive, the
//! Wayback Machine, arXiv, Common Crawl): at most one every `gap` to a host, across every listing
//! of this process, and under the host's shared rest (see `crate::hosts`): a 429 or 503 rests the
//! host for its Retry-After, downloads from it too, and the request is made again after.

use std::collections::HashMap;
use std::time::Duration;

use parking_lot::Mutex;
use reqwest::{RequestBuilder, Response, StatusCode};
use url::Url;

/// How many times one request is made while its host refuses it (429/503) or does not answer.
const ATTEMPTS: u32 = 4;
/// The rest after a refusal that names none, twice as long after each refusal in a row.
const FIRST_REST: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(5) };
/// Longest rest a listing waits out for one request: a host asking for longer is reported.
const MAX_REST: Duration = if cfg!(test) { Duration::from_secs(2) } else { Duration::from_secs(5 * 60) };
/// Longest one request may take, its answer's headers included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// When the next request to each host (scheme, host and port) may go out.
static NEXT: Mutex<Option<HashMap<String, tokio::time::Instant>>> = Mutex::new(None);

fn host_of(url: &Url) -> String {
    format!("{}://{}:{}", url.scheme(), url.host_str().unwrap_or_default(), url.port_or_known_default().unwrap_or(0))
}

/// Waits until `url`'s host may be asked again: `gap` after the request to it before (made here).
pub(crate) async fn pace(url: &Url, gap: Duration) {
    let wait = {
        let mut next = NEXT.lock();
        let next = next.get_or_insert_with(HashMap::new);
        let now = tokio::time::Instant::now();
        // Hosts asked long ago are forgotten.
        next.retain(|_, at| *at + Duration::from_secs(600) > now);
        let at = next.get(&host_of(url)).copied().filter(|at| *at > now).unwrap_or(now);
        next.insert(host_of(url), at + gap);
        at - now
    };
    tokio::time::sleep(wait).await;
}

fn busy(status: StatusCode) -> bool {
    matches!(status, StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE)
}

/// A GET of `url` (see [`send`]).
pub(crate) async fn get(http: &reqwest::Client, url: &Url, gap: Duration) -> Result<Response, String> {
    send(url, gap, || http.get(url.clone())).await
}

/// The answer to `request` (made for `url`), paced to one every `gap` to its host and made again
/// while the host refuses it (429/503) or does not answer, [`ATTEMPTS`] times in all. A refusal
/// rests the host (see `crate::hosts`) for its Retry-After, else for a wait that doubles. Any
/// other answer is returned as it is, a refusal too once the attempts are used up or when the host
/// asks for a rest longer than [`MAX_REST`]; `Err` when the host could not be reached.
pub(crate) async fn send(url: &Url, gap: Duration, request: impl Fn() -> RequestBuilder) -> Result<Response, String> {
    let mut rest = FIRST_REST;
    let host = url.host_str().unwrap_or_default().to_string();
    for attempt in 1..=ATTEMPTS {
        pace(url, gap).await;
        let slot = crate::hosts::acquire(url, 0).await;
        let sent = tokio::time::timeout(REQUEST_TIMEOUT, request().send()).await;
        match sent {
            Ok(Ok(answer)) if busy(answer.status()) => {
                let asked = crate::worker::retry_after(answer.headers());
                slot.throttled(asked.unwrap_or(rest).min(MAX_REST));
                if attempt == ATTEMPTS || asked.is_some_and(|asked| asked > MAX_REST) {
                    return Ok(answer);
                }
                tracing::info!("{} asks to slow down (HTTP {}): asking again later", host, answer.status().as_u16());
                rest = rest.saturating_mul(2);
            }
            Ok(Ok(answer)) => {
                slot.accepted();
                return Ok(answer);
            }
            Ok(Err(e)) if attempt == ATTEMPTS => return Err(format!("Cannot reach {}: {}", host, e)),
            Err(_) if attempt == ATTEMPTS => return Err(format!("{} did not answer within {} s", host, REQUEST_TIMEOUT.as_secs())),
            _ => {
                let banned = slot.unanswered();
                drop(slot);
                tokio::time::sleep(banned.unwrap_or(rest)).await;
                rest = rest.saturating_mul(2);
            }
        }
    }
    unreachable!("the last attempt returns")
}

/// What a refusal `status` of `site` says to the user, or a plain error.
pub(crate) fn refused(site: &str, status: StatusCode) -> String {
    if busy(status) {
        format!("{} is limiting requests (HTTP {}): try again later", site, status.as_u16())
    } else {
        format!("{} answered HTTP {}", site, status.as_u16())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::serve;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Requests to one host go out `gap` apart however many listings make them, and a refusal
    /// (429 with Retry-After) is waited out and asked again; the answer after is the one returned.
    #[tokio::test]
    async fn requests_are_paced_and_a_refusal_is_asked_again() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        let (addr, hits) = serve(move |path, _| match path {
            "/busy" if counter.fetch_add(1, Ordering::SeqCst) == 0 => (429, "Retry-After: 1\r\n".into(), Vec::new()),
            _ => (200, String::new(), b"ok".to_vec()),
        })
        .await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = |path: &str| Url::parse(&format!("http://{addr}{path}")).unwrap();
        let started = tokio::time::Instant::now();
        let gap = Duration::from_millis(150);
        let (a, b, c) = tokio::join!(get(&http, &url("/a"), gap), get(&http, &url("/b"), gap), get(&http, &url("/c"), gap));
        assert!([a, b, c].iter().all(|r| r.as_ref().is_ok_and(|r| r.status() == 200)));
        assert!(started.elapsed() >= gap * 2, "three requests, two gaps: {:?}", started.elapsed());
        let answer = get(&http, &url("/busy"), Duration::ZERO).await.unwrap();
        assert_eq!((answer.status(), hits.lock()["/busy"]), (StatusCode::OK, 2), "asked again after the rest it asked for");
        assert!(started.elapsed() >= Duration::from_secs(1));
    }
}
