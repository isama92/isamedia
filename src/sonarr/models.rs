//! Serde DTOs for the Sonarr v3 API (used by Sonarr v3 and v4). Only the
//! fields the UI reads. Everything is `Option`/defaulted so payload
//! differences between server versions deserialize instead of erroring;
//! v3/v4 splits (single `language` vs `languages` list) carry both fields.

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Series {
    pub id: i64,
    pub title: Option<String>,
    pub sort_title: Option<String>,
    pub overview: Option<String>,
    pub year: Option<i32>,
    pub monitored: bool,
    /// ISO datetime the series was added to Sonarr.
    pub added: Option<String>,
    pub next_airing: Option<String>,
    pub ratings: Option<Ratings>,
    pub seasons: Vec<Season>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Ratings {
    pub value: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Season {
    pub season_number: i32,
    pub monitored: bool,
    pub statistics: Option<SeasonStatistics>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SeasonStatistics {
    pub episode_file_count: i32,
    pub episode_count: i32,
    pub total_episode_count: i32,
    pub size_on_disk: i64,
    pub next_airing: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Episode {
    pub id: i64,
    pub series_id: i64,
    pub season_number: i32,
    pub episode_number: i32,
    pub title: Option<String>,
    /// Air date as "YYYY-MM-DD", already in the series' timezone.
    pub air_date: Option<String>,
    /// Air moment as an ISO UTC datetime; the unaired check uses this.
    pub air_date_utc: Option<String>,
    pub has_file: bool,
    pub monitored: bool,
    pub episode_file_id: i64,
    /// Present when the episode list is requested with includeEpisodeFile.
    pub episode_file: Option<EpisodeFile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EpisodeFile {
    pub id: i64,
    pub path: Option<String>,
    pub size: i64,
    pub quality: Option<QualityWrapper>,
    /// v4 sends a list; v3 sends a single `language` object instead.
    pub languages: Vec<Language>,
    pub language: Option<Language>,
    pub media_info: Option<MediaInfo>,
}

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
    /// v4 only; absent on v3.
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

/// Envelope of Sonarr's paginated endpoints (queue, history).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Page<T> {
    pub total_records: i64,
    pub records: Vec<T>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueueItem {
    pub id: i64,
    pub series_id: Option<i64>,
    pub episode_id: Option<i64>,
    /// Bytes, decimal in the payload.
    pub size: f64,
    pub sizeleft: f64,
    pub status: Option<String>,
    pub title: Option<String>,
    /// Present when the queue is requested with includeEpisode; carries the
    /// season number for the per-season downloading marker.
    pub episode: Option<QueueEpisode>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueueEpisode {
    pub id: i64,
    pub season_number: i32,
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
    /// v4 sends a list; v3 sends a single `language` object instead.
    pub languages: Vec<Language>,
    pub language: Option<Language>,
    pub quality: Option<QualityWrapper>,
    pub rejected: bool,
    pub rejections: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryRecord {
    pub episode_id: i64,
    /// The release title the event was recorded under.
    pub source_title: Option<String>,
    /// "grabbed", "downloadFolderImported", "episodeFileDeleted", ...
    pub event_type: Option<String>,
    /// Free-form per-event details; grabbed events carry the release guid.
    pub data: std::collections::HashMap<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_series_with_seasons() {
        let raw = r#"{
            "id": 5,
            "title": "Black Clover",
            "sortTitle": "black clover",
            "overview": "Asta and Yuno were abandoned together.",
            "year": 2017,
            "monitored": true,
            "added": "2020-01-05T18:22:00Z",
            "nextAiring": "2026-07-12T14:25:00Z",
            "ratings": { "votes": 512, "value": 7.9 },
            "seasons": [
                { "seasonNumber": 0, "monitored": false },
                {
                    "seasonNumber": 1,
                    "monitored": true,
                    "statistics": {
                        "episodeFileCount": 10,
                        "episodeCount": 12,
                        "totalEpisodeCount": 51,
                        "sizeOnDisk": 15032385536,
                        "percentOfEpisodes": 83.3,
                        "nextAiring": "2026-07-12T14:25:00Z"
                    }
                }
            ]
        }"#;
        let series: Series = serde_json::from_str(raw).unwrap();
        assert_eq!(series.id, 5);
        assert_eq!(series.title.as_deref(), Some("Black Clover"));
        assert_eq!(series.year, Some(2017));
        assert!((series.ratings.unwrap().value - 7.9).abs() < f64::EPSILON);
        assert_eq!(series.seasons.len(), 2);
        assert!(series.seasons[0].statistics.is_none());
        let stats = series.seasons[1].statistics.as_ref().unwrap();
        assert_eq!(stats.episode_file_count, 10);
        assert_eq!(stats.episode_count, 12);
    }

    #[test]
    fn deserializes_episode_with_and_without_file() {
        let with_file = r#"{
            "id": 101,
            "seriesId": 5,
            "seasonNumber": 1,
            "episodeNumber": 5,
            "title": "The Path to the Wizard King",
            "airDate": "2017-11-07",
            "airDateUtc": "2017-11-07T09:25:00Z",
            "hasFile": true,
            "monitored": true,
            "episodeFileId": 88,
            "episodeFile": {
                "id": 88,
                "path": "/tv/Black Clover/Season 01/S01E05.mkv",
                "size": 1503238553,
                "quality": { "quality": { "id": 7, "name": "Bluray-1080p" } },
                "languages": [ { "id": 8, "name": "Japanese" } ],
                "mediaInfo": {
                    "audioChannels": 2.0,
                    "audioCodec": "FLAC",
                    "audioLanguages": "jpn",
                    "videoCodec": "x264",
                    "resolution": "1920x1080",
                    "runTime": "23:41",
                    "subtitles": "eng"
                }
            }
        }"#;
        let episode: Episode = serde_json::from_str(with_file).unwrap();
        assert!(episode.has_file);
        let file = episode.episode_file.as_ref().unwrap();
        assert_eq!(
            file.quality.as_ref().unwrap().quality.name.as_deref(),
            Some("Bluray-1080p")
        );
        assert_eq!(file.languages[0].name.as_deref(), Some("Japanese"));
        let info = file.media_info.as_ref().unwrap();
        assert_eq!(info.audio_channels, Some(2.0));
        assert!(
            info.video_dynamic_range.is_none(),
            "v3 payload has no dynamic range"
        );

        let missing = r#"{
            "id": 102,
            "seriesId": 5,
            "seasonNumber": 1,
            "episodeNumber": 6,
            "title": "The Black Bulls",
            "hasFile": false,
            "monitored": true,
            "episodeFileId": 0
        }"#;
        let episode: Episode = serde_json::from_str(missing).unwrap();
        assert!(!episode.has_file);
        assert!(episode.episode_file.is_none());
        assert!(episode.air_date_utc.is_none());
    }

    #[test]
    fn deserializes_queue_page() {
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
                    "episode": { "id": 102, "seasonNumber": 1 }
                }
            ]
        }"#;
        let page: Page<QueueItem> = serde_json::from_str(raw).unwrap();
        assert_eq!(page.total_records, 1);
        let item = &page.records[0];
        assert_eq!(item.episode_id, Some(102));
        assert_eq!(item.episode.as_ref().unwrap().season_number, 1);
        assert!(item.sizeleft < item.size);
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
    fn deserializes_usenet_release_with_v3_single_language() {
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
        assert_eq!(record.episode_id, 102);
        assert_eq!(record.event_type.as_deref(), Some("grabbed"));
        assert_eq!(
            record.data.get("guid").and_then(|v| v.as_str()),
            Some("magnet-abc")
        );
    }
}
