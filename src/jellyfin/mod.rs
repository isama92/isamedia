//! Jellyfin REST API client. Pure HTTP, no UI dependencies.
//!
//! There is no maintained Rust SDK for Jellyfin, so this hand-rolls the
//! handful of endpoints jfsh used, targeting the 10.9+ route names (the same
//! ones the sj14/jellyfin-go SDK calls).

pub mod display;
pub mod models;
pub mod url;

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;

pub use models::{ItemsResponse, MediaItem, MediaSegment};

use models::{AuthRequest, AuthResponse, PlaybackInfo, SegmentsResponse};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    InvalidHost(String),
    #[error("authentication failed (wrong username or password?)")]
    AuthFailed,
    #[error("session expired")]
    Unauthorized,
    #[error("server returned {0}")]
    Status(StatusCode),
    #[error("cannot reach server: {0}")]
    Http(#[from] reqwest::Error),
}

/// Everything needed to (re)connect. Mirrors the jellyfin section of the
/// config file.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub host: String,
    pub username: String,
    pub password: String,
    pub device: String,
    pub device_id: String,
    pub version: String,
    pub token: String,
    pub user_id: String,
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    pub host: String,
    pub user_id: String,
    pub token: String,
    auth_header: String,
}

fn auth_header(device: &str, device_id: &str, version: &str, token: Option<&str>) -> String {
    let mut header = format!(
        "MediaBrowser Client=\"isamedia\", Device=\"{device}\", DeviceId=\"{device_id}\", Version=\"{version}\""
    );
    if let Some(token) = token {
        header.push_str(&format!(", Token=\"{token}\""));
    }
    header
}

impl Client {
    /// Build a client, authenticating with username/password only when no
    /// token is stored yet (same short-circuit jfsh does).
    pub async fn connect(creds: Credentials) -> Result<Self, Error> {
        let host = url::normalize_host(&creds.host)?;
        let http = reqwest::Client::new();

        let (token, user_id) = if creds.token.is_empty() || creds.user_id.is_empty() {
            let header = auth_header(&creds.device, &creds.device_id, &creds.version, None);
            let response = http
                .post(format!("{host}/Users/AuthenticateByName"))
                .header(reqwest::header::AUTHORIZATION, header)
                .json(&AuthRequest {
                    username: &creds.username,
                    pw: &creds.password,
                })
                .send()
                .await?;
            match response.status() {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(Error::AuthFailed),
                status if !status.is_success() => return Err(Error::Status(status)),
                _ => {}
            }
            let auth: AuthResponse = response.json().await?;
            (auth.access_token, auth.user.id)
        } else {
            (creds.token, creds.user_id)
        };

        let auth_header = auth_header(
            &creds.device,
            &creds.device_id,
            &creds.version,
            Some(&token),
        );
        Ok(Self {
            http,
            host,
            user_id,
            token,
            auth_header,
        })
    }

    /// Send a request and map error statuses; 401 becomes `Error::Unauthorized`
    /// so the UI can drop back to the login screen.
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&PlaybackInfo<'_>>,
    ) -> Result<reqwest::Response, Error> {
        let mut request = self
            .http
            .request(method, format!("{}{}", self.host, path))
            .header(reqwest::header::AUTHORIZATION, &self.auth_header)
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        match response.status() {
            StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
            status if !status.is_success() => Err(Error::Status(status)),
            _ => Ok(response),
        }
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, Error> {
        let response = self.send(method, path, query, None).await?;
        Ok(response.json().await?)
    }

    async fn get_items(&self, path: &str, query: &[(&str, &str)]) -> Result<Vec<MediaItem>, Error> {
        let response: ItemsResponse = self.request(Method::GET, path, query).await?;
        Ok(response.items)
    }

    pub async fn get_resume(&self) -> Result<Vec<MediaItem>, Error> {
        self.get_items(
            "/UserItems/Resume",
            &[
                ("userId", self.user_id.as_str()),
                ("fields", "MediaStreams"),
            ],
        )
        .await
    }

    pub async fn get_next_up(&self) -> Result<Vec<MediaItem>, Error> {
        self.get_items(
            "/Shows/NextUp",
            &[
                ("fields", "MediaStreams"),
                ("enableTotalRecordCount", "false"),
                ("disableFirstEpisode", "false"),
                ("enableResumable", "false"),
                ("enableRewatching", "false"),
            ],
        )
        .await
    }

    pub async fn get_recently_added(&self) -> Result<Vec<MediaItem>, Error> {
        self.get_items(
            "/Items",
            &[
                ("recursive", "true"),
                ("includeItemTypes", "Movie,Series"),
                ("fields", "MediaStreams"),
                ("limit", "100"),
                ("sortBy", "DateCreated"),
                ("sortOrder", "Descending"),
            ],
        )
        .await
    }

    pub async fn search(&self, query: &str) -> Result<Vec<MediaItem>, Error> {
        self.get_items(
            "/Items",
            &[
                ("searchTerm", query),
                ("recursive", "true"),
                ("includeItemTypes", "Movie,Series"),
                ("fields", "MediaStreams"),
                ("limit", "100"),
            ],
        )
        .await
    }

    /// Episodes of a series; accepts a series or an episode of it.
    pub async fn get_episodes(&self, item: &MediaItem) -> Result<Vec<MediaItem>, Error> {
        let series_id = if item.kind == models::ItemKind::Series {
            item.id.as_str()
        } else {
            item.series_id.as_deref().unwrap_or(item.id.as_str())
        };
        self.get_items(
            &format!("/Shows/{series_id}/Episodes"),
            &[("fields", "MediaStreams")],
        )
        .await
    }

    async fn report(&self, path: &str, item_id: &str, ticks: i64) -> Result<(), Error> {
        self.send(
            Method::POST,
            path,
            &[],
            Some(&PlaybackInfo {
                item_id,
                position_ticks: ticks,
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn report_playback_start(&self, item_id: &str, ticks: i64) -> Result<(), Error> {
        self.report("/Sessions/Playing", item_id, ticks).await
    }

    pub async fn report_playback_progress(&self, item_id: &str, ticks: i64) -> Result<(), Error> {
        self.report("/Sessions/Playing/Progress", item_id, ticks)
            .await
    }

    pub async fn report_playback_stopped(&self, item_id: &str, ticks: i64) -> Result<(), Error> {
        self.report("/Sessions/Playing/Stopped", item_id, ticks)
            .await
    }

    pub async fn mark_watched(&self, item_id: &str) -> Result<(), Error> {
        self.send(
            Method::POST,
            &format!("/UserPlayedItems/{item_id}"),
            &[],
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn mark_unwatched(&self, item_id: &str) -> Result<(), Error> {
        self.send(
            Method::DELETE,
            &format!("/UserPlayedItems/{item_id}"),
            &[],
            None,
        )
        .await?;
        Ok(())
    }

    /// Media segments (intro/outro/...) of the given types. Empty types
    /// short-circuit to no segments, like jfsh.
    pub async fn get_media_segments(
        &self,
        item_id: &str,
        types: &[String],
    ) -> Result<Vec<MediaSegment>, Error> {
        if types.is_empty() {
            return Ok(Vec::new());
        }
        let query: Vec<(&str, &str)> = types
            .iter()
            .map(|t| ("includeSegmentTypes", t.as_str()))
            .collect();
        let response: SegmentsResponse = self
            .request(Method::GET, &format!("/MediaSegments/{item_id}"), &query)
            .await?;
        Ok(response.items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test against the public Jellyfin demo server; run manually with
    /// `cargo test demo_server -- --ignored`.
    #[tokio::test]
    #[ignore = "hits the public Jellyfin demo server"]
    async fn demo_server_smoke() {
        let client = Client::connect(Credentials {
            host: "https://demo.jellyfin.org/stable".into(),
            username: "demo".into(),
            password: String::new(),
            device: "isamedia-test".into(),
            device_id: "isamedia-test-device".into(),
            version: "0.0.0".into(),
            token: String::new(),
            user_id: String::new(),
        })
        .await
        .expect("connect to demo server");
        assert!(!client.token.is_empty());

        let recent = client.get_recently_added().await.expect("recently added");
        assert!(!recent.is_empty(), "demo server should have items");

        client.get_resume().await.expect("resume");
        client.get_next_up().await.expect("next up");

        if let Some(series) = recent.iter().find(|i| i.kind == models::ItemKind::Series) {
            let episodes = client.get_episodes(series).await.expect("episodes");
            assert!(!episodes.is_empty());
        }

        let bad = Client::connect(Credentials {
            host: "https://demo.jellyfin.org/stable".into(),
            username: "demo".into(),
            password: "wrong-password".into(),
            device: "isamedia-test".into(),
            device_id: "isamedia-test-device".into(),
            version: "0.0.0".into(),
            token: String::new(),
            user_id: String::new(),
        })
        .await;
        assert!(matches!(bad, Err(Error::AuthFailed)), "got {bad:?}");
    }
}
