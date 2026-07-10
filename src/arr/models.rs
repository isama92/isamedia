//! Serde DTOs whose shape is identical across Sonarr and Radarr (v3 API).
//! Only the fields the UI reads. Everything is `Option`/defaulted so payload
//! differences between server versions deserialize instead of erroring;
//! version splits (single `language` vs `languages` list) carry both fields.

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QualityWrapper {
    pub quality: Quality,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Quality {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Language {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaInfo {
    pub video_codec: Option<String>,
    /// e.g. "1920x1080".
    pub resolution: Option<String>,
    /// Newer servers only; absent on older v3.
    pub video_dynamic_range: Option<String>,
    pub audio_codec: Option<String>,
    /// Decimal in the payload (5.1 channels), so not an integer type.
    pub audio_channels: Option<f64>,
    /// Slash-separated language codes, e.g. "eng/jpn".
    pub audio_languages: Option<String>,
    pub subtitles: Option<String>,
    /// e.g. "23:41".
    pub run_time: Option<String>,
}

/// Envelope of the paginated endpoints (queue, Sonarr history).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Page<T> {
    pub total_records: i64,
    pub records: Vec<T>,
}

/// One download in the queue. A superset of both backends: Sonarr fills the
/// series/episode fields, Radarr fills `movie_id`; the other side stays None.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueueItem {
    pub id: i64,
    pub series_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub movie_id: Option<i64>,
    /// Bytes, decimal in the payload.
    pub size: f64,
    /// Marked obsolete upstream; still serialized by every current release.
    /// If a future server drops it, progress reads 100% — cosmetic only.
    pub sizeleft: f64,
    pub status: Option<String>,
    pub title: Option<String>,
    /// Both backends carry the release languages inline on the queue record.
    pub languages: Vec<Language>,
    pub quality: Option<QualityWrapper>,
    /// Remaining time, "HH:MM:SS" or "d.HH:MM:SS"; absent once finished or
    /// while stalled.
    pub timeleft: Option<String>,
    /// The download lifecycle stage ("downloading", "importing", ...) —
    /// finer-grained than `status`.
    pub tracked_download_state: Option<String>,
    /// "ok" | "warning" | "error"; drives the warning marker in the view.
    pub tracked_download_status: Option<String>,
    /// Present when a Sonarr queue is requested with includeEpisode; carries
    /// the season number for the per-season downloading marker plus the
    /// number/title the Downloads view renders.
    pub episode: Option<QueueEpisode>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueueEpisode {
    pub id: i64,
    pub season_number: i32,
    pub episode_number: i32,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Release {
    pub guid: String,
    pub indexer_id: i64,
    pub title: Option<String>,
    /// Age in whole days.
    pub age: i64,
    /// Bytes.
    pub size: i64,
    pub indexer: Option<String>,
    /// "torrent" or "usenet"; seeders/leechers only mean anything on torrents.
    pub protocol: Option<String>,
    pub seeders: Option<i32>,
    pub leechers: Option<i32>,
    /// Newer servers send a list; older ones a single `language` object.
    pub languages: Vec<Language>,
    pub language: Option<Language>,
    pub quality: Option<QualityWrapper>,
    pub rejected: bool,
    pub rejections: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryRecord {
    /// The release title the event was recorded under.
    pub source_title: Option<String>,
    /// "grabbed", "downloadFolderImported", ...
    pub event_type: Option<String>,
    /// Free-form per-event details; grabbed events carry the release guid.
    pub data: std::collections::HashMap<String, serde_json::Value>,
}

/// A configured library root, offered as a target when adding a new item.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RootFolder {
    pub path: String,
    pub accessible: bool,
}

/// A named quality profile the add flow lets the user pick by name.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QualityProfile {
    pub id: i64,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_sonarr_queue_page() {
        let raw = r#"{
            "page": 1,
            "pageSize": 1000,
            "totalRecords": 1,
            "records": [
                {
                    "id": 900,
                    "seriesId": 5,
                    "episodeId": 102,
                    "size": 1500000000.0,
                    "sizeleft": 870000000.0,
                    "status": "downloading",
                    "title": "Black.Clover.S01E06.1080p.WEB.H264",
                    "timeleft": "00:12:30",
                    "trackedDownloadState": "downloading",
                    "trackedDownloadStatus": "ok",
                    "languages": [ { "id": 1, "name": "English" } ],
                    "quality": { "quality": { "id": 3, "name": "WEBDL-1080p" } },
                    "episode": { "id": 102, "seasonNumber": 1, "episodeNumber": 6, "title": "The Man Who Cuts Death" }
                }
            ]
        }"#;
        let page: Page<QueueItem> = serde_json::from_str(raw).unwrap();
        assert_eq!(page.total_records, 1);
        let item = &page.records[0];
        assert_eq!(item.episode_id, Some(102));
        assert_eq!(item.movie_id, None);
        let episode = item.episode.as_ref().unwrap();
        assert_eq!(episode.season_number, 1);
        assert_eq!(episode.episode_number, 6);
        assert_eq!(episode.title.as_deref(), Some("The Man Who Cuts Death"));
        assert_eq!(item.timeleft.as_deref(), Some("00:12:30"));
        assert_eq!(
            item.languages.first().and_then(|l| l.name.as_deref()),
            Some("English")
        );
        assert_eq!(
            item.quality
                .as_ref()
                .and_then(|q| q.quality.name.as_deref()),
            Some("WEBDL-1080p")
        );
        assert!(item.sizeleft < item.size);
    }

    #[test]
    fn deserializes_radarr_queue_page() {
        let raw = r#"{
            "page": 1,
            "pageSize": 1000,
            "totalRecords": 1,
            "records": [
                {
                    "id": 901,
                    "movieId": 42,
                    "size": 32000000000.0,
                    "sizeleft": 8000000000.0,
                    "status": "downloading",
                    "title": "Interstellar.2014.2160p.BluRay.x265",
                    "timeleft": "01:42:07",
                    "languages": [ { "id": 1, "name": "English" } ],
                    "quality": { "quality": { "id": 19, "name": "Bluray-2160p" } }
                }
            ]
        }"#;
        let page: Page<QueueItem> = serde_json::from_str(raw).unwrap();
        let item = &page.records[0];
        assert_eq!(item.movie_id, Some(42));
        assert_eq!(item.series_id, None);
        assert_eq!(item.episode_id, None);
        assert!(item.episode.is_none());
        assert_eq!(item.timeleft.as_deref(), Some("01:42:07"));
        assert_eq!(
            item.quality
                .as_ref()
                .and_then(|q| q.quality.name.as_deref()),
            Some("Bluray-2160p")
        );
        assert_eq!(
            item.languages.first().and_then(|l| l.name.as_deref()),
            Some("English")
        );
    }

    #[test]
    fn deserializes_torrent_release_with_rejections() {
        let raw = r#"{
            "guid": "magnet-abc",
            "indexerId": 2,
            "title": "[SubsPlease] Black Clover - 05 (1080p)",
            "age": 748,
            "size": 1503238553,
            "indexer": "Nyaa",
            "protocol": "torrent",
            "seeders": 142,
            "leechers": 7,
            "languages": [ { "id": 8, "name": "Japanese" } ],
            "quality": { "quality": { "id": 9, "name": "HDTV-1080p" } },
            "rejected": true,
            "rejections": [ "Not an upgrade for existing episode file(s)" ]
        }"#;
        let release: Release = serde_json::from_str(raw).unwrap();
        assert!(release.rejected);
        assert_eq!(release.rejections.len(), 1);
        assert_eq!(release.seeders, Some(142));
        assert_eq!(release.indexer_id, 2);
    }

    #[test]
    fn deserializes_usenet_release_with_single_language() {
        let raw = r#"{
            "guid": "nzb-def",
            "indexerId": 1,
            "title": "Black.Clover.S01E05.1080p.BluRay.x264",
            "age": 1155,
            "size": 1395864371,
            "indexer": "NZBgeek",
            "protocol": "usenet",
            "seeders": null,
            "leechers": null,
            "language": { "id": 8, "name": "Japanese" },
            "quality": { "quality": { "id": 7, "name": "Bluray-1080p" } },
            "rejected": false,
            "rejections": []
        }"#;
        let release: Release = serde_json::from_str(raw).unwrap();
        assert!(!release.rejected);
        assert_eq!(release.seeders, None);
        assert!(release.languages.is_empty());
        assert_eq!(release.language.unwrap().name.as_deref(), Some("Japanese"));
    }

    #[test]
    fn deserializes_history_record() {
        let raw = r#"{
            "episodeId": 102,
            "seriesId": 5,
            "sourceTitle": "Black.Clover.S01E06.1080p.WEB.H264",
            "eventType": "grabbed",
            "date": "2026-07-01T10:00:00Z",
            "data": {
                "guid": "magnet-abc",
                "indexer": "Nyaa",
                "age": "748"
            }
        }"#;
        let record: HistoryRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(record.event_type.as_deref(), Some("grabbed"));
        assert_eq!(
            record.data.get("guid").and_then(|v| v.as_str()),
            Some("magnet-abc")
        );
    }

    #[test]
    fn deserializes_add_prerequisites() {
        let folders = r#"[
            { "id": 1, "path": "/movies", "accessible": true, "freeSpace": 1000 },
            { "id": 2, "path": "/movies-4k", "accessible": false }
        ]"#;
        let folders: Vec<RootFolder> = serde_json::from_str(folders).unwrap();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].path, "/movies");
        assert!(folders[0].accessible);
        assert!(!folders[1].accessible);

        let profiles = r#"[
            { "id": 4, "name": "HD-1080p", "items": [] },
            { "id": 6, "name": "Any" }
        ]"#;
        let profiles: Vec<QualityProfile> = serde_json::from_str(profiles).unwrap();
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].id, 4);
        assert_eq!(profiles[1].name, "Any");
    }
}
