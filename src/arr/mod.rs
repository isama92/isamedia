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

use models::{Page, QualityProfile, QueueItem, RootFolder};

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
/// and the loop below is capped so a server misreporting `totalRecords` can
/// never chain-fetch forever.
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
        let response = self.send(method, path, query, None, timeout).await?;
        Ok(response.json().await?)
    }

    /// Like [`request`](Self::request) but forwards a body and reads the
    /// response back; the add endpoints POST a payload and return the created
    /// item, which the UI navigates straight to.
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

    /// The whole download queue; callers filter client-side, which sidesteps
    /// version differences in the server-side filters. `extra_query` carries
    /// backend-specific params (e.g. Sonarr's includeEpisode).
    pub async fn get_queue(&self, extra_query: &[(&str, &str)]) -> Result<Vec<QueueItem>, Error> {
        let page_size = QUEUE_PAGE_SIZE.to_string();
        let mut records = Vec::new();
        for page_number in 1..=QUEUE_MAX_PAGES {
            let page_number_str = page_number.to_string();
            let mut query: Vec<(&str, &str)> = vec![
                ("page", page_number_str.as_str()),
                ("pageSize", page_size.as_str()),
            ];
            query.extend_from_slice(extra_query);
            let page: Page<QueueItem> = self
                .request(Method::GET, "/api/v3/queue", &query, None)
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
