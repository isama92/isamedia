//! Presentation helpers for media items, ported from jfsh's item.go so the
//! list output matches jfsh exactly.

use super::models::{ItemKind, MediaItem};

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
        ItemKind::Other => String::new(),
    }
}

/// Second list row line, e.g. "Movie  | Rating: 8.4 | Runtime: 2h10m".
pub fn item_description(item: &MediaItem) -> String {
    match item.kind {
        ItemKind::Movie => format!(
            "Movie  | Rating: {:.1} | Runtime: {}",
            rating(item),
            runtime(item.run_time_ticks.unwrap_or(0)),
        ),
        ItemKind::Series => format!("Series | Rating: {:.1}", rating(item)),
        ItemKind::Episode => item.name.clone().unwrap_or_default(),
        ItemKind::Video => format!(
            "Video  | Rating: {:.1} | Runtime: {}",
            rating(item),
            runtime(item.run_time_ticks.unwrap_or(0)),
        ),
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
        assert_eq!(item_description(&item), "Movie  | Rating: 8.0 | Runtime: 1h33m");
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
                    ..Default::default()
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
