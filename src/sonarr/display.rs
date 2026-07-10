//! Presentation helpers for Sonarr data: pure functions the UI renders from
//! and tests can exercise without a server.

use super::models::{Episode, HistoryRecord, QueueItem, Release, Season, Series};

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

/// Percent done, clamped; a queue item reporting no size counts as 0%.
pub fn queue_progress(item: &QueueItem) -> u8 {
    if item.size <= 0.0 {
        return 0;
    }
    let done = 100.0 * (1.0 - item.sizeleft / item.size);
    done.clamp(0.0, 100.0) as u8
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
    let monitored = if season.monitored {
        "monitored"
    } else {
        "unmonitored"
    };
    format!("{name:<10}  {counts:>7} • {monitored}")
}

pub fn episode_code(season_number: i32, episode_number: i32) -> String {
    format!("S{season_number:02}E{episode_number:02}")
}

pub fn format_size(bytes: i64) -> String {
    const UNITS: [(&str, f64); 3] = [
        ("GiB", 1024.0 * 1024.0 * 1024.0),
        ("MiB", 1024.0 * 1024.0),
        ("KiB", 1024.0),
    ];
    let bytes = bytes.max(0) as f64;
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1} {unit}", bytes / scale);
        }
    }
    format!("{bytes:.0} B")
}

pub fn format_age_days(age: i64) -> String {
    format!("{}d", age.max(0))
}

/// The first language name a release carries; v4 sends a list, v3 a single
/// object, so both fields are consulted.
pub fn release_language(release: &Release) -> Option<String> {
    release
        .languages
        .iter()
        .filter_map(|language| language.name.clone())
        .next()
        .or_else(|| {
            release
                .language
                .as_ref()
                .and_then(|language| language.name.clone())
        })
}

/// The metadata line under a release title. Rejection/grabbed markers are
/// styled spans the caller appends, not part of this string.
pub fn release_line2(release: &Release) -> String {
    let mut parts = vec![
        format_age_days(release.age),
        release.indexer.clone().unwrap_or_default(),
        format_size(release.size),
    ];
    if release.protocol.as_deref() == Some("torrent") {
        parts.push(format!(
            "↑{} ↓{}",
            release.seeders.unwrap_or(0),
            release.leechers.unwrap_or(0)
        ));
    }
    if let Some(language) = release_language(release) {
        parts.push(language);
    }
    if let Some(quality) = release
        .quality
        .as_ref()
        .and_then(|q| q.quality.name.clone())
    {
        parts.push(quality);
    }
    parts.retain(|part| !part.is_empty());
    parts.join(" • ")
}

/// Whether this exact release was grabbed or imported before (Sonarr keeps
/// the history even after the file is deleted). Matched by the guid a
/// grabbed event records, falling back to the release title.
pub fn previously_grabbed(history: &[HistoryRecord], release: &Release) -> bool {
    history.iter().any(|record| {
        record
            .data
            .get("guid")
            .and_then(|value| value.as_str())
            .is_some_and(|guid| guid == release.guid)
            || (release.title.is_some() && record.source_title == release.title)
    })
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

/// The current moment as "YYYY-MM-DDTHH:MM:SSZ", comparable lexicographically
/// with Sonarr's airDateUtc values. Hand-rolled from the epoch (Howard
/// Hinnant's civil-from-days algorithm) rather than pulling in a date crate
/// for one timestamp.
pub fn now_utc_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    epoch_to_iso(secs)
}

fn epoch_to_iso(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// Days since 1970-01-01 to a (year, month, day) civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonarr::models::{
        EpisodeFile, Language, Quality, QualityWrapper, QueueEpisode, SeasonStatistics,
    };

    fn quality(name: &str) -> Option<QualityWrapper> {
        Some(QualityWrapper {
            quality: Quality {
                name: Some(name.into()),
            },
        })
    }

    #[test]
    fn size_formatting() {
        assert_eq!(format_size(1_503_238_553), "1.4 GiB");
        assert_eq!(format_size(786_432_000), "750.0 MiB");
        assert_eq!(format_size(4_096), "4.0 KiB");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(-1), "0 B");
    }

    #[test]
    fn epoch_conversion_matches_known_dates() {
        assert_eq!(epoch_to_iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_to_iso(951_827_696), "2000-02-29T12:34:56Z");
        assert_eq!(epoch_to_iso(1_767_225_599), "2025-12-31T23:59:59Z");
        assert_eq!(epoch_to_iso(1_767_225_600), "2026-01-01T00:00:00Z");
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
    fn queue_progress_clamps() {
        let item = |size: f64, sizeleft: f64| QueueItem {
            size,
            sizeleft,
            ..QueueItem::default()
        };
        assert_eq!(queue_progress(&item(0.0, 0.0)), 0);
        assert_eq!(queue_progress(&item(1000.0, 1000.0)), 0);
        assert_eq!(queue_progress(&item(1000.0, 0.0)), 100);
        // sizeleft above size (server hiccup) must not underflow.
        assert_eq!(queue_progress(&item(1000.0, 2000.0)), 0);
    }

    #[test]
    fn season_matching_needs_embedded_episode() {
        let queue = vec![QueueItem {
            series_id: Some(5),
            episode_id: Some(102),
            episode: Some(QueueEpisode {
                id: 102,
                season_number: 1,
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
    fn release_line_torrent_vs_usenet() {
        let mut release = Release {
            age: 748,
            size: 1_503_238_553,
            indexer: Some("Nyaa".into()),
            protocol: Some("torrent".into()),
            seeders: Some(142),
            leechers: Some(7),
            languages: vec![Language {
                name: Some("Japanese".into()),
            }],
            quality: quality("Bluray-1080p"),
            ..Release::default()
        };
        assert_eq!(
            release_line2(&release),
            "748d • Nyaa • 1.4 GiB • ↑142 ↓7 • Japanese • Bluray-1080p"
        );

        release.protocol = Some("usenet".into());
        release.seeders = None;
        release.leechers = None;
        // v3 payloads carry a single language object instead of a list.
        release.languages = Vec::new();
        release.language = Some(Language {
            name: Some("Japanese".into()),
        });
        assert_eq!(
            release_line2(&release),
            "748d • Nyaa • 1.4 GiB • Japanese • Bluray-1080p"
        );
    }

    #[test]
    fn grabbed_before_matches_guid_or_title() {
        let release = Release {
            guid: "magnet-abc".into(),
            title: Some("Black.Clover.S01E06".into()),
            ..Release::default()
        };
        let by_guid = HistoryRecord {
            data: [("guid".to_string(), serde_json::json!("magnet-abc"))]
                .into_iter()
                .collect(),
            ..HistoryRecord::default()
        };
        let by_title = HistoryRecord {
            source_title: Some("Black.Clover.S01E06".into()),
            ..HistoryRecord::default()
        };
        let unrelated = HistoryRecord {
            source_title: Some("Other.Release".into()),
            data: [("guid".to_string(), serde_json::json!("magnet-xyz"))]
                .into_iter()
                .collect(),
            ..HistoryRecord::default()
        };
        assert!(previously_grabbed(&[by_guid], &release));
        assert!(previously_grabbed(&[by_title], &release));
        assert!(!previously_grabbed(&[unrelated], &release));
        assert!(!previously_grabbed(&[], &release));
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
