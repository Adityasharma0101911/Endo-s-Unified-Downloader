use std::collections::HashSet;
use reqwest::Client;
use serde::Deserialize;
use url::Url;

#[derive(Debug, Deserialize)]
struct ArchiveMetadata {
    server: Option<String>,
    dir: Option<String>,
    workable_servers: Option<Vec<String>>,
}

/// Smart Resolver that inspects URLs and automatically discovers multi-cluster
/// mirror nodes (e.g. for Archive.org items via the official Metadata API).
pub struct SmartResolver;

impl SmartResolver {
    /// Expands a single URL into multiple direct storage cluster mirrors if available.
    pub async fn resolve_mirrors(client: &Client, url: &Url) -> Vec<Url> {
        if let Some(host) = url.host_str() {
            if host.ends_with("archive.org") {
                if let Some(mirrors) = Self::resolve_archive_org(client, url).await {
                    if !mirrors.is_empty() {
                        tracing::info!(
                            "Archive.org Smart Resolver: discovered {} physical cluster mirrors for {}",
                            mirrors.len(),
                            url
                        );
                        return mirrors;
                    }
                }
            }
        }

        // Default: return the original URL as a single mirror
        vec![url.clone()]
    }

    /// Resolves an archive.org/download/{identifier}/{filename} URL to its
    /// direct physical storage cluster nodes (iaXXXXXX.us.archive.org).
    async fn resolve_archive_org(client: &Client, url: &Url) -> Option<Vec<Url>> {
        let segments: Vec<&str> = url.path_segments()?.collect();

        // Expect /download/{identifier}/{filename...}
        if segments.len() < 3 || segments[0] != "download" {
            return None;
        }

        let identifier = segments[1];
        let filename = segments[2..].join("/");

        let metadata_url = format!("https://archive.org/metadata/{}", identifier);
        let resp = client.get(&metadata_url).send().await.ok()?;

        if !resp.status().is_success() {
            return None;
        }

        let bytes = resp.bytes().await.ok()?;
        let metadata: ArchiveMetadata = serde_json::from_slice(&bytes).ok()?;
        let dir = metadata.dir?;

        let mut servers = Vec::new();
        let mut seen = HashSet::new();

        if let Some(primary) = metadata.server {
            if seen.insert(primary.clone()) {
                servers.push(primary);
            }
        }

        if let Some(workable) = metadata.workable_servers {
            for ws in workable {
                if seen.insert(ws.clone()) {
                    servers.push(ws);
                }
            }
        }

        let mut mirror_urls = Vec::new();
        for s in servers {
            let mirror_str = format!("https://{}{}/{}", s, dir, filename);
            if let Ok(m_url) = Url::parse(&mirror_str) {
                mirror_urls.push(m_url);
            }
        }

        // Ensure original URL is also kept as fallback
        if !mirror_urls.iter().any(|u| u == url) {
            mirror_urls.push(url.clone());
        }

        Some(mirror_urls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_non_archive_url_resolves_to_self() {
        let client = Client::new();
        let url = Url::parse("https://example.com/file.zip").unwrap();
        let resolved = SmartResolver::resolve_mirrors(&client, &url).await;
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0], url);
    }
}
