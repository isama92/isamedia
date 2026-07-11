//! The wire protocol Sonarr and Radarr share (Radarr is a Sonarr fork):
//! `X-Api-Key` header auth, the `/api/v3/system/status` probe, the paginated
//! queue envelope and the release/grab contract. Backend-specific endpoints
//! live in `crate::sonarr` / `crate::radarr`, which wrap a [`Transport`].
//! Pure HTTP, no UI dependencies. The API key rides only in a header —
//! never in a URL, so it can never leak into a log line that prints one.

pub mod display;
pub mod models;

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;

use models::{Command, Page, QualityProfile, QueueItem, RootFolder};

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
pub const RELEASE_SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The queue endpoint is paginated; one page this size covers any sane queue,
/// and `paginate` caps the page count so a server misreporting `totalRecords`
/// can never chain-fetch forever.
const QUEUE_PAGE_SIZE: usize = 1000;
const QUEUE_MAX_PAGES: usize = 5;

/// The transport half of an *arr client: auth, timeouts, status mapping and
/// the endpoints whose contract is identical across backends.
#[derive(Clone)]
pub struct Transport {
    http: reqwest::Client,
    pub host: String,
    api_key: String,
}

// Hand-written so `{:?}` can never leak the API key into a log, no matter
// how carelessly a client ends up in a tracing call.
impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transport")
            .field("host", &self.host)
            .field("api_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Transport {
    /// Build a transport and validate the key with a status call; a 401/403
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
        let transport = Self {
            http,
            host,
            api_key,
        };
        transport
            .send(Method::GET, "/api/v3/system/status", &[], None, None)
            .await?;
        Ok(transport)
    }

    /// Send a request and map error statuses; 401/403 become
    /// `Error::Unauthorized` so the UI can drop back to the setup screen.
    pub(crate) async fn send(
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

    pub(crate) async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        timeout: Option<std::time::Duration>,
    ) -> Result<T, Error> {
        self.send_json(method, path, query, None, timeout).await
    }

    /// Like [`request`](Self::request) but forwards a body; the add endpoints
    /// POST a payload and return the created item, which the UI navigates
    /// straight to.
    pub(crate) async fn send_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&serde_json::Value>,
        timeout: Option<std::time::Duration>,
    ) -> Result<T, Error> {
        let response = self.send(method, path, query, body, timeout).await?;
        Ok(response.json().await?)
    }

    /// The configured library roots, offered as add targets. Same shape on
    /// both backends.
    pub async fn get_root_folders(&self) -> Result<Vec<RootFolder>, Error> {
        self.request(Method::GET, "/api/v3/rootfolder", &[], None)
            .await
    }

    /// The quality profiles, offered as add targets. Same shape on both
    /// backends.
    pub async fn get_quality_profiles(&self) -> Result<Vec<QualityProfile>, Error> {
        self.request(Method::GET, "/api/v3/qualityprofile", &[], None)
            .await
    }

    /// Follow a paginated `/api/v3` list to completion: adds `page`/`pageSize`
    /// on top of `extra_query` and accumulates records until the server has
    /// returned them all. Capped at `max_pages` so a server misreporting
    /// `totalRecords` can never chain-fetch forever.
    pub(crate) async fn paginate<T: DeserializeOwned + Default>(
        &self,
        path: &str,
        extra_query: &[(&str, &str)],
        page_size: usize,
        max_pages: usize,
    ) -> Result<Vec<T>, Error> {
        let page_size_str = page_size.to_string();
        let mut records = Vec::new();
        let mut complete = false;
        for page_number in 1..=max_pages {
            let page_number_str = page_number.to_string();
            let mut query: Vec<(&str, &str)> = vec![
                ("page", page_number_str.as_str()),
                ("pageSize", page_size_str.as_str()),
            ];
            query.extend_from_slice(extra_query);
            let page: Page<T> = self.request(Method::GET, path, &query, None).await?;
            let page_len = page.records.len();
            records.extend(page.records);
            // Stop once every record is in hand, or on a short (hence last)
            // page; the short-page guard also breaks a misreported total.
            if records.len() >= page.total_records.max(0) as usize || page_len < page_size {
                complete = true;
                break;
            }
        }
        // Exhausting the loop without the stop condition means the cap clipped
        // the results, unlike a clean finish; log it so a silent truncation
        // (e.g. a >max_pages history) is at least visible in debug output.
        if !complete {
            tracing::debug!(
                path,
                max_pages,
                fetched = records.len(),
                "paginate hit its page cap; results may be truncated"
            );
        }
        Ok(records)
    }

    /// The whole download queue; callers filter client-side, which sidesteps
    /// version differences in the server-side filters. `extra_query` carries
    /// backend-specific params (e.g. Sonarr's includeEpisode).
    pub async fn get_queue(&self, extra_query: &[(&str, &str)]) -> Result<Vec<QueueItem>, Error> {
        self.paginate(
            "/api/v3/queue",
            extra_query,
            QUEUE_PAGE_SIZE,
            QUEUE_MAX_PAGES,
        )
        .await
    }

    /// Poll one command's status by id. Used to track a background
    /// auto-search to completion; the endpoint is identical on both backends.
    pub async fn get_command(&self, command_id: i64) -> Result<Command, Error> {
        self.request(
            Method::GET,
            &format!("/api/v3/command/{command_id}"),
            &[],
            None,
        )
        .await
    }

    /// Grab a release from the latest interactive search; the server looks
    /// the release up in its cache by guid + indexer.
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

    /// Remove queue items by id via the bulk endpoint (which also covers the
    /// single-item case, so there is one code path). `remove_from_client`
    /// deletes the partial data from the download client; `blocklist` marks
    /// the release so it is not grabbed again.
    pub async fn delete_queue_items(
        &self,
        ids: &[i64],
        remove_from_client: bool,
        blocklist: bool,
    ) -> Result<(), Error> {
        let body = serde_json::json!({ "ids": ids });
        self.send(
            Method::DELETE,
            "/api/v3/queue/bulk",
            &[
                (
                    "removeFromClient",
                    if remove_from_client { "true" } else { "false" },
                ),
                ("blocklist", if blocklist { "true" } else { "false" }),
            ],
            Some(&body),
            None,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_secrets() {
        let transport = Transport {
            http: reqwest::Client::new(),
            host: "https://arr.example.com".into(),
            api_key: "sekret-key".into(),
        };
        let debug = format!("{transport:?}");
        assert!(!debug.contains("sekret-key"), "{debug}");
        assert!(debug.contains("<redacted>"));
        // The host still prints, which is what makes Debug useful.
        assert!(debug.contains("arr.example.com"));
    }
}
