//! Serde DTOs specific to Sonarr's v3 API (v3 and v4 both serve it). Only
//! the fields the UI reads; everything `Option`/defaulted so both server
//! versions deserialize. Shapes shared with Radarr live in
//! `crate::arr::models` and are re-exported here for the app layer.

use serde::Deserialize;

pub use crate::arr::models::{
    Command, HistoryRecord, Language, MediaInfo, QualityProfile, QualityWrapper, QueueItem,
    Release, RootFolder,
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Series {
    pub id: i64,
    /// TVDb id: the identity the add endpoint keys on. Populated on lookup
    /// results (whose `id` is 0 until added) and on library series alike.
    pub tvdb_id: Option<i64>,
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
    /// Add-time options, editable via the `o` wizard. Present on library
    /// GET /series; absent (defaulted) on lookup results, where the edit
    /// wizard never opens.
    pub quality_profile_id: i64,
    pub root_folder_path: Option<String>,
    /// "standard" | "daily" | "anime".
    pub series_type: Option<String>,
    pub season_folder: bool,
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
    /// Plot/description; returned by /episode by default. Shown by the info view.
    pub overview: Option<String>,
    /// Air date as "YYYY-MM-DD", already in the series' timezone.
    pub air_date: Option<String>,
    /// Air moment as an ISO UTC datetime; the unaired check uses this.
    pub air_date_utc: Option<String>,
    /// Runtime in minutes; sent by v4, absent on older v3.
    pub runtime: Option<i32>,
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
            "qualityProfileId": 4,
            "rootFolderPath": "/tv",
            "seriesType": "anime",
            "seasonFolder": true,
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
        assert_eq!(series.quality_profile_id, 4);
        assert_eq!(series.root_folder_path.as_deref(), Some("/tv"));
        assert_eq!(series.series_type.as_deref(), Some("anime"));
        assert!(series.season_folder);
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
            "overview": "Asta and Yuno chase the wizard king dream.",
            "airDate": "2017-11-07",
            "airDateUtc": "2017-11-07T09:25:00Z",
            "runtime": 24,
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
        assert_eq!(
            episode.overview.as_deref(),
            Some("Asta and Yuno chase the wizard king dream.")
        );
        assert_eq!(episode.runtime, Some(24));
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
        assert!(episode.overview.is_none());
        assert!(episode.runtime.is_none());
    }

    #[test]
    fn deserializes_lookup_result_without_id() {
        // A series/lookup hit for something not yet in the library: id 0,
        // but the tvdbId the add endpoint needs is present.
        let raw = r#"{
            "title": "Frieren",
            "year": 2023,
            "tvdbId": 424536,
            "overview": "The elf mage Frieren.",
            "seasons": [
                { "seasonNumber": 0 },
                { "seasonNumber": 1 }
            ],
            "ratings": { "value": 8.8 }
        }"#;
        let series: Series = serde_json::from_str(raw).unwrap();
        assert_eq!(series.id, 0);
        assert_eq!(series.tvdb_id, Some(424536));
        assert_eq!(series.seasons.len(), 2);
    }
}
