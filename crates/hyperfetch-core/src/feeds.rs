//! Podcast and RSS/Atom feeds read into one download per episode.

use url::Url;

use crate::ingest::{ListOptions, Task};

/// Whether `url` names a feed (or a podcast show page) this module lists, from its shape alone.
pub fn lists(_url: &Url) -> bool {
    false
}

/// One task per episode of the feed at `url`; called only when [`lists`] takes `url`. None when
/// it is no feed after all (the link is then downloaded as it is); `Some(Ok)` is never empty.
pub async fn list(_http: &reqwest::Client, _url: &Url, _options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    None
}
