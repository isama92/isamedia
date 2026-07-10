//! Presentation helpers for Sonarr data: pure functions the UI renders from
//! and tests can exercise without a server. Helpers shared with Radarr live
//! in `crate::arr::display` and are re-exported here for the app layer.

use super::models::{Episode, QueueItem, Season, Series};

pub use crate::arr::display::{
    GLYPH_DOWNLOADING, GLYPH_GRABBED, GLYPH_REJECTED, SYMBOL_LEGEND, format_size, monitored_label,
    now_utc_iso, previously_grabbed, queue_progress, release_line2,
};

/// One episode row's status column, in priority order: an active download
/// beats everything (an upgrade of an existing file still shows as
/// downloading), then a present file, then aired-but-missing vs not yet
/// aired. An episode with no air date at all counts as unaired (TBA).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EpStatus {
    Downloaded(String),
    Downloading(u8),
    Unaired,
    Missing,
}

pub fn episode_status(
    episode: &Episode,
    queue_entry: Option<&QueueItem>,
    now_iso: &str,
) -> EpStatus {
    if let Some(entry) = queue_entry {
        return EpStatus::Downloading(queue_progress(entry));
    }
    if episode.has_file {
        let quality = episode
            .episode_file
            .as_ref()
            .and_then(|file| file.quality.as_ref())
            .and_then(|q| q.quality.name.clone())
            .unwrap_or_else(|| "downloaded".to_string());
        return EpStatus::Downloaded(quality);
    }
    match episode.air_date_utc.as_deref() {
        Some(aired) if aired.le(now_iso) => EpStatus::Missing,
        _ => EpStatus::Unaired,
    }
}

pub fn episode_queue_entry(queue: &[QueueItem], episode_id: i64) -> Option<&QueueItem> {
    queue
        .iter()
        .find(|item| item.episode_id == Some(episode_id))
}

/// Whether any queue item belongs to this season; needs the queue fetched
/// with includeEpisode, since only the embedded episode knows its season.
pub fn season_downloading(queue: &[QueueItem], series_id: i64, season_number: i32) -> bool {
    queue.iter().any(|item| {
        item.series_id == Some(series_id)
            && item
                .episode
                .as_ref()
                .is_some_and(|episode| episode.season_number == season_number)
    })
}

pub fn season_name(season_number: i32) -> String {
    if season_number == 0 {
        "Specials".to_string()
    } else {
        format!("Season {season_number}")
    }
}

/// "Season 1   10/12 • monitored"; the downloading marker is a styled span
/// the caller appends, not part of this string.
pub fn season_label(season: &Season) -> String {
    let name = season_name(season.season_number);
    let counts = season
        .statistics
        .as_ref()
        .map(|stats| format!("{}/{}", stats.episode_file_count, stats.episode_count))
        .unwrap_or_default();
    let monitored = monitored_label(season.monitored);
    format!("{name:<10}  {counts:>7} • {monitored}")
}

pub fn episode_code(season_number: i32, episode_number: i32) -> String {
    format!("S{season_number:02}E{episode_number:02}")
}

/// "2023 • 8.8 • 2 seasons" — the meta line under add-search result rows;
/// pieces the server doesn't know are skipped and specials (season 0) are
/// not counted. The Radarr counterpart is `radarr::display::movie_meta`.
pub fn series_meta(series: &Series) -> String {
    let mut parts = Vec::new();
    if let Some(year) = series.year {
        parts.push(year.to_string());
    }
    if let Some(ratings) = &series.ratings
        && ratings.value > 0.0
    {
        parts.push(format!("{:.1}", ratings.value));
    }
    let seasons = series
        .seasons
        .iter()
        .filter(|season| season.season_number > 0)
        .count();
    if seasons > 0 {
        parts.push(format!(
            "{seasons} season{}",
            if seasons == 1 { "" } else { "s" }
        ));
    }
    parts.join(" • ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesSort {
    TitleAz,
    Year,
    RecentlyAdded,
    NextAiring,
}

impl SeriesSort {
    pub fn label(self) -> &'static str {
        match self {
            SeriesSort::TitleAz => "Title, A to Z",
            SeriesSort::Year => "Year, newest first",
            SeriesSort::RecentlyAdded => "Recently added",
            SeriesSort::NextAiring => "Next airing",
        }
    }
}

/// Sort menu entries, default first.
pub const SERIES_SORTS: [SeriesSort; 4] = [
    SeriesSort::TitleAz,
    SeriesSort::Year,
    SeriesSort::RecentlyAdded,
    SeriesSort::NextAiring,
];

fn title_key(series: &Series) -> String {
    series
        .sort_title
        .clone()
        .or_else(|| series.title.as_ref().map(|title| title.to_lowercase()))
        .unwrap_or_default()
}

pub fn sort_series(list: &mut [Series], sort: SeriesSort) {
    match sort {
        SeriesSort::TitleAz => list.sort_by_key(title_key),
        // Missing years sink to the bottom of the newest-first list.
        SeriesSort::Year => {
            list.sort_by_key(|series| std::cmp::Reverse(series.year.unwrap_or(i32::MIN)))
        }
        // ISO datetimes sort lexicographically; unset dates sink to the end.
        SeriesSort::RecentlyAdded => {
            list.sort_by(|a, b| b.added.cmp(&a.added));
        }
        SeriesSort::NextAiring => {
            list.sort_by(|a, b| match (&a.next_airing, &b.next_airing) {
                (Some(a), Some(b)) => a.cmp(b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arr::models::{Quality, QualityWrapper, QueueEpisode};
    use crate::sonarr::models::{EpisodeFile, SeasonStatistics};

    fn quality(name: &str) -> Option<QualityWrapper> {
        Some(QualityWrapper {
            quality: Quality {
                name: Some(name.into()),
            },
        })
    }

    #[test]
    fn status_priority_and_unaired() {
        let now = "2026-07-08T12:00:00Z";
        let mut episode = Episode {
            has_file: true,
            episode_file: Some(EpisodeFile {
                quality: quality("Bluray-1080p"),
                ..EpisodeFile::default()
            }),
            air_date_utc: Some("2020-01-01T00:00:00Z".into()),
            ..Episode::default()
        };
        assert_eq!(
            episode_status(&episode, None, now),
            EpStatus::Downloaded("Bluray-1080p".into())
        );

        // An in-flight download outranks the existing file (upgrade).
        let entry = QueueItem {
            size: 1000.0,
            sizeleft: 250.0,
            ..QueueItem::default()
        };
        assert_eq!(
            episode_status(&episode, Some(&entry), now),
            EpStatus::Downloading(75)
        );

        episode.has_file = false;
        episode.episode_file = None;
        assert_eq!(episode_status(&episode, None, now), EpStatus::Missing);

        episode.air_date_utc = Some("2026-07-12T00:00:00Z".into());
        assert_eq!(episode_status(&episode, None, now), EpStatus::Unaired);

        // No air date at all counts as unaired (TBA), not missing.
        episode.air_date_utc = None;
        assert_eq!(episode_status(&episode, None, now), EpStatus::Unaired);
    }

    #[test]
    fn season_matching_needs_embedded_episode() {
        let queue = vec![QueueItem {
            series_id: Some(5),
            episode_id: Some(102),
            episode: Some(QueueEpisode {
                id: 102,
                season_number: 1,
                ..QueueEpisode::default()
            }),
            ..QueueItem::default()
        }];
        assert!(season_downloading(&queue, 5, 1));
        assert!(!season_downloading(&queue, 5, 2));
        assert!(!season_downloading(&queue, 6, 1));
        assert!(episode_queue_entry(&queue, 102).is_some());
        assert!(episode_queue_entry(&queue, 103).is_none());
    }

    #[test]
    fn season_labels() {
        assert_eq!(season_name(0), "Specials");
        assert_eq!(season_name(3), "Season 3");
        let season = Season {
            season_number: 1,
            monitored: true,
            statistics: Some(SeasonStatistics {
                episode_file_count: 10,
                episode_count: 12,
                ..SeasonStatistics::default()
            }),
        };
        assert_eq!(season_label(&season), "Season 1      10/12 • monitored");
        assert_eq!(episode_code(1, 5), "S01E05");
    }

    #[test]
    fn series_meta_line() {
        use crate::sonarr::models::Ratings;

        let season = |number| Season {
            season_number: number,
            ..Season::default()
        };
        let full = Series {
            year: Some(2023),
            ratings: Some(Ratings { value: 8.8 }),
            // Season 0 (specials) is not counted.
            seasons: vec![season(0), season(1), season(2)],
            ..Series::default()
        };
        assert_eq!(series_meta(&full), "2023 • 8.8 • 2 seasons");

        let single = Series {
            year: Some(2020),
            seasons: vec![season(1)],
            ..Series::default()
        };
        assert_eq!(series_meta(&single), "2020 • 1 season");

        // Zero rating is skipped, like the Radarr helper.
        let unrated = Series {
            year: Some(2019),
            ratings: Some(Ratings { value: 0.0 }),
            seasons: vec![season(1)],
            ..Series::default()
        };
        assert_eq!(series_meta(&unrated), "2019 • 1 season");

        assert_eq!(series_meta(&Series::default()), "");
    }

    #[test]
    fn series_sorts() {
        let series =
            |title: &str, year: Option<i32>, added: Option<&str>, next: Option<&str>| Series {
                title: Some(title.into()),
                year,
                added: added.map(str::to_string),
                next_airing: next.map(str::to_string),
                ..Series::default()
            };
        let mut list = vec![
            series("Beta", Some(2019), Some("2024-01-01T00:00:00Z"), None),
            series(
                "alpha",
                Some(2021),
                Some("2025-06-01T00:00:00Z"),
                Some("2026-08-01T00:00:00Z"),
            ),
            series("Gamma", None, None, Some("2026-07-10T00:00:00Z")),
        ];

        sort_series(&mut list, SeriesSort::TitleAz);
        let titles: Vec<_> = list.iter().map(|s| s.title.as_deref().unwrap()).collect();
        assert_eq!(titles, ["alpha", "Beta", "Gamma"]);

        sort_series(&mut list, SeriesSort::Year);
        assert_eq!(list[0].year, Some(2021));
        assert_eq!(list[2].year, None, "missing year sorts last");

        sort_series(&mut list, SeriesSort::RecentlyAdded);
        assert_eq!(list[0].title.as_deref(), Some("alpha"));
        assert!(list[2].added.is_none(), "never-set added sorts last");

        sort_series(&mut list, SeriesSort::NextAiring);
        assert_eq!(list[0].title.as_deref(), Some("Gamma"));
        assert!(list[2].next_airing.is_none(), "no next airing sorts last");
    }
}
