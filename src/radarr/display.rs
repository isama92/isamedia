//! Presentation helpers for Radarr data: pure functions the UI renders from
//! and tests can exercise without a server. Helpers shared with Sonarr live
//! in `crate::arr::display` and are re-exported here for the app layer.

use super::models::{Movie, QueueItem};

pub use crate::arr::display::{
    GLYPH_DOWNLOADING, GLYPH_GRABBED, GLYPH_REJECTED, SYMBOL_LEGEND, format_size, now_utc_iso,
    previously_grabbed, queue_progress, release_line2,
};

/// One movie's status, in priority order: an active download beats
/// everything (an upgrade of an existing file still shows as downloading),
/// then a present file, then released-but-missing vs not yet released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MovieStatus {
    Downloaded(String),
    Downloading(u8),
    Missing,
    /// Not out yet; carries the release date when the server knows one.
    Unreleased(Option<String>),
}

/// Released iff the server says so, or any release date has passed. This
/// deliberately ignores `isAvailable`, which reflects the movie's
/// minimumAvailability setting and would show announced-only movies with a
/// far-future date as missing.
fn released(movie: &Movie, now_iso: &str) -> bool {
    movie.status.as_deref() == Some("released")
        || [
            &movie.release_date,
            &movie.digital_release,
            &movie.physical_release,
        ]
        .into_iter()
        .flatten()
        .any(|date| date.as_str() <= now_iso)
}

/// The date shown on unreleased rows: the earliest concrete release date,
/// falling back to the cinema date, truncated to "YYYY-MM-DD".
fn unreleased_date(movie: &Movie) -> Option<String> {
    [
        &movie.release_date,
        &movie.digital_release,
        &movie.physical_release,
    ]
    .into_iter()
    .flatten()
    .min()
    .or(movie.in_cinemas.as_ref())
    .map(|date| date.chars().take(10).collect())
}

pub fn movie_status(movie: &Movie, queue_entry: Option<&QueueItem>, now_iso: &str) -> MovieStatus {
    if let Some(entry) = queue_entry {
        return MovieStatus::Downloading(queue_progress(entry));
    }
    if movie.has_file {
        let quality = movie
            .movie_file
            .as_ref()
            .and_then(|file| file.quality.as_ref())
            .and_then(|q| q.quality.name.clone())
            .unwrap_or_else(|| "downloaded".to_string());
        return MovieStatus::Downloaded(quality);
    }
    if released(movie, now_iso) {
        MovieStatus::Missing
    } else {
        MovieStatus::Unreleased(unreleased_date(movie))
    }
}

pub fn movie_queue_entry(queue: &[QueueItem], movie_id: i64) -> Option<&QueueItem> {
    queue.iter().find(|item| item.movie_id == Some(movie_id))
}

/// The rating to display: tmdb (always populated for known movies), falling
/// back to imdb. Zero values mean "no rating" and are skipped.
pub fn rating(movie: &Movie) -> Option<f64> {
    let ratings = movie.ratings.as_ref()?;
    [&ratings.tmdb, &ratings.imdb]
        .into_iter()
        .flatten()
        .map(|rating| rating.value)
        .find(|value| *value > 0.0)
}

/// "2014 • 8.7 • 2h 49m" — the movie meta line shown under list rows and in
/// the detail header; pieces the server doesn't know are skipped.
pub fn movie_meta(movie: &Movie) -> String {
    let mut parts = Vec::new();
    if let Some(year) = movie.year {
        parts.push(year.to_string());
    }
    if let Some(rating) = rating(movie) {
        parts.push(format!("{rating:.1}"));
    }
    if let Some(runtime) = format_runtime(movie.runtime) {
        parts.push(runtime);
    }
    parts.join(" • ")
}

/// "1h 52m" / "52m"; 0 minutes means the runtime is unknown.
pub fn format_runtime(minutes: i64) -> Option<String> {
    if minutes <= 0 {
        return None;
    }
    let (hours, minutes) = (minutes / 60, minutes % 60);
    Some(if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovieSort {
    TitleAz,
    Year,
    RecentlyAdded,
    ReleaseDate,
}

impl MovieSort {
    pub fn label(self) -> &'static str {
        match self {
            MovieSort::TitleAz => "Title, A to Z",
            MovieSort::Year => "Year, newest first",
            MovieSort::RecentlyAdded => "Recently added",
            MovieSort::ReleaseDate => "Release date, newest first",
        }
    }
}

/// Sort menu entries, default first.
pub const MOVIE_SORTS: [MovieSort; 4] = [
    MovieSort::TitleAz,
    MovieSort::Year,
    MovieSort::RecentlyAdded,
    MovieSort::ReleaseDate,
];

fn title_key(movie: &Movie) -> String {
    movie
        .sort_title
        .clone()
        .or_else(|| movie.title.as_ref().map(|title| title.to_lowercase()))
        .unwrap_or_default()
}

pub fn sort_movies(list: &mut [Movie], sort: MovieSort) {
    match sort {
        MovieSort::TitleAz => list.sort_by_key(title_key),
        // Missing years sink to the bottom of the newest-first list.
        MovieSort::Year => {
            list.sort_by_key(|movie| std::cmp::Reverse(movie.year.unwrap_or(i32::MIN)))
        }
        // ISO datetimes sort lexicographically; unset dates sink to the end.
        MovieSort::RecentlyAdded => {
            list.sort_by(|a, b| b.added.cmp(&a.added));
        }
        MovieSort::ReleaseDate => {
            list.sort_by(|a, b| match (unreleased_date(a), unreleased_date(b)) {
                (Some(a), Some(b)) => b.cmp(&a),
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
    use crate::arr::models::{Quality, QualityWrapper};
    use crate::radarr::models::{MovieFile, Rating, Ratings};

    const NOW: &str = "2026-07-08T12:00:00Z";

    fn quality(name: &str) -> Option<QualityWrapper> {
        Some(QualityWrapper {
            quality: Quality {
                name: Some(name.into()),
            },
        })
    }

    #[test]
    fn status_priority() {
        let mut movie = Movie {
            has_file: true,
            movie_file: Some(MovieFile {
                quality: quality("Bluray-2160p"),
                ..MovieFile::default()
            }),
            status: Some("released".into()),
            ..Movie::default()
        };
        assert_eq!(
            movie_status(&movie, None, NOW),
            MovieStatus::Downloaded("Bluray-2160p".into())
        );

        // An in-flight download outranks the existing file (upgrade).
        let entry = QueueItem {
            size: 1000.0,
            sizeleft: 250.0,
            ..QueueItem::default()
        };
        assert_eq!(
            movie_status(&movie, Some(&entry), NOW),
            MovieStatus::Downloading(75)
        );

        movie.has_file = false;
        movie.movie_file = None;
        assert_eq!(movie_status(&movie, None, NOW), MovieStatus::Missing);
    }

    #[test]
    fn released_rule_edges() {
        // status alone marks a movie released, even with no dates.
        let by_status = Movie {
            status: Some("released".into()),
            ..Movie::default()
        };
        assert_eq!(movie_status(&by_status, None, NOW), MovieStatus::Missing);

        // A passed digital date releases an "inCinemas" movie.
        let by_date = Movie {
            status: Some("inCinemas".into()),
            digital_release: Some("2026-07-01T00:00:00Z".into()),
            ..Movie::default()
        };
        assert_eq!(movie_status(&by_date, None, NOW), MovieStatus::Missing);

        // Future dates only: unreleased, showing the earliest one.
        let future = Movie {
            status: Some("announced".into()),
            digital_release: Some("2026-09-12T00:00:00Z".into()),
            physical_release: Some("2026-11-01T00:00:00Z".into()),
            ..Movie::default()
        };
        assert_eq!(
            movie_status(&future, None, NOW),
            MovieStatus::Unreleased(Some("2026-09-12".into()))
        );

        // No dates at all: unreleased with the cinema fallback, else None.
        let cinema_only = Movie {
            status: Some("inCinemas".into()),
            in_cinemas: Some("2026-08-20T00:00:00Z".into()),
            ..Movie::default()
        };
        assert_eq!(
            movie_status(&cinema_only, None, NOW),
            MovieStatus::Unreleased(Some("2026-08-20".into()))
        );
        let bare = Movie {
            status: Some("tba".into()),
            ..Movie::default()
        };
        assert_eq!(
            movie_status(&bare, None, NOW),
            MovieStatus::Unreleased(None)
        );
    }

    #[test]
    fn rating_prefers_tmdb_then_imdb() {
        let both = Movie {
            ratings: Some(Ratings {
                tmdb: Some(Rating { value: 8.4 }),
                imdb: Some(Rating { value: 8.7 }),
            }),
            ..Movie::default()
        };
        assert_eq!(rating(&both), Some(8.4));

        let imdb_only = Movie {
            ratings: Some(Ratings {
                tmdb: None,
                imdb: Some(Rating { value: 6.1 }),
            }),
            ..Movie::default()
        };
        assert_eq!(rating(&imdb_only), Some(6.1));

        // A zero tmdb value means "unrated there" and falls through.
        let zero_tmdb = Movie {
            ratings: Some(Ratings {
                tmdb: Some(Rating { value: 0.0 }),
                imdb: Some(Rating { value: 5.5 }),
            }),
            ..Movie::default()
        };
        assert_eq!(rating(&zero_tmdb), Some(5.5));
        assert_eq!(rating(&Movie::default()), None);
    }

    #[test]
    fn meta_line_skips_unknown_pieces() {
        let full = Movie {
            year: Some(2014),
            runtime: 169,
            ratings: Some(Ratings {
                tmdb: Some(Rating { value: 8.4 }),
                imdb: None,
            }),
            ..Movie::default()
        };
        assert_eq!(movie_meta(&full), "2014 • 8.4 • 2h 49m");

        let bare = Movie::default();
        assert_eq!(movie_meta(&bare), "");

        let year_only = Movie {
            year: Some(2027),
            ..Movie::default()
        };
        assert_eq!(movie_meta(&year_only), "2027");
    }

    #[test]
    fn runtime_formatting() {
        assert_eq!(format_runtime(169).as_deref(), Some("2h 49m"));
        assert_eq!(format_runtime(52).as_deref(), Some("52m"));
        assert_eq!(format_runtime(60).as_deref(), Some("1h 0m"));
        assert_eq!(format_runtime(0), None);
        assert_eq!(format_runtime(-5), None);
    }

    #[test]
    fn queue_matching_by_movie_id() {
        let queue = vec![QueueItem {
            movie_id: Some(42),
            ..QueueItem::default()
        }];
        assert!(movie_queue_entry(&queue, 42).is_some());
        assert!(movie_queue_entry(&queue, 43).is_none());
    }

    #[test]
    fn movie_sorts() {
        let movie =
            |title: &str, year: Option<i32>, added: Option<&str>, digital: Option<&str>| Movie {
                title: Some(title.into()),
                year,
                added: added.map(str::to_string),
                digital_release: digital.map(str::to_string),
                ..Movie::default()
            };
        let mut list = vec![
            movie(
                "Beta",
                Some(2019),
                Some("2024-01-01T00:00:00Z"),
                Some("2019-05-01T00:00:00Z"),
            ),
            movie(
                "alpha",
                Some(2021),
                Some("2025-06-01T00:00:00Z"),
                Some("2021-08-01T00:00:00Z"),
            ),
            movie("Gamma", None, None, None),
        ];

        sort_movies(&mut list, MovieSort::TitleAz);
        let titles: Vec<_> = list.iter().map(|m| m.title.as_deref().unwrap()).collect();
        assert_eq!(titles, ["alpha", "Beta", "Gamma"]);

        sort_movies(&mut list, MovieSort::Year);
        assert_eq!(list[0].year, Some(2021));
        assert_eq!(list[2].year, None, "missing year sorts last");

        sort_movies(&mut list, MovieSort::RecentlyAdded);
        assert_eq!(list[0].title.as_deref(), Some("alpha"));
        assert!(list[2].added.is_none(), "never-set added sorts last");

        sort_movies(&mut list, MovieSort::ReleaseDate);
        assert_eq!(list[0].title.as_deref(), Some("alpha"), "newest first");
        assert!(
            list[2].digital_release.is_none(),
            "no release date sorts last"
        );
    }
}
