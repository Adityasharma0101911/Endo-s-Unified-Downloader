//! Why is it slow?: what holds a running download back, told only from what the app knows of it
//! (its options, what its engine reported, what the engine learned of its server), each with the
//! fix the app can make when there is one.

use hyperfetch_core::queue::QueueItem;

use crate::util::format_bytes;
use crate::JobView;

/// Most connections one download opens, whatever it asks for (the engine's `MAX_CONNECTIONS`).
const MOST: usize = 64;

/// A change that lifts what holds a download back, which the app makes by starting the download
/// again with it, from where it is (see `App::restart_with`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fix {
    /// This many connections.
    Connections(usize),
    /// No speed limit for it.
    NoLimit,
}

/// One thing that holds a download back, in plain words, and its fix when the app has one.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub text: String,
    pub fix: Option<Fix>,
}

fn finding(text: String, fix: Option<Fix>) -> Finding {
    Finding { text, fix }
}

/// A chunk's failure that the server answered busy (429 or 503), which the engine waits out.
fn busy(reason: &str) -> bool {
    reason.starts_with("HTTP 429") || reason.starts_with("HTTP 503")
}

/// What holds running download `item` back, from what its engine reported (`view`): the server
/// answering busy, the speed limit it runs at, a server that sends the file over one connection or
/// caps each one, the server's connection cap or the Connections per server setting, a small file,
/// connections failing or silent, and a proxy. `on_host` is the connections all running downloads
/// have open to its server, `limited` the speed of all those under its speed limit together.
/// Empty when nothing in the app holds it back.
pub fn findings(item: &QueueItem, view: &JobView, on_host: usize, limited: f64) -> Vec<Finding> {
    let options = &item.options;
    let mut found = Vec::new();
    let failed: Vec<&str> = view.chunks.iter().filter_map(|chunk| chunk.status.strip_prefix("Failed: ")).collect();
    if failed.iter().any(|reason| busy(reason)) {
        let text = "The server answered that it is busy (HTTP 429 or 503): the download waits as long as it asks, then tries again.";
        found.push(finding(text.to_string(), None));
    }
    if let Some(limit) = options.max_speed.filter(|&limit| limited >= 0.9 * limit as f64) {
        let text = format!("Your speed limit holds it: the downloads under it run at its {}/s together.", format_bytes(limit));
        found.push(finding(text, Some(Fix::NoLimit)));
    }
    // What its connections tell: only of a download over the engine's own HTTP connections.
    if let Some(host) = view.host {
        let (connections, wanted) = (view.active_workers, options.num_connections.clamp(1, MOST));
        if connections <= 1 && host.accepts_ranges == Some(false) {
            found.push(finding("The server does not send parts of the file, so it comes over one connection.".to_string(), None));
        } else if connections <= 1 && item.total_bytes == 0 {
            found.push(finding("The server does not tell the file's size, so it comes over one connection.".to_string(), None));
        } else if connections >= wanted {
            if host.capped_per_connection == Some(true) && wanted < MOST {
                let each = host.connection_rate.map_or_else(String::new, |rate| format!(" (about {}/s each)", format_bytes(rate as u64)));
                let text = format!("The server caps each connection's speed{each}, and the download has all the {wanted} connections it may: more would bring more.");
                found.push(finding(text, Some(Fix::Connections((wanted * 2).min(MOST)))));
            }
        } else if let Some(cap) = host.connection_cap.filter(|&cap| on_host >= cap) {
            let text = format!("The server serves {cap} connections at once and refused more, so it gets no more than that.");
            found.push(finding(text, None));
        } else if options.max_connections_per_host > 0 && on_host >= options.max_connections_per_host {
            let text = format!(
                "The Connections per server setting holds it: {} connections to this server, for all downloads from it together.",
                options.max_connections_per_host
            );
            found.push(finding(text, None));
        } else if item.small {
            let text = format!("The file is small ({}): more connections would take longer to open than to bring it.", format_bytes(item.total_bytes));
            found.push(finding(text, None));
        }
    }
    let retrying: Vec<&&str> = failed.iter().filter(|reason| !busy(reason)).collect();
    if let Some(stalled) = view.stalled_for() {
        let text = format!(
            "No data has come for {} s: a connection that stays silent for {} s is replaced.",
            stalled.as_secs(),
            options.stall_timeout_secs
        );
        found.push(finding(text, None));
    } else if let Some(last) = retrying.last() {
        found.push(finding(format!("{} of its parts failed and wait to be tried again (last: {last}).", retrying.len()), None));
    }
    if options.proxy.is_some() || !options.proxy_pool.is_empty() {
        let text = "It goes through your proxy (Settings > Connections & networks): the proxy's speed counts as much as the server's.";
        found.push(finding(text.to_string(), None));
    }
    found
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use hyperfetch_core::chunk::ChunkSnapshot;
    use hyperfetch_core::engine::DownloadOptions;
    use hyperfetch_core::hosts::HostProfile;
    use hyperfetch_core::queue::DownloadQueue;

    use super::*;

    /// A running download of 100 MiB at 1 MB/s with `options`.
    fn item(options: DownloadOptions) -> QueueItem {
        let mut queue = DownloadQueue::new();
        let id = queue.add_item(vec![url::Url::parse("https://example.com/big.iso").unwrap()], options);
        queue.mark_started(id);
        let mut item = queue.get_item(id).unwrap().clone();
        (item.total_bytes, item.speed_bytes_per_sec) = (100 << 20, 1e6);
        item
    }

    /// What `connections` of a download over HTTP to a server of which `host` is known reported.
    fn view(connections: usize, host: HostProfile) -> JobView {
        JobView { active_workers: connections, host: Some(host), got_snapshot: true, last_progress: Some(Instant::now()), ..Default::default() }
    }

    fn texts(found: &[Finding]) -> Vec<&str> {
        found.iter().map(|finding| finding.text.as_str()).collect()
    }

    fn chunk(status: &str) -> ChunkSnapshot {
        ChunkSnapshot { id: 0, range_start: 0, range_end: 9, downloaded_bytes: 0, total_bytes: 10, status: status.to_string(), worker_id: None }
    }

    /// With 16 connections of 16 open and nothing known against the server, nothing in the app
    /// holds the download back.
    #[test]
    fn nothing_holds_back_a_download_with_all_it_asked_for() {
        let options = DownloadOptions { num_connections: 16, ..Default::default() };
        assert_eq!(texts(&findings(&item(options), &view(16, HostProfile::default()), 16, 1e6)), Vec::<&str>::new());
    }

    /// The speed limit holds a download when all those under it run at it together (it is
    /// shared), and its fix lifts it; well below it, it holds nothing.
    #[test]
    fn the_speed_limit_holds_it_when_they_run_at_it() {
        let options = DownloadOptions { num_connections: 4, max_speed: Some(2_000_000), ..Default::default() };
        let found = findings(&item(options.clone()), &view(4, HostProfile::default()), 4, 1_950_000.0);
        assert_eq!(found.len(), 1);
        assert!(found[0].text.starts_with("Your speed limit holds it") && found[0].text.contains("1.91 MiB/s"), "{found:?}");
        assert_eq!(found[0].fix, Some(Fix::NoLimit));
        assert_eq!(texts(&findings(&item(options), &view(4, HostProfile::default()), 4, 1e6)), Vec::<&str>::new());
    }

    /// A server that sends no parts of the file, or does not tell its size, gives it one
    /// connection.
    #[test]
    fn one_connection_when_the_server_sends_no_parts_or_no_size() {
        let options = DownloadOptions { num_connections: 16, ..Default::default() };
        let no_ranges = HostProfile { accepts_ranges: Some(false), ..Default::default() };
        let found = findings(&item(options.clone()), &view(1, no_ranges), 1, 1e6);
        assert_eq!(texts(&found), ["The server does not send parts of the file, so it comes over one connection."]);
        let mut unsized_file = item(options);
        unsized_file.total_bytes = 0;
        let found = findings(&unsized_file, &view(1, HostProfile { accepts_ranges: Some(true), ..Default::default() }), 1, 1e6);
        assert_eq!(texts(&found), ["The server does not tell the file's size, so it comes over one connection."]);
    }

    /// A server that caps each connection, with every connection the download may have open,
    /// would give more over more: the fix doubles them, up to 64. At 64 there is no more to ask.
    #[test]
    fn a_server_capping_each_connection_takes_more_connections() {
        let capped = HostProfile { capped_per_connection: Some(true), connection_rate: Some(256.0 * 1024.0), ..Default::default() };
        let found = findings(&item(DownloadOptions { num_connections: 8, ..Default::default() }), &view(8, capped), 8, 1e6);
        assert_eq!(found.len(), 1);
        assert!(found[0].text.contains("(about 256.00 KiB/s each)") && found[0].text.contains("all the 8 connections"), "{found:?}");
        assert_eq!(found[0].fix, Some(Fix::Connections(16)));
        let found = findings(&item(DownloadOptions { num_connections: 40, ..Default::default() }), &view(40, capped), 40, 1e6);
        assert_eq!(found[0].fix, Some(Fix::Connections(64)));
        assert_eq!(texts(&findings(&item(DownloadOptions { num_connections: 64, ..Default::default() }), &view(64, capped), 64, 1e6)), Vec::<&str>::new());
        let uncapped = HostProfile { capped_per_connection: Some(false), ..capped };
        assert_eq!(texts(&findings(&item(DownloadOptions { num_connections: 8, ..Default::default() }), &view(8, uncapped), 8, 1e6)), Vec::<&str>::new());
    }

    /// Fewer connections than asked for: the server's own cap, else the Connections per server
    /// setting (all downloads to the server together), else a small file says why.
    #[test]
    fn fewer_connections_tell_what_holds_them() {
        let options = DownloadOptions { num_connections: 16, max_connections_per_host: 8, ..Default::default() };
        let capped = HostProfile { connection_cap: Some(4), ..Default::default() };
        let found = findings(&item(options.clone()), &view(4, capped), 4, 1e6);
        assert_eq!(texts(&found), ["The server serves 4 connections at once and refused more, so it gets no more than that."]);

        let found = findings(&item(options.clone()), &view(3, HostProfile::default()), 8, 1e6);
        assert_eq!(texts(&found), ["The Connections per server setting holds it: 8 connections to this server, for all downloads from it together."]);

        let mut small = item(options.clone());
        (small.total_bytes, small.small) = (3 << 20, true);
        let found = findings(&small, &view(2, HostProfile::default()), 2, 1e6);
        assert_eq!(texts(&found), ["The file is small (3.00 MiB): more connections would take longer to open than to bring it."]);
        // Still opening its connections, with room for more: nothing says why yet.
        assert_eq!(texts(&findings(&item(options), &view(3, HostProfile::default()), 3, 1e6)), Vec::<&str>::new());
    }

    /// A server answering busy is waited out; parts that failed otherwise wait to be tried again;
    /// a download with no data for a while waits on a silent connection.
    #[test]
    fn busy_failing_and_silent_connections_are_told() {
        let options = DownloadOptions { num_connections: 4, stall_timeout_secs: 30, ..Default::default() };
        let mut waiting = view(4, HostProfile::default());
        waiting.chunks = vec![chunk("Worker 1"), chunk("Failed: HTTP 503 Service Unavailable"), chunk("Failed: connection reset"), chunk("Failed: timed out")];
        let found = findings(&item(options.clone()), &waiting, 4, 1e6);
        assert_eq!(
            texts(&found),
            [
                "The server answered that it is busy (HTTP 429 or 503): the download waits as long as it asks, then tries again.",
                "2 of its parts failed and wait to be tried again (last: timed out).",
            ]
        );
        let mut silent = view(4, HostProfile::default());
        silent.last_progress = Some(Instant::now() - Duration::from_secs(12));
        // Only a download whose engine runs can stall.
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        silent.running = Some(crate::Running { cancel: Default::default(), task: rt.spawn(async {}), snapshot: Default::default() });
        let found = findings(&item(options), &silent, 4, 1e6);
        assert_eq!(texts(&found), ["No data has come for 12 s: a connection that stays silent for 30 s is replaced."]);
    }

    /// A proxy in the way is told; a download that is no plain HTTP one (a torrent, a video from a
    /// site) says nothing of connections it does not have.
    #[test]
    fn a_proxy_is_told_and_other_downloads_say_nothing_of_connections() {
        let options = DownloadOptions { num_connections: 16, proxy: Some("http://proxy.example:8080".into()), ..Default::default() };
        let found = findings(&item(options), &view(16, HostProfile::default()), 16, 1e6);
        assert_eq!(texts(&found), ["It goes through your proxy (Settings > Connections & networks): the proxy's speed counts as much as the server's."]);
        let video = JobView { active_workers: 1, got_snapshot: true, last_progress: Some(Instant::now()), ..Default::default() };
        assert_eq!(texts(&findings(&item(DownloadOptions { num_connections: 16, ..Default::default() }), &video, 1, 1e6)), Vec::<&str>::new());
    }
}
