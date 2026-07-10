//! Presentation helpers shared by the *arr apps: pure functions the UIs
//! render from and tests can exercise without a server.

use super::models::{HistoryRecord, QueueItem, Release};

/// Row markers the Sonarr and Radarr lists render. Kept here as the single
/// source so the glyph a row draws and the glyph the help legend documents
/// cannot drift apart.
pub const GLYPH_DOWNLOADING: &str = "↓";
pub const GLYPH_REJECTED: &str = "!";
pub const GLYPH_GRABBED: &str = "✓";

/// The symbol legend, shown as its own labelled column in the expanded help so
/// the glyphs are explained without being mistaken for keybindings.
pub const SYMBOL_LEGEND: [(&str, &str); 3] = [
    (GLYPH_DOWNLOADING, "downloading"),
    (GLYPH_REJECTED, "rejected"),
    (GLYPH_GRABBED, "grabbed before"),
];

/// Percent done, clamped; a queue item reporting no size counts as 0%.
pub fn queue_progress(item: &QueueItem) -> u8 {
    if item.size <= 0.0 {
        return 0;
    }
    let done = 100.0 * (1.0 - item.sizeleft / item.size);
    done.clamp(0.0, 100.0) as u8
}

/// The first language name a queue item carries (mirrors `release_language`).
pub fn queue_language(item: &QueueItem) -> Option<String> {
    item.languages
        .iter()
        .find_map(|language| language.name.clone())
}

/// The quality/format name a queue item was grabbed at, if any.
pub fn queue_quality(item: &QueueItem) -> Option<String> {
    item.quality
        .as_ref()
        .and_then(|wrapper| wrapper.quality.name.clone())
}

/// Remaining time as the server reports it ("HH:MM:SS" / "d.HH:MM:SS"), or a
/// dash when finished/stalled and no estimate is available.
pub fn queue_timeleft(item: &QueueItem) -> String {
    match item.timeleft.as_deref() {
        Some(value) if !value.trim().is_empty() => value.to_string(),
        _ => "-".to_string(),
    }
}

/// A short status for the Downloads view: the download lifecycle state, with
/// the percent appended while still downloading and a marker when the server
/// flags a warning or error. Multi-word camelCase states are spelled out;
/// already-readable single-word ones ("importing", "completed", "queued")
/// pass through unchanged.
pub fn queue_state_label(item: &QueueItem) -> String {
    let raw = item
        .tracked_download_state
        .as_deref()
        .or(item.status.as_deref())
        .unwrap_or("queued");
    let pretty = match raw {
        "importPending" => "import pending",
        "failedPending" => "failed",
        "downloadClientUnavailable" => "client unavailable",
        "delay" => "delayed",
        other => other,
    };
    let mut label = if raw.eq_ignore_ascii_case("downloading") {
        format!("{pretty} {}%", queue_progress(item))
    } else {
        pretty.to_string()
    };
    match item.tracked_download_status.as_deref() {
        Some("warning") => label = format!("{label} (warning)"),
        Some("error") => label = format!("{label} (error)"),
        _ => {}
    }
    label
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

/// The first language name a release carries; newer servers send a list,
/// older ones a single object, so both fields are consulted.
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

/// Whether this exact release was grabbed or imported before (the server
/// keeps the history even after the file is deleted). Matched by the guid a
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

/// The current moment as "YYYY-MM-DDTHH:MM:SSZ", comparable lexicographically
/// with the servers' UTC datetime values. Hand-rolled from the epoch (Howard
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
    use crate::arr::models::{Language, Quality, QualityWrapper};

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
    fn queue_language_and_quality() {
        let item = QueueItem {
            languages: vec![Language {
                name: Some("Japanese".into()),
            }],
            quality: quality("Bluray-1080p"),
            ..QueueItem::default()
        };
        assert_eq!(queue_language(&item).as_deref(), Some("Japanese"));
        assert_eq!(queue_quality(&item).as_deref(), Some("Bluray-1080p"));

        let empty = QueueItem::default();
        assert_eq!(queue_language(&empty), None);
        assert_eq!(queue_quality(&empty), None);
    }

    #[test]
    fn queue_timeleft_falls_back_to_dash() {
        let with = QueueItem {
            timeleft: Some("00:12:30".into()),
            ..QueueItem::default()
        };
        assert_eq!(queue_timeleft(&with), "00:12:30");
        assert_eq!(queue_timeleft(&QueueItem::default()), "-");
    }

    #[test]
    fn queue_state_label_variants() {
        let downloading = QueueItem {
            size: 1000.0,
            sizeleft: 370.0,
            tracked_download_state: Some("downloading".into()),
            tracked_download_status: Some("ok".into()),
            ..QueueItem::default()
        };
        assert_eq!(queue_state_label(&downloading), "downloading 63%");

        let importing = QueueItem {
            tracked_download_state: Some("importPending".into()),
            ..QueueItem::default()
        };
        assert_eq!(queue_state_label(&importing), "import pending");

        let warned = QueueItem {
            status: Some("completed".into()),
            tracked_download_status: Some("warning".into()),
            ..QueueItem::default()
        };
        assert_eq!(queue_state_label(&warned), "completed (warning)");

        // No state or status at all reads as queued.
        assert_eq!(queue_state_label(&QueueItem::default()), "queued");
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
        // Older payloads carry a single language object instead of a list.
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
}
