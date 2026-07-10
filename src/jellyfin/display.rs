//! Presentation helpers for media items, ported from jfsh's item.go so the
//! list output matches jfsh exactly.

use super::models::{ItemKind, MediaItem, MediaStream};

fn runtime(ticks: i64) -> String {
    let minutes = ticks / 600_000_000;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    format!("{}h{}m", minutes / 60, minutes % 60)
}

fn year(item: &MediaItem) -> i32 {
    item.production_year.unwrap_or(0)
}

fn rating(item: &MediaItem) -> f32 {
    item.community_rating.unwrap_or(0.0)
}

fn played_percentage(item: &MediaItem) -> f64 {
    item.user_data
        .as_ref()
        .and_then(|data| data.played_percentage)
        .unwrap_or(0.0)
}

pub fn resume_position_ticks(item: &MediaItem) -> i64 {
    item.user_data
        .as_ref()
        .and_then(|data| data.playback_position_ticks)
        .unwrap_or(0)
}

pub fn watched(item: &MediaItem) -> bool {
    item.user_data
        .as_ref()
        .and_then(|data| data.played)
        .unwrap_or(false)
}

/// The date part of an ISO timestamp ("2019-11-04T03:08:41Z" -> "2019-11-04").
fn short_date(value: &str) -> &str {
    // split always yields at least one element, so this cannot fail.
    value.split('T').next().unwrap_or(value)
}

/// " | Added: ... | Released: ..." plus the watched checkmark, appended to
/// Movie and Video descriptions. Each segment is omitted when its data is
/// absent (e.g. a fetch that did not request DateCreated in its fields).
fn date_suffix(item: &MediaItem) -> String {
    let mut suffix = String::new();
    if let Some(added) = &item.date_created {
        suffix.push_str(&format!(" | Added: {}", short_date(added)));
    }
    if let Some(released) = &item.premiere_date {
        suffix.push_str(&format!(" | Released: {}", short_date(released)));
    }
    if watched(item) {
        suffix.push_str(" ✓");
    }
    suffix
}

/// Human label for a library view's CollectionType.
fn library_label(collection_type: Option<&str>) -> String {
    match collection_type {
        Some("movies") => "Movies".to_string(),
        Some("tvshows") => "Shows".to_string(),
        Some("boxsets") => "Collections".to_string(),
        Some("music") => "Music".to_string(),
        Some("homevideos") => "Home videos".to_string(),
        Some(other) => {
            let mut label = other.to_string();
            if let Some(first) = label.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            label
        }
        None => "Library".to_string(),
    }
}

/// List row title, e.g. "The Expanse S01E02 (2015) [45%]".
pub fn item_title(item: &MediaItem) -> String {
    let name = item.name.as_deref().unwrap_or("");
    match item.kind {
        ItemKind::Movie => {
            let mut title = format!("{name} ({})", year(item));
            let pct = played_percentage(item);
            if pct > 0.0 {
                title.push_str(&format!(" [{pct:.0}%]"));
            }
            title
        }
        ItemKind::Episode => {
            let mut title = format!(
                "{} S{:02}E{:02} ({})",
                item.series_name.as_deref().unwrap_or(""),
                item.parent_index_number.unwrap_or(0),
                item.index_number.unwrap_or(0),
                year(item),
            );
            let pct = played_percentage(item);
            if pct > 0.0 {
                title.push_str(&format!(" [{pct:.0}%]"));
            }
            title
        }
        ItemKind::Series => {
            let mut title = format!("{name} ({})", year(item));
            if let Some(data) = &item.user_data {
                title.push_str(&format!(" [{}]", data.unplayed_item_count.unwrap_or(0)));
            }
            title
        }
        ItemKind::Video => format!("{name} ({})", year(item)),
        // Libraries and collections list by bare name only.
        ItemKind::BoxSet | ItemKind::CollectionFolder => name.to_string(),
        ItemKind::Other => String::new(),
    }
}

/// Second list row line, e.g. "Movie  | Rating: 8.4 | Runtime: 2h10m".
pub fn item_description(item: &MediaItem) -> String {
    match item.kind {
        ItemKind::Movie => format!(
            "Movie  | Rating: {:.1} | Runtime: {}{}",
            rating(item),
            runtime(item.run_time_ticks.unwrap_or(0)),
            date_suffix(item),
        ),
        ItemKind::Series => format!("Series | Rating: {:.1}", rating(item)),
        ItemKind::Episode => item.name.clone().unwrap_or_default(),
        ItemKind::Video => format!(
            "Video  | Rating: {:.1} | Runtime: {}{}",
            rating(item),
            runtime(item.run_time_ticks.unwrap_or(0)),
            date_suffix(item),
        ),
        ItemKind::BoxSet => match item.child_count {
            Some(1) => "1 video".to_string(),
            Some(count) => format!("{count} videos"),
            None => String::new(),
        },
        ItemKind::CollectionFolder => {
            let label = library_label(item.collection_type.as_deref());
            match item.child_count {
                Some(1) => format!("{label} | 1 item"),
                Some(count) => format!("{label} | {count} items"),
                None => label,
            }
        }
        ItemKind::Other => String::new(),
    }
}

/// Title forced onto mpv's window/playlist, e.g. "The Expanse - S1:E2 - ... (2015)".
pub fn media_title(item: &MediaItem) -> String {
    match item.kind {
        ItemKind::Movie => format!("{} ({})", item.name.as_deref().unwrap_or(""), year(item)),
        ItemKind::Episode => format!(
            "{} - S{}:E{} - {} ({})",
            item.series_name.as_deref().unwrap_or(""),
            item.parent_index_number.unwrap_or(0),
            item.index_number.unwrap_or(0),
            item.name.as_deref().unwrap_or(""),
            year(item),
        ),
        _ => item.path.clone().unwrap_or_default(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalSubtitle {
    pub language: String,
    pub title: String,
    /// Server-relative path; append to the host for the full URL.
    pub path: String,
}

pub fn external_subtitles(item: &MediaItem) -> Vec<ExternalSubtitle> {
    item.media_streams
        .iter()
        .filter(|stream| {
            stream.kind.as_deref() == Some("Subtitle") && stream.is_external == Some(true)
        })
        .map(|stream| {
            let index = stream.index.unwrap_or(0);
            ExternalSubtitle {
                language: stream.language.clone().unwrap_or_default(),
                title: stream
                    .display_title
                    .clone()
                    .unwrap_or_else(|| format!("External {index}")),
                path: format!(
                    "/Videos/{id}/{id}/Subtitles/{index}/0/Stream.srt",
                    id = item.id
                ),
            }
        })
        .collect()
}

/// Season selector label: "Season 1", or "Specials" for season 0 / unnumbered.
pub fn season_name(number: Option<i32>) -> String {
    match number {
        Some(n) if n > 0 => format!("Season {n}"),
        _ => "Specials".to_string(),
    }
}

/// Episode code, e.g. "S01E02", from the season/episode numbers an Episode
/// carries. Missing numbers fall back to 0.
pub fn episode_code(item: &MediaItem) -> String {
    format!(
        "S{:02}E{:02}",
        item.parent_index_number.unwrap_or(0),
        item.index_number.unwrap_or(0),
    )
}

/// One-line episode meta for the info panel, e.g. "Aired 2015-12-14 · 8.1 · 45m".
/// Each segment is dropped when its data is absent.
pub fn episode_meta(item: &MediaItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(aired) = &item.premiere_date {
        parts.push(format!("Aired {}", short_date(aired)));
    }
    if rating(item) > 0.0 {
        parts.push(format!("{:.1}", rating(item)));
    }
    if let Some(ticks) = item.run_time_ticks
        && ticks > 0
    {
        parts.push(runtime(ticks));
    }
    parts.join(" · ")
}

/// Technical stream summary for the info panel: a (label, value) row for the
/// primary video and audio tracks and the subtitle languages. Rows whose data
/// is missing are omitted, so a server that returns sparse `MediaStreams`
/// simply shows fewer lines. Values come from Jellyfin's own `DisplayTitle`
/// (e.g. "1080p H264", "English - EAC3 - 5.1"), falling back to the language.
pub fn media_summary(item: &MediaItem) -> Vec<(&'static str, String)> {
    let label = |stream: &MediaStream| {
        stream
            .display_title
            .clone()
            .or_else(|| stream.language.clone())
            .filter(|value| !value.is_empty())
    };
    let first = |kind: &str| {
        item.media_streams
            .iter()
            .find(|stream| stream.kind.as_deref() == Some(kind))
            .and_then(label)
    };

    let mut rows: Vec<(&'static str, String)> = Vec::new();
    if let Some(video) = first("Video") {
        rows.push(("Video", video));
    }
    if let Some(audio) = first("Audio") {
        rows.push(("Audio", audio));
    }
    let subs: Vec<String> = item
        .media_streams
        .iter()
        .filter(|stream| stream.kind.as_deref() == Some("Subtitle"))
        .filter_map(|stream| {
            stream
                .language
                .clone()
                .or_else(|| stream.display_title.clone())
                .filter(|value| !value.is_empty())
        })
        .collect();
    if !subs.is_empty() {
        rows.push(("Subs", subs.join(", ")));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jellyfin::models::{MediaStream, UserData};

    fn movie() -> MediaItem {
        MediaItem {
            id: "m1".into(),
            name: Some("Dune".into()),
            kind: ItemKind::Movie,
            production_year: Some(2021),
            community_rating: Some(8.03),
            run_time_ticks: Some(93 * 600_000_000),
            ..Default::default()
        }
    }

    #[test]
    fn runtime_formatting() {
        assert_eq!(runtime(45 * 600_000_000), "45m");
        assert_eq!(runtime(130 * 600_000_000), "2h10m");
        assert_eq!(runtime(0), "0m");
    }

    #[test]
    fn movie_title_with_progress() {
        let mut item = movie();
        assert_eq!(item_title(&item), "Dune (2021)");
        item.user_data = Some(UserData {
            played_percentage: Some(45.4),
            ..Default::default()
        });
        assert_eq!(item_title(&item), "Dune (2021) [45%]");
        assert_eq!(
            item_description(&item),
            "Movie  | Rating: 8.0 | Runtime: 1h33m"
        );
    }

    #[test]
    fn episode_title() {
        let item = MediaItem {
            id: "e1".into(),
            name: Some("Dulcinea".into()),
            kind: ItemKind::Episode,
            series_name: Some("The Expanse".into()),
            index_number: Some(1),
            parent_index_number: Some(1),
            production_year: Some(2015),
            ..Default::default()
        };
        assert_eq!(item_title(&item), "The Expanse S01E01 (2015)");
        assert_eq!(item_description(&item), "Dulcinea");
        assert_eq!(media_title(&item), "The Expanse - S1:E1 - Dulcinea (2015)");
    }

    #[test]
    fn series_title_shows_unplayed_count() {
        let item = MediaItem {
            id: "s1".into(),
            name: Some("The Expanse".into()),
            kind: ItemKind::Series,
            production_year: Some(2015),
            user_data: Some(UserData {
                unplayed_item_count: Some(12),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(item_title(&item), "The Expanse (2015) [12]");
    }

    #[test]
    fn box_set_lists_by_name_only() {
        let mut item = MediaItem {
            id: "b1".into(),
            name: Some("Trilogy".into()),
            kind: ItemKind::BoxSet,
            production_year: Some(1999),
            community_rating: Some(7.5),
            ..Default::default()
        };
        assert_eq!(item_title(&item), "Trilogy");
        assert_eq!(item_description(&item), "");
        item.child_count = Some(12);
        assert_eq!(item_description(&item), "12 videos");
        item.child_count = Some(1);
        assert_eq!(item_description(&item), "1 video");
    }

    #[test]
    fn short_date_takes_date_part() {
        assert_eq!(short_date("2019-11-04T03:08:41.0000000Z"), "2019-11-04");
        assert_eq!(short_date("not a date"), "not a date");
    }

    #[test]
    fn video_description_with_dates_and_watched() {
        let mut item = MediaItem {
            id: "v1".into(),
            name: Some("Holiday".into()),
            kind: ItemKind::Video,
            community_rating: Some(7.5),
            run_time_ticks: Some(93 * 600_000_000),
            date_created: Some("2024-03-01T10:00:00.0000000Z".into()),
            premiere_date: Some("1999-06-11T00:00:00.0000000Z".into()),
            ..Default::default()
        };
        assert_eq!(
            item_description(&item),
            "Video  | Rating: 7.5 | Runtime: 1h33m | Added: 2024-03-01 | Released: 1999-06-11"
        );
        item.user_data = Some(UserData {
            played: Some(true),
            ..Default::default()
        });
        assert_eq!(
            item_description(&item),
            "Video  | Rating: 7.5 | Runtime: 1h33m | Added: 2024-03-01 | Released: 1999-06-11 ✓"
        );
        // Absent dates leave the description exactly as before.
        item.date_created = None;
        item.premiere_date = None;
        item.user_data = None;
        assert_eq!(
            item_description(&item),
            "Video  | Rating: 7.5 | Runtime: 1h33m"
        );
    }

    #[test]
    fn movie_description_gets_the_same_suffix() {
        let mut item = movie();
        item.date_created = Some("2024-03-01T10:00:00.0000000Z".into());
        item.user_data = Some(UserData {
            played: Some(true),
            ..Default::default()
        });
        assert_eq!(
            item_description(&item),
            "Movie  | Rating: 8.0 | Runtime: 1h33m | Added: 2024-03-01 ✓"
        );
    }

    #[test]
    fn library_view_description() {
        let mut item = MediaItem {
            id: "lib1".into(),
            name: Some("Movies".into()),
            kind: ItemKind::CollectionFolder,
            collection_type: Some("movies".into()),
            child_count: Some(250),
            ..Default::default()
        };
        assert_eq!(item_description(&item), "Movies | 250 items");
        item.collection_type = Some("tvshows".into());
        item.child_count = Some(1);
        assert_eq!(item_description(&item), "Shows | 1 item");
        item.collection_type = Some("boxsets".into());
        item.child_count = None;
        assert_eq!(item_description(&item), "Collections");
        // Unknown types fall back to a capitalized CollectionType.
        item.collection_type = Some("playlists".into());
        assert_eq!(item_description(&item), "Playlists");
        item.collection_type = None;
        assert_eq!(item_description(&item), "Library");
    }

    #[test]
    fn season_names() {
        assert_eq!(season_name(Some(1)), "Season 1");
        assert_eq!(season_name(Some(12)), "Season 12");
        assert_eq!(season_name(Some(0)), "Specials");
        assert_eq!(season_name(None), "Specials");
        assert_eq!(season_name(Some(-1)), "Specials");
    }

    #[test]
    fn episode_codes() {
        let item = MediaItem {
            kind: ItemKind::Episode,
            parent_index_number: Some(1),
            index_number: Some(2),
            ..Default::default()
        };
        assert_eq!(episode_code(&item), "S01E02");
        // Missing numbers fall back to zero.
        assert_eq!(episode_code(&MediaItem::default()), "S00E00");
    }

    #[test]
    fn episode_meta_omits_absent_segments() {
        let mut item = MediaItem {
            kind: ItemKind::Episode,
            premiere_date: Some("2015-12-14T00:00:00.0000000Z".into()),
            community_rating: Some(8.1),
            run_time_ticks: Some(45 * 600_000_000),
            ..Default::default()
        };
        assert_eq!(episode_meta(&item), "Aired 2015-12-14 · 8.1 · 45m");
        // No rating, no runtime, no air date -> empty string, no stray dots.
        item.community_rating = None;
        item.run_time_ticks = None;
        item.premiere_date = None;
        assert_eq!(episode_meta(&item), "");
        // Only a rating.
        item.community_rating = Some(7.0);
        assert_eq!(episode_meta(&item), "7.0");
    }

    #[test]
    fn media_summary_from_streams() {
        let item = MediaItem {
            media_streams: vec![
                MediaStream {
                    kind: Some("Video".into()),
                    display_title: Some("1080p H264".into()),
                    ..Default::default()
                },
                MediaStream {
                    kind: Some("Audio".into()),
                    display_title: Some("English - EAC3 - 5.1".into()),
                    ..Default::default()
                },
                MediaStream {
                    kind: Some("Subtitle".into()),
                    language: Some("English".into()),
                    ..Default::default()
                },
                MediaStream {
                    kind: Some("Subtitle".into()),
                    language: Some("Dutch".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            media_summary(&item),
            vec![
                ("Video", "1080p H264".to_string()),
                ("Audio", "English - EAC3 - 5.1".to_string()),
                ("Subs", "English, Dutch".to_string()),
            ]
        );
        // No streams -> no rows.
        assert!(media_summary(&MediaItem::default()).is_empty());
    }

    #[test]
    fn external_subtitle_paths() {
        let item = MediaItem {
            id: "e1".into(),
            media_streams: vec![
                MediaStream {
                    kind: Some("Video".into()),
                    index: Some(0),
                    ..Default::default()
                },
                MediaStream {
                    kind: Some("Subtitle".into()),
                    is_external: Some(true),
                    index: Some(2),
                    language: Some("eng".into()),
                    display_title: Some("English SRT".into()),
                },
                MediaStream {
                    kind: Some("Subtitle".into()),
                    is_external: Some(false),
                    index: Some(3),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let subs = external_subtitles(&item);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].path, "/Videos/e1/e1/Subtitles/2/0/Stream.srt");
        assert_eq!(subs[0].title, "English SRT");
    }
}
