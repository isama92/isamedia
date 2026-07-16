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
    #[error("unexpected response from server (not valid JSON)")]
    Decode(#[from] serde_json::Error),
}

/// What opening a library lists, as (includeItemTypes, recursive): movie and
/// show libraries recurse into their folders for a single item type, any
/// other library lists its direct children (e.g. the collections). Item
/// counts use the same scope so the number matches what opening shows.
pub fn library_scope(collection_type: Option<&str>) -> (Option<&'static str>, bool) {
    match collection_type {
        Some("movies") => (Some("Movie"), true),
        Some("tvshows") => (Some("Series"), true),
        _ => (None, false),
    }
}

/// Parameters for one page of an `/Items` listing under a parent (a library
/// view or a collection). Owned strings so a query built on the render
/// thread moves cleanly into the spawned request task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryItemsQuery {
    pub parent_id: String,
    /// e.g. `Some("Movie")` / `Some("Series")`; `None` lists all children.
    pub include_item_types: Option<&'static str>,
    /// Recurse into folders (movie/show libraries); collections list their
    /// direct children.
    pub recursive: bool,
    pub start_index: usize,
    pub limit: usize,
    /// "SortName" | "DateCreated" | "PremiereDate".
    pub sort_by: &'static str,
    /// "Ascending" | "Descending".
    pub sort_order: &'static str,
    pub search_term: Option<String>,
}

/// Everything needed to (re)connect: the jellyfin config section plus the
/// secrets fetched from the OS keyring.
#[derive(Clone)]
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

// Hand-written so `{:?}` can never leak a secret into a log, no matter how
// carelessly a Credentials/Client ends up in a tracing call.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("host", &self.host)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("device", &self.device)
            .field("device_id", &self.device_id)
            .field("version", &self.version)
            .field("token", &"<redacted>")
            .field("user_id", &self.user_id)
            .finish()
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    pub host: String,
    pub user_id: String,
    pub token: String,
    auth_header: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("host", &self.host)
            .field("user_id", &self.user_id)
            .field("token", &"<redacted>")
            .field("auth_header", &"<redacted>")
            .finish_non_exhaustive()
    }
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
        // Every request path needs a timeout: an unbounded call can never be
        // cancelled from the render loop. All calls here are small JSON
        // (media streaming goes through mpv, not this client), so a blanket
        // per-request timeout is safe.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;

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
            // Read the body then deserialize separately so a proxy answering
            // HTML with 200 surfaces as `Decode`, not `Http` ("cannot reach
            // server"). A transport failure on `bytes()` is still `Http`.
            let bytes = response.bytes().await?;
            let auth: AuthResponse = serde_json::from_slice(&bytes)?;
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

    /// Send a request and map error statuses; 401 or 403 becomes
    /// `Error::Unauthorized` so the UI can drop back to the login screen. A
    /// 403 on an authenticated request means the session token was revoked,
    /// which must re-prompt like a 401 rather than surface as a dead-end
    /// "server returned 403" (the `connect` path maps 403 to `AuthFailed`,
    /// the correct "wrong credentials" outcome for a fresh login).
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
    ) -> Result<T, Error> {
        let response = self.send(method, path, query, None).await?;
        // Read the body then deserialize separately: a decode failure (e.g. a
        // reverse proxy returning HTML with 200) becomes `Decode`, not `Http`,
        // so it does not masquerade as a network failure. A transport error on
        // `bytes()` is still `Http`.
        let bytes = response.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
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
                ("fields", "MediaStreams,DateCreated,Overview,Genres"),
            ],
        )
        .await
    }

    pub async fn get_next_up(&self) -> Result<Vec<MediaItem>, Error> {
        self.get_items(
            "/Shows/NextUp",
            &[
                ("fields", "MediaStreams,Overview,Genres"),
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
                ("fields", "MediaStreams,DateCreated,Overview,Genres"),
                ("limit", "100"),
                ("sortBy", "DateCreated"),
                ("sortOrder", "Descending"),
            ],
        )
        .await
    }

    /// A single item by id, with the fields the show header needs. Opening a
    /// show from a hub episode starts with only the series id, not the series
    /// item itself, so it is fetched here. Unlike the list endpoints this
    /// returns a bare item, not an `ItemsResponse`.
    pub async fn get_item(&self, id: &str) -> Result<MediaItem, Error> {
        self.request(
            Method::GET,
            &format!("/Users/{}/Items/{id}", self.user_id),
            &[("fields", "Overview,Genres")],
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
                ("fields", "MediaStreams,DateCreated,Overview,Genres"),
                ("limit", "100"),
            ],
        )
        .await
    }

    /// The user's library views (Movies, Shows, Collections, ...), with
    /// `child_count` replaced by a real count per view. The ChildCount that
    /// /Views itself returns is unreliable (it can disagree with the item
    /// list and vary between calls), so each view gets a limit=0 count
    /// request instead, all issued concurrently.
    pub async fn get_libraries(&self) -> Result<ItemsResponse, Error> {
        let mut response: ItemsResponse = self
            .request(Method::GET, &format!("/Users/{}/Views", self.user_id), &[])
            .await?;
        let handles: Vec<_> = response
            .items
            .iter()
            .map(|item| {
                let client = self.clone();
                let parent_id = item.id.clone();
                let collection_type = item.collection_type.clone();
                tokio::spawn(async move {
                    client
                        .count_library_items(&parent_id, collection_type.as_deref())
                        .await
                })
            })
            .collect();
        for (item, handle) in response.items.iter_mut().zip(handles) {
            // A failed count is cosmetic (the row just shows no number);
            // the view list itself already loaded fine.
            item.child_count = match handle.await {
                Ok(Ok(count)) => Some(count),
                Ok(Err(err)) => {
                    tracing::debug!(%err, "library count request failed");
                    None
                }
                Err(err) => {
                    tracing::debug!(%err, "library count task failed");
                    None
                }
            };
        }
        Ok(response)
    }

    /// Number of items opening this library would list, read from the
    /// server-side TotalRecordCount. `limit=1` rather than `limit=0`: the
    /// one throwaway item is cheap, and 0 is too easily reinterpreted as
    /// "no limit" by a future server version.
    async fn count_library_items(
        &self,
        parent_id: &str,
        collection_type: Option<&str>,
    ) -> Result<i32, Error> {
        let (include_item_types, recursive) = library_scope(collection_type);
        let mut params: Vec<(&str, &str)> = vec![
            ("parentId", parent_id),
            ("recursive", if recursive { "true" } else { "false" }),
            ("limit", "1"),
        ];
        if let Some(types) = include_item_types {
            params.push(("includeItemTypes", types));
        }
        let response: ItemsResponse = self.request(Method::GET, "/Items", &params).await?;
        Ok(response.total_record_count.clamp(0, i32::MAX as i64) as i32)
    }

    /// One page of items under a library or collection. Returns the full
    /// response (not just the items) because the caller needs
    /// `total_record_count` to drive pagination.
    pub async fn get_library_items(
        &self,
        query: &LibraryItemsQuery,
    ) -> Result<ItemsResponse, Error> {
        let start_index = query.start_index.to_string();
        let limit = query.limit.to_string();
        let mut params: Vec<(&str, &str)> = vec![
            ("parentId", query.parent_id.as_str()),
            ("recursive", if query.recursive { "true" } else { "false" }),
            ("startIndex", start_index.as_str()),
            ("limit", limit.as_str()),
            ("sortBy", query.sort_by),
            ("sortOrder", query.sort_order),
            (
                "fields",
                "MediaStreams,DateCreated,ChildCount,Overview,Genres",
            ),
        ];
        if let Some(types) = query.include_item_types {
            params.push(("includeItemTypes", types));
        }
        if let Some(term) = &query.search_term {
            params.push(("searchTerm", term.as_str()));
        }
        self.request(Method::GET, "/Items", &params).await
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
            &[("fields", "MediaStreams,Overview,Genres")],
        )
        .await
    }

    async fn report(
        &self,
        path: &str,
        item_id: &str,
        ticks: i64,
        is_paused: bool,
    ) -> Result<(), Error> {
        self.send(
            Method::POST,
            path,
            &[],
            Some(&PlaybackInfo {
                item_id,
                position_ticks: ticks,
                is_paused,
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn report_playback_start(&self, item_id: &str, ticks: i64) -> Result<(), Error> {
        self.report("/Sessions/Playing", item_id, ticks, false)
            .await
    }

    pub async fn report_playback_progress(
        &self,
        item_id: &str,
        ticks: i64,
        is_paused: bool,
    ) -> Result<(), Error> {
        self.report("/Sessions/Playing/Progress", item_id, ticks, is_paused)
            .await
    }

    pub async fn report_playback_stopped(&self, item_id: &str, ticks: i64) -> Result<(), Error> {
        self.report("/Sessions/Playing/Stopped", item_id, ticks, false)
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

    #[test]
    fn library_scope_matches_what_opening_lists() {
        assert_eq!(library_scope(Some("movies")), (Some("Movie"), true));
        assert_eq!(library_scope(Some("tvshows")), (Some("Series"), true));
        assert_eq!(library_scope(Some("boxsets")), (None, false));
        assert_eq!(library_scope(Some("music")), (None, false));
        assert_eq!(library_scope(None), (None, false));
    }

    #[test]
    fn debug_never_prints_secrets() {
        let creds = Credentials {
            host: "https://jf.example.com".into(),
            username: "me".into(),
            password: "hunter2".into(),
            device: "box".into(),
            device_id: "dev-1".into(),
            version: "0.1.0".into(),
            token: "sekret-token".into(),
            user_id: "uid".into(),
        };
        let debug = format!("{creds:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(!debug.contains("sekret-token"), "{debug}");
        assert!(debug.contains("<redacted>"));
        // The username/host still print, which is what makes Debug useful.
        assert!(debug.contains("jf.example.com"));

        let client = Client {
            http: reqwest::Client::new(),
            host: "https://jf.example.com".into(),
            user_id: "uid".into(),
            token: "sekret-token".into(),
            auth_header: "MediaBrowser Token=\"sekret-token\"".into(),
        };
        let debug = format!("{client:?}");
        assert!(!debug.contains("sekret-token"), "{debug}");
    }

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

        let libraries = client.get_libraries().await.expect("libraries");
        assert!(!libraries.items.is_empty(), "demo server should have views");
        assert!(
            libraries.items.iter().all(|lib| lib.child_count.is_some()),
            "every view should get a real item count"
        );
        let page = client
            .get_library_items(&LibraryItemsQuery {
                parent_id: libraries.items[0].id.clone(),
                include_item_types: None,
                recursive: true,
                start_index: 0,
                limit: 5,
                sort_by: "DateCreated",
                sort_order: "Descending",
                search_term: None,
            })
            .await
            .expect("library items");
        assert!(page.total_record_count >= page.items.len() as i64);

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
