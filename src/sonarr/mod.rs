//! Sonarr v3 REST API client (Sonarr v3 and v4 both serve /api/v3). Pure
//! HTTP, no UI dependencies. The transport (auth, timeouts, status mapping,
//! queue pagination, grabbing) is shared with Radarr in `crate::arr`; only
//! the Sonarr-specific endpoints live here.

pub mod display;
pub mod models;

use reqwest::Method;

pub use crate::arr::Error;
pub use models::{Episode, HistoryRecord, QualityProfile, QueueItem, Release, RootFolder, Series};

use crate::arr::models::Page;

#[derive(Clone, Debug)] // derived Debug is safe: Transport redacts the key
pub struct Client {
    transport: crate::arr::Transport,
}

impl Client {
    /// Build a client and validate the key with a status call; a 401/403
    /// comes back as `Error::Unauthorized` so the UI can re-prompt for it.
    pub async fn connect(host: &str, api_key: String) -> Result<Self, Error> {
        let transport = crate::arr::Transport::connect(host, api_key).await?;
        Ok(Self { transport })
    }

    pub fn host(&self) -> &str {
        &self.transport.host
    }

    pub async fn get_series(&self) -> Result<Vec<Series>, Error> {
        self.transport
            .request(Method::GET, "/api/v3/series", &[], None)
            .await
    }

    pub async fn get_episodes(&self, series_id: i64, season: i32) -> Result<Vec<Episode>, Error> {
        let series_id = series_id.to_string();
        let season = season.to_string();
        self.transport
            .request(
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

    /// The whole download queue, with episodes embedded so season markers
    /// can be derived; callers filter by series client-side.
    pub async fn get_queue(&self) -> Result<Vec<QueueItem>, Error> {
        self.transport
            .get_queue(&[("includeEpisode", "true")])
            .await
    }

    /// Look up series to add, by free text or by `tvdb:<id>`; the server
    /// parses the prefix itself, so id searches need no special handling.
    /// Hits are shaped like library series but have id 0.
    pub async fn lookup_series(&self, term: &str) -> Result<Vec<Series>, Error> {
        self.transport
            .request(
                Method::GET,
                "/api/v3/series/lookup",
                &[("term", term)],
                None,
            )
            .await
    }

    /// Add a series from a built payload and return the created series (with
    /// its new id), so the UI can open its detail straight away.
    pub async fn add_series(&self, body: &serde_json::Value) -> Result<Series, Error> {
        self.transport
            .send_json(Method::POST, "/api/v3/series", &[], Some(body), None)
            .await
    }

    pub async fn get_root_folders(&self) -> Result<Vec<RootFolder>, Error> {
        self.transport.get_root_folders().await
    }

    pub async fn get_quality_profiles(&self) -> Result<Vec<QualityProfile>, Error> {
        self.transport.get_quality_profiles().await
    }

    pub async fn search_releases(&self, episode_id: i64) -> Result<Vec<Release>, Error> {
        let episode_id = episode_id.to_string();
        self.transport
            .request(
                Method::GET,
                "/api/v3/release",
                &[("episodeId", episode_id.as_str())],
                Some(crate::arr::RELEASE_SEARCH_TIMEOUT),
            )
            .await
    }

    pub async fn grab_release(&self, guid: &str, indexer_id: i64) -> Result<(), Error> {
        self.transport.grab_release(guid, indexer_id).await
    }

    /// Kick off Sonarr's own automatic search for one episode.
    pub async fn episode_search(&self, episode_id: i64) -> Result<(), Error> {
        self.transport
            .send(
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
        self.transport
            .send(
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
    /// "grabbed before" marker in interactive search results. Paginated,
    /// unlike Radarr's /history/movie.
    pub async fn get_history(&self, episode_id: i64) -> Result<Vec<HistoryRecord>, Error> {
        let episode_id = episode_id.to_string();
        let page: Page<HistoryRecord> = self
            .transport
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
