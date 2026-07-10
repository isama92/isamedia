//! Serde DTOs specific to Radarr's v3 API. Only the fields the UI reads;
//! everything `Option`/defaulted so server versions deserialize. Shapes
//! shared with Sonarr live in `crate::arr::models` and are re-exported here
//! for the app layer.

use serde::Deserialize;

pub use crate::arr::models::{
    Command, HistoryRecord, Language, MediaInfo, QualityProfile, QualityWrapper, QueueItem,
    Release, RootFolder,
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Movie {
    pub id: i64,
    /// TMDb id: the identity the add endpoint keys on. Populated on lookup
    /// results (whose `id` is 0 until added) and on library movies alike.
    pub tmdb_id: Option<i64>,
    pub title: Option<String>,
    pub sort_title: Option<String>,
    pub overview: Option<String>,
    pub year: Option<i32>,
    pub monitored: bool,
    /// ISO datetime the movie was added to Radarr.
    pub added: Option<String>,
    /// "tba" | "announced" | "inCinemas" | "released" | "deleted".
    pub status: Option<String>,
    pub in_cinemas: Option<String>,
    pub physical_release: Option<String>,
    pub digital_release: Option<String>,
    /// Server-computed earliest release; newer servers only.
    pub release_date: Option<String>,
    /// Minutes; 0 means unknown.
    pub runtime: i64,
    pub has_file: bool,
    pub movie_file_id: i64,
    pub size_on_disk: Option<i64>,
    /// Keyed by source, unlike Sonarr's flat ratings object.
    pub ratings: Option<Ratings>,
    /// Embedded by GET /movie whenever the movie has a file.
    pub movie_file: Option<MovieFile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Ratings {
    pub tmdb: Option<Rating>,
    pub imdb: Option<Rating>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Rating {
    pub value: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MovieFile {
    pub id: i64,
    pub path: Option<String>,
    pub size: i64,
    pub quality: Option<QualityWrapper>,
    /// Kept tolerant like the Sonarr file DTO: list plus single fallback.
    pub languages: Vec<Language>,
    pub language: Option<Language>,
    pub media_info: Option<MediaInfo>,
    pub edition: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_movie_with_file_and_keyed_ratings() {
        let raw = r#"{
            "id": 42,
            "title": "Interstellar",
            "sortTitle": "interstellar",
            "overview": "A team of explorers travel through a wormhole in space.",
            "year": 2014,
            "monitored": true,
            "added": "2021-03-01T10:00:00Z",
            "status": "released",
            "inCinemas": "2014-11-05T00:00:00Z",
            "physicalRelease": "2015-03-31T00:00:00Z",
            "digitalRelease": "2015-03-17T00:00:00Z",
            "runtime": 169,
            "hasFile": true,
            "movieFileId": 7,
            "sizeOnDisk": 33500000000,
            "ratings": {
                "imdb": { "votes": 1900000, "value": 8.7, "type": "user" },
                "tmdb": { "votes": 33000, "value": 8.4, "type": "user" }
            },
            "movieFile": {
                "id": 7,
                "movieId": 42,
                "path": "/movies/Interstellar (2014)/Interstellar.mkv",
                "size": 33500000000,
                "quality": { "quality": { "id": 19, "name": "Bluray-2160p" } },
                "languages": [ { "id": 1, "name": "English" } ],
                "edition": "IMAX",
                "mediaInfo": {
                    "audioChannels": 5.1,
                    "audioCodec": "DTS-HD MA",
                    "audioLanguages": "eng",
                    "videoCodec": "x265",
                    "videoDynamicRange": "HDR10",
                    "resolution": "3840x2160",
                    "runTime": "2:49:00",
                    "subtitles": "eng"
                }
            }
        }"#;
        let movie: Movie = serde_json::from_str(raw).unwrap();
        assert_eq!(movie.id, 42);
        assert_eq!(movie.status.as_deref(), Some("released"));
        assert_eq!(movie.runtime, 169);
        let ratings = movie.ratings.as_ref().unwrap();
        assert!((ratings.tmdb.as_ref().unwrap().value - 8.4).abs() < f64::EPSILON);
        assert!((ratings.imdb.as_ref().unwrap().value - 8.7).abs() < f64::EPSILON);
        let file = movie.movie_file.as_ref().unwrap();
        assert_eq!(
            file.quality.as_ref().unwrap().quality.name.as_deref(),
            Some("Bluray-2160p")
        );
        assert_eq!(file.edition.as_deref(), Some("IMAX"));
        assert_eq!(
            file.media_info
                .as_ref()
                .unwrap()
                .video_dynamic_range
                .as_deref(),
            Some("HDR10")
        );
    }

    #[test]
    fn deserializes_unreleased_movie_without_file() {
        let raw = r#"{
            "id": 43,
            "title": "Announced Only",
            "year": 2027,
            "monitored": true,
            "status": "announced",
            "runtime": 0,
            "hasFile": false,
            "movieFileId": 0
        }"#;
        let movie: Movie = serde_json::from_str(raw).unwrap();
        assert!(!movie.has_file);
        assert!(movie.movie_file.is_none());
        assert!(movie.in_cinemas.is_none());
        assert!(movie.digital_release.is_none());
        assert!(movie.physical_release.is_none());
        assert!(movie.release_date.is_none());
        assert!(movie.ratings.is_none());
    }

    #[test]
    fn tolerates_imdb_only_ratings() {
        let raw = r#"{
            "id": 44,
            "title": "Obscure",
            "hasFile": false,
            "movieFileId": 0,
            "ratings": { "imdb": { "votes": 120, "value": 6.1, "type": "user" } }
        }"#;
        let movie: Movie = serde_json::from_str(raw).unwrap();
        let ratings = movie.ratings.as_ref().unwrap();
        assert!(ratings.tmdb.is_none());
        assert!((ratings.imdb.as_ref().unwrap().value - 6.1).abs() < f64::EPSILON);
    }

    #[test]
    fn deserializes_lookup_result_without_id() {
        // A movie/lookup hit for something not yet in the library: id 0,
        // but the tmdbId the add endpoint needs is present.
        let raw = r#"{
            "title": "Dune: Part Two",
            "year": 2024,
            "tmdbId": 693134,
            "overview": "Paul Atreides unites with the Fremen.",
            "runtime": 167,
            "hasFile": false,
            "movieFileId": 0,
            "ratings": { "tmdb": { "value": 8.2 } }
        }"#;
        let movie: Movie = serde_json::from_str(raw).unwrap();
        assert_eq!(movie.id, 0);
        assert_eq!(movie.tmdb_id, Some(693134));
        assert_eq!(movie.year, Some(2024));
    }
}
