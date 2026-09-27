//! Cloud storage folder links (Google Drive, MediaFire) read into one download per file.

use url::Url;

use crate::ingest::{ListOptions, Task};

/// Whether `url` names a cloud storage folder this module lists, from its shape alone.
pub fn lists(_url: &Url) -> bool {
    false
}

/// One task per file in the folder at `url`, subfolders kept in `Task::folder` or `Task::name`;
/// called only when [`lists`] takes `url`. None when it is no folder after all (the link is then
/// downloaded as it is); `Some(Ok)` is never empty.
pub async fn list(_http: &reqwest::Client, _url: &Url, _options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    None
}
