//! Sonarr v3 REST API client (Sonarr v3 and v4 both serve /api/v3). Pure
//! HTTP, no UI dependencies. Auth is a single API key sent as the
//! `X-Api-Key` header on every request — never in the URL, so it can never
//! leak into a log line that prints one.

pub mod display;
pub mod models;

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;

pub use models::{Episode, HistoryRecord, QueueItem, Release, Series};

use models::Page;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    InvalidHost(String),
    #[error("no API key stored")]
    MissingApiKey,
    #[error("API key rejected by server")]
    Unauthorized,
    #[error("server returned {0}")]
    Status(StatusCode),
    #[error("cannot reach server: {0}")]
    Http(#[from] reqwest::Error),
}

/// Interactive search fans out to every indexer and only answers when the
/// slowest one does; the blanket 30s request timeout would cut it off.
const RELEASE_SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The queue endpoint is paginated; one page this size covers any sane queue,
/// and the loop below is capped so a server misreporting `totalRecords` can
/// never chain-fetch forever.
const QUEUE_PAGE_SIZE: usize = 1000;
const QUEUE_MAX_PAGES: usize = 5;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    pub host: String,
    api_key: String,
}

// Hand-written so `{:?}` can never leak the API key into a log, no matter
// how carelessly a Client ends up in a tracing call.
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("host", &self.host)
            .field("api_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Build a client and validate the key with a status call; a 401/403
    /// comes back as `Error::Unauthorized` so the UI can re-prompt for it.
    pub async fn connect(host: &str, api_key: String) -> Result<Self, Error> {
        let host =
            crate::net::normalize_host(host).map_err(|err| Error::InvalidHost(err.to_string()))?;
        // Every request path needs a timeout: an unbounded call can never be
        // cancelled from the render loop. The one endpoint that legitimately
        // runs long (release search) overrides this per-request.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        let client = Self {
            http,
            host,
            api_key,
        };
        client
            .send(Method::GET, "/api/v3/system/status", &[], None, None)
            .await?;
        Ok(client)
    }

    /// Send a request and map error statuses; 401/403 become
    /// `Error::Unauthorized` so the UI can drop back to the setup screen.
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&serde_json::Value>,
        timeout: Option<std::time::Duration>,
    ) -> Result<reqwest::Response, Error> {
        let mut request = self
            .http
            .request(method, format!("{}{}", self.host, path))
            .header("X-Api-Key", &self.api_key)
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = request.send().await?;
        match response.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::Unauthorized),
            status if !status.is_success() => Err(Error::Status(status)),
            _ => Ok(response),
        }
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        timeout: Option<std::time::Duration>,
    ) -> Result<T, Error> {
        let response = self.send(method, path, query, None, timeout).await?;
        Ok(response.json().await?)
    }

    pub async fn get_series(&self) -> Result<Vec<Series>, Error> {
        self.request(Method::GET, "/api/v3/series", &[], None).await
    }

    pub async fn get_episodes(&self, series_id: i64, season: i32) -> Result<Vec<Episode>, Error> {
        let series_id = series_id.to_string();
        let season = season.to_string();
        self.request(
            Method::GET,
            "/api/v3/episode",
            &[
                ("seriesId", series_id.as_str()),
                ("seasonNumber", season.as_str()),
                ("includeEpisodeFile", "true"),
            ],
            None,
        )
        .await
    }

    /// The whole download queue; callers filter by series client-side, which
    /// sidesteps the v3/v4 difference in server-side series filters.
    pub async fn get_queue(&self) -> Result<Vec<QueueItem>, Error> {
        let page_size = QUEUE_PAGE_SIZE.to_string();
        let mut records = Vec::new();
        for page_number in 1..=QUEUE_MAX_PAGES {
            let page_number_str = page_number.to_string();
            let page: Page<QueueItem> = self
                .request(
                    Method::GET,
                    "/api/v3/queue",
                    &[
                        ("page", page_number_str.as_str()),
                        ("pageSize", page_size.as_str()),
                        ("includeEpisode", "true"),
                    ],
                    None,
                )
                .await?;
            let page_len = page.records.len();
            records.extend(page.records);
            // The empty-page guard stops the chain if the server misreports
            // the total.
            if records.len() >= page.total_records.max(0) as usize || page_len == 0 {
                break;
            }
        }
        Ok(records)
    }

    pub async fn search_releases(&self, episode_id: i64) -> Result<Vec<Release>, Error> {
        let episode_id = episode_id.to_string();
        self.request(
            Method::GET,
            "/api/v3/release",
            &[("episodeId", episode_id.as_str())],
            Some(RELEASE_SEARCH_TIMEOUT),
        )
        .await
    }

    /// Grab a release from the latest interactive search; Sonarr looks the
    /// release up in its cache by guid + indexer.
    pub async fn grab_release(&self, guid: &str, indexer_id: i64) -> Result<(), Error> {
        self.send(
            Method::POST,
            "/api/v3/release",
            &[],
            Some(&serde_json::json!({ "guid": guid, "indexerId": indexer_id })),
            None,
        )
        .await?;
        Ok(())
    }

    /// Kick off Sonarr's own automatic search for one episode.
    pub async fn episode_search(&self, episode_id: i64) -> Result<(), Error> {
        self.send(
            Method::POST,
            "/api/v3/command",
            &[],
            Some(&serde_json::json!({ "name": "EpisodeSearch", "episodeIds": [episode_id] })),
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn delete_episode_file(&self, file_id: i64) -> Result<(), Error> {
        self.send(
            Method::DELETE,
            &format!("/api/v3/episodefile/{file_id}"),
            &[],
            None,
            None,
        )
        .await?;
        Ok(())
    }

    /// Past grab/import/delete events of one episode, for the
    /// "grabbed before" marker in interactive search results.
    pub async fn get_history(&self, episode_id: i64) -> Result<Vec<HistoryRecord>, Error> {
        let episode_id = episode_id.to_string();
        let page: Page<HistoryRecord> = self
            .request(
                Method::GET,
                "/api/v3/history",
                &[
                    ("episodeId", episode_id.as_str()),
                    ("page", "1"),
                    ("pageSize", "100"),
                ],
                None,
            )
            .await?;
        Ok(page.records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_secrets() {
        let client = Client {
            http: reqwest::Client::new(),
            host: "https://sonarr.example.com".into(),
            api_key: "sekret-key".into(),
        };
        let debug = format!("{client:?}");
        assert!(!debug.contains("sekret-key"), "{debug}");
        assert!(debug.contains("<redacted>"));
        // The host still prints, which is what makes Debug useful.
        assert!(debug.contains("sonarr.example.com"));
    }
}
