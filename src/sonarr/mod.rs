//! Sonarr v3 REST API client (Sonarr v3 and v4 both serve /api/v3). Pure
//! HTTP, no UI dependencies. The transport (auth, timeouts, status mapping,
//! queue pagination, grabbing) is shared with Radarr in `crate::arr`; only
//! the Sonarr-specific endpoints live here.

pub mod display;
pub mod models;

use std::sync::Arc;

use reqwest::Method;
use tokio::sync::Mutex;

pub use crate::arr::Error;
pub use models::{
    Command, Episode, HistoryRecord, QualityProfile, QueueItem, Release, RootFolder, Series,
};

use crate::arr::models::Page;

#[derive(Clone, Debug)] // derived Debug is safe: Transport redacts the key
pub struct Client {
    transport: crate::arr::Transport,
    /// Serialises the season-monitor read-modify-write (see
    /// [`set_season_monitored`](Self::set_season_monitored)). Shared across
    /// clones, so two quick toggles run one after the other instead of both
    /// GETting the pre-toggle series and the second PUT reverting the first.
    season_monitor_lock: Arc<Mutex<()>>,
}

impl Client {
    /// Build a client and validate the key with a status call; a 401/403
    /// comes back as `Error::Unauthorized` so the UI can re-prompt for it.
    pub async fn connect(host: &str, api_key: String) -> Result<Self, Error> {
        let transport = crate::arr::Transport::connect(host, api_key).await?;
        Ok(Self {
            transport,
            season_monitor_lock: Arc::new(Mutex::new(())),
        })
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

    /// Remove queue items by id. See `arr::Transport::delete_queue_items`.
    pub async fn delete_queue_items(
        &self,
        ids: &[i64],
        remove_from_client: bool,
        blocklist: bool,
    ) -> Result<(), Error> {
        self.transport
            .delete_queue_items(ids, remove_from_client, blocklist)
            .await
    }

    /// Look up series to add, by free text or by `tvdb:<id>`; the server
    /// parses the prefix itself, so id searches need no special handling.
    /// Returned as raw JSON: POST /series requires fields the typed `Series`
    /// doesn't carry (titleSlug, images, the full seasons array), so the add
    /// forwards the whole lookup object back rather than a hand-built subset.
    pub async fn lookup_series(&self, term: &str) -> Result<Vec<serde_json::Value>, Error> {
        self.transport
            .request(
                Method::GET,
                "/api/v3/series/lookup",
                &[("term", term)],
                None,
            )
            .await
    }

    /// Add a series from a lookup object augmented with the user's choices,
    /// and return the created series (with its new id) so the UI can open its
    /// detail straight away.
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

    /// Kick off Sonarr's automatic search across a whole series (every
    /// monitored missing episode). Returns the created command so a background
    /// monitor can poll it to completion (see [`get_command`](Self::get_command)).
    pub async fn series_search(&self, series_id: i64) -> Result<Command, Error> {
        self.transport
            .send_json(
                Method::POST,
                "/api/v3/command",
                &[],
                Some(&serde_json::json!({ "name": "SeriesSearch", "seriesId": series_id })),
                None,
            )
            .await
    }

    /// Poll one command's status by id; see `arr::Transport::get_command`.
    pub async fn get_command(&self, command_id: i64) -> Result<Command, Error> {
        self.transport.get_command(command_id).await
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

    /// Remove a series from the library, including every season and episode.
    /// `delete_files` also deletes its files from disk; `add_import_exclusion`
    /// adds an import-list exclusion so a list can't re-add it. Both ride as
    /// query params, like the delete-queue endpoint (see
    /// `arr::Transport::delete_queue_items`).
    pub async fn delete_series(
        &self,
        series_id: i64,
        delete_files: bool,
        add_import_exclusion: bool,
    ) -> Result<(), Error> {
        self.transport
            .send(
                Method::DELETE,
                &format!("/api/v3/series/{series_id}"),
                &[
                    ("deleteFiles", if delete_files { "true" } else { "false" }),
                    (
                        "addImportListExclusion",
                        if add_import_exclusion {
                            "true"
                        } else {
                            "false"
                        },
                    ),
                ],
                None,
                None,
            )
            .await?;
        Ok(())
    }

    /// Toggle a series' monitored flag via the bulk editor endpoint, which
    /// takes only the ids and the changed field (the `Series` model is
    /// deserialize-only, so we can't PUT the whole resource back).
    pub async fn set_series_monitored(&self, series_id: i64, monitored: bool) -> Result<(), Error> {
        let body = serde_json::json!({ "seriesIds": [series_id], "monitored": monitored });
        self.transport
            .send(Method::PUT, "/api/v3/series/editor", &[], Some(&body), None)
            .await?;
        Ok(())
    }

    /// Edit a series' add-time options via the same bulk editor endpoint. Only
    /// the changed fields are sent (a `None` leaves that option untouched);
    /// `move_files` rides only when `root_folder_path` is set, telling Sonarr
    /// whether to relocate existing files on disk to the new root.
    pub async fn edit_series_options(
        &self,
        series_id: i64,
        root_folder_path: Option<&str>,
        move_files: bool,
        quality_profile_id: Option<i64>,
        series_type: Option<&str>,
        season_folder: Option<bool>,
    ) -> Result<(), Error> {
        let body = series_editor_body(
            series_id,
            root_folder_path,
            move_files,
            quality_profile_id,
            series_type,
            season_folder,
        );
        self.transport
            .send(Method::PUT, "/api/v3/series/editor", &[], Some(&body), None)
            .await?;
        Ok(())
    }

    /// Toggle a season's monitored flag. Season monitoring lives inside the
    /// series object and there is no per-season editor, so fetch the series as
    /// raw JSON, flip the matching season, and PUT it back.
    ///
    /// The whole read-modify-write runs under `season_monitor_lock`: without
    /// it, toggling two seasons in quick succession spawns two overlapping
    /// GET-modify-PUT cycles, and the second GET can read the pre-first-PUT
    /// series and revert the first season. The endpoint offers no optimistic
    /// concurrency (no ETag), so serialising the cycles is the only way to
    /// avoid the lost update.
    pub async fn set_season_monitored(
        &self,
        series_id: i64,
        season_number: i32,
        monitored: bool,
    ) -> Result<(), Error> {
        let _guard = self.season_monitor_lock.lock().await;
        let mut series: serde_json::Value = self
            .transport
            .request(
                Method::GET,
                &format!("/api/v3/series/{series_id}"),
                &[],
                None,
            )
            .await?;
        if let Some(seasons) = series.get_mut("seasons").and_then(|s| s.as_array_mut()) {
            for season in seasons {
                if season.get("seasonNumber").and_then(|n| n.as_i64())
                    == Some(i64::from(season_number))
                    && let Some(obj) = season.as_object_mut()
                {
                    obj.insert("monitored".into(), serde_json::Value::Bool(monitored));
                }
            }
        }
        self.transport
            .send(
                Method::PUT,
                &format!("/api/v3/series/{series_id}"),
                &[],
                Some(&series),
                None,
            )
            .await?;
        Ok(())
    }

    /// Toggle one episode's monitored flag via the dedicated bulk endpoint.
    pub async fn set_episode_monitored(
        &self,
        episode_id: i64,
        monitored: bool,
    ) -> Result<(), Error> {
        let body = serde_json::json!({ "episodeIds": [episode_id], "monitored": monitored });
        self.transport
            .send(
                Method::PUT,
                "/api/v3/episode/monitor",
                &[],
                Some(&body),
                None,
            )
            .await?;
        Ok(())
    }

    /// Past grab/import/delete events of one episode, for the
    /// "grabbed before" marker in interactive search results. Sonarr's
    /// `/history` is paginated (unlike Radarr's `/history/movie`), so follow the
    /// pages to completion; a single page would drop the marker on episodes
    /// with a long history. Bounded by `MAX_PAGES` so a runaway `totalRecords`
    /// can't loop forever.
    pub async fn get_history(&self, episode_id: i64) -> Result<Vec<HistoryRecord>, Error> {
        const PAGE_SIZE: usize = 100;
        const MAX_PAGES: usize = 20;
        let episode_id = episode_id.to_string();
        let page_size = PAGE_SIZE.to_string();
        let mut records: Vec<HistoryRecord> = Vec::new();
        for page in 1..=MAX_PAGES {
            let page = page.to_string();
            let response: Page<HistoryRecord> = self
                .transport
                .request(
                    Method::GET,
                    "/api/v3/history",
                    &[
                        ("episodeId", episode_id.as_str()),
                        ("page", page.as_str()),
                        ("pageSize", page_size.as_str()),
                    ],
                    None,
                )
                .await?;
            let fetched = response.records.len();
            records.extend(response.records);
            // Stop on a short/last page or once every record is in hand.
            if fetched < PAGE_SIZE || records.len() as i64 >= response.total_records {
                return Ok(records);
            }
        }
        tracing::debug!(
            episode_id = %episode_id,
            "history exceeded {MAX_PAGES} pages; older grab markers may be missing"
        );
        Ok(records)
    }
}

/// Build the `series/editor` bulk-update body: always the id, plus only the
/// fields that were supplied. `moveFiles` rides only alongside `rootFolderPath`,
/// so an edit that doesn't change the root never asks Sonarr to relocate files.
fn series_editor_body(
    series_id: i64,
    root_folder_path: Option<&str>,
    move_files: bool,
    quality_profile_id: Option<i64>,
    series_type: Option<&str>,
    season_folder: Option<bool>,
) -> serde_json::Value {
    let mut body = serde_json::json!({ "seriesIds": [series_id] });
    let object = body
        .as_object_mut()
        .expect("json! object literal is always an object");
    if let Some(path) = root_folder_path {
        object.insert("rootFolderPath".into(), serde_json::json!(path));
        object.insert("moveFiles".into(), serde_json::json!(move_files));
    }
    if let Some(id) = quality_profile_id {
        object.insert("qualityProfileId".into(), serde_json::json!(id));
    }
    if let Some(kind) = series_type {
        object.insert("seriesType".into(), serde_json::json!(kind));
    }
    if let Some(season_folder) = season_folder {
        object.insert("seasonFolder".into(), serde_json::json!(season_folder));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_body_sends_only_supplied_fields() {
        // Quality only: no root, so neither rootFolderPath nor moveFiles ride.
        assert_eq!(
            series_editor_body(5, None, true, Some(7), None, None),
            serde_json::json!({ "seriesIds": [5], "qualityProfileId": 7 })
        );
        // Root change carries moveFiles alongside it.
        assert_eq!(
            series_editor_body(5, Some("/tv4k"), true, None, None, None),
            serde_json::json!({
                "seriesIds": [5],
                "rootFolderPath": "/tv4k",
                "moveFiles": true,
            })
        );
        // Series-type and season-folder ride independently of the root.
        assert_eq!(
            series_editor_body(5, None, false, None, Some("anime"), Some(false)),
            serde_json::json!({
                "seriesIds": [5],
                "seriesType": "anime",
                "seasonFolder": false,
            })
        );
        // Nothing supplied: just the id (the caller guards against this).
        assert_eq!(
            series_editor_body(9, None, false, None, None, None),
            serde_json::json!({ "seriesIds": [9] })
        );
    }
}
