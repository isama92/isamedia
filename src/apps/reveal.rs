//! Cross-app payloads for "open the item I am looking at in Radarr/Sonarr".
//!
//! The Jellyfin app builds a [`RevealRequest`] from the hovered item and sends
//! it to the owning arr app, which resolves it against its own library and
//! either opens the detail page or answers with a [`RevealFailed`]. These types
//! live here, outside both apps, because an app's own `Msg` enum is private by
//! design: the shell routes messages by app id and never learns their types, so
//! two sibling apps must not name each other's message enums either.

use crate::app::AppId;

/// Which arr app owns a kind of Jellyfin item. Seasons and episodes resolve to
/// their series, so there is no separate episode variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealKind {
    Movie,
    Series,
}

impl RevealKind {
    /// The app that can open this kind of item; matches its `MediaApp::id`.
    pub fn app_id(self) -> AppId {
        match self {
            Self::Movie => "radarr",
            Self::Series => "sonarr",
        }
    }

    /// Tab label, for the messages the user reads.
    pub fn app_title(self) -> &'static str {
        match self {
            Self::Movie => "Radarr",
            Self::Series => "Sonarr",
        }
    }
}

/// Which arr tabs are configured. Jellyfin keeps a copy so it can gate the `u`
/// binding and, more importantly, leave the hint out of its help entirely: a
/// key that is advertised but does nothing is worse than no key at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArrTargets {
    pub radarr: bool,
    pub sonarr: bool,
}

impl ArrTargets {
    /// Whether the app that owns this kind of item is configured.
    pub fn has(self, kind: RevealKind) -> bool {
        match kind {
            RevealKind::Movie => self.radarr,
            RevealKind::Series => self.sonarr,
        }
    }
}

/// A Jellyfin item to find and open in Radarr/Sonarr.
#[derive(Debug, Clone)]
pub struct RevealRequest {
    /// Jellyfin's reveal generation, echoed back in [`RevealFailed`] so a
    /// failure for an abandoned request cannot overwrite the message for a
    /// newer one.
    pub reveal_gen: u64,
    /// The app that asked, so the outcome can be reported back to the tab the
    /// user is still looking at without the target app assuming who sent it.
    pub origin: AppId,
    pub kind: RevealKind,
    /// TMDB id for a movie, TVDB id for a series: whichever the target app
    /// keys its library on. `None` when Jellyfin has no id for the item, which
    /// leaves only the title/year fallback.
    pub external_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
}

/// Sent back to Jellyfin when the arr app could not open the item, so the
/// message lands on the tab the user is still looking at.
#[derive(Debug, Clone)]
pub struct RevealFailed {
    pub reveal_gen: u64,
    pub reason: String,
}

/// Why a reveal found nothing to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealMiss {
    NotFound,
    /// Several library entries share the title and the years could not tell
    /// them apart. Reported rather than guessed: silently opening the wrong
    /// remake is worse than saying nothing matched.
    Ambiguous,
    /// The target app has no library to search: never connected, or its list
    /// fetch failed. Distinguished from `NotFound` because the item may well be
    /// there — we simply could not look.
    Unavailable,
}

impl RevealMiss {
    /// The user-facing message, naming the app that was searched.
    pub fn reason(self, app_title: &str) -> String {
        match self {
            Self::NotFound => format!("not in {app_title}"),
            Self::Ambiguous => format!("several possible matches in {app_title}"),
            Self::Unavailable => format!("{app_title} is unavailable"),
        }
    }
}

/// What came of handing a request to the app that owns the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealOutcome {
    /// The detail page is open; the app should now ask the shell for focus.
    Opened,
    /// The library has not arrived yet. The request stays pending and is retried
    /// when the next list result lands, which is what makes `u` work on a tab
    /// the user has never opened.
    Waiting,
    Missed(RevealMiss),
}

/// One library entry as [`match_index`] needs to see it, so Radarr's movies and
/// Sonarr's series share a single implementation.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub external_id: Option<i64>,
    pub title: Option<&'a str>,
    pub year: Option<i32>,
}

/// Fold a title down to what two metadata sources are likely to agree on:
/// lowercase alphanumerics separated by single spaces. Punctuation and case are
/// where Jellyfin and the *arr sources most often differ on the same title
/// ("Spider-Man: No Way Home" against "Spider Man No Way Home").
pub fn normalise_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Find the library entry a request refers to, as an index into `candidates`.
///
/// An external-id hit is authoritative and wins immediately. Otherwise the
/// normalised titles must agree, and so must the years when both sides know
/// one — a year is what separates a remake from the original, but plenty of
/// entries have none, so a missing year cannot be treated as a mismatch. Any
/// remaining tie is an [`RevealMiss::Ambiguous`] rather than a guess.
pub fn match_index<'a, I>(candidates: I, request: &RevealRequest) -> Result<usize, RevealMiss>
where
    I: IntoIterator<Item = Candidate<'a>>,
{
    let wanted_title = normalise_title(&request.title);
    let mut title_hits: Vec<usize> = Vec::new();
    for (index, candidate) in candidates.into_iter().enumerate() {
        if let (Some(wanted), Some(found)) = (request.external_id, candidate.external_id)
            && wanted == found
        {
            return Ok(index);
        }
        if wanted_title.is_empty() {
            continue;
        }
        let title_agrees = candidate
            .title
            .is_some_and(|title| normalise_title(title) == wanted_title);
        let year_agrees = match (request.year, candidate.year) {
            (Some(wanted), Some(found)) => wanted == found,
            _ => true,
        };
        if title_agrees && year_agrees {
            title_hits.push(index);
        }
    }
    match title_hits.as_slice() {
        [only] => Ok(*only),
        [] => Err(RevealMiss::NotFound),
        _ => Err(RevealMiss::Ambiguous),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(external_id: Option<i64>, title: &str, year: Option<i32>) -> RevealRequest {
        RevealRequest {
            reveal_gen: 1,
            origin: "jellyfin",
            kind: RevealKind::Movie,
            external_id,
            title: title.to_string(),
            year,
        }
    }

    fn candidate(external_id: Option<i64>, title: &str, year: Option<i32>) -> Candidate<'_> {
        Candidate {
            external_id,
            title: Some(title),
            year,
        }
    }

    #[test]
    fn normalises_punctuation_case_and_spacing() {
        assert_eq!(
            normalise_title("Spider-Man: No Way Home"),
            "spider man no way home"
        );
        assert_eq!(normalise_title("  WALL[]E  "), "wall e");
        assert_eq!(normalise_title("Am\u{e9}lie"), "am\u{e9}lie");
        assert_eq!(normalise_title("!!!"), "");
    }

    #[test]
    fn external_id_wins_over_a_matching_title() {
        // The id is authoritative: entry 1 matches it even though entry 0 is
        // the one whose title looks right.
        let library = [
            candidate(Some(999), "The Matrix", Some(1999)),
            candidate(Some(603), "Matrix, The", Some(1999)),
        ];
        let found = match_index(library, &request(Some(603), "The Matrix", Some(1999)));
        assert_eq!(found, Ok(1));
    }

    #[test]
    fn falls_back_to_title_and_year() {
        // No id on either side of the comparison, so the title carries it.
        let library = [
            candidate(None, "Dune", Some(1984)),
            candidate(None, "Dune", Some(2021)),
        ];
        assert_eq!(
            match_index(library, &request(None, "Dune", Some(2021))),
            Ok(1)
        );
        assert_eq!(
            match_index(library, &request(None, "dune!", Some(1984))),
            Ok(0)
        );
    }

    #[test]
    fn year_mismatch_is_not_a_match() {
        let library = [candidate(None, "Dune", Some(2021))];
        assert_eq!(
            match_index(library, &request(None, "Dune", Some(1984))),
            Err(RevealMiss::NotFound)
        );
    }

    #[test]
    fn missing_year_on_either_side_still_matches() {
        assert_eq!(
            match_index(
                [candidate(None, "Dune", None)],
                &request(None, "Dune", Some(2021))
            ),
            Ok(0)
        );
        assert_eq!(
            match_index(
                [candidate(None, "Dune", Some(2021))],
                &request(None, "Dune", None)
            ),
            Ok(0)
        );
    }

    #[test]
    fn indistinguishable_titles_report_ambiguity() {
        // Two remakes and nothing to tell them apart: better to say so than to
        // open one at random.
        let library = [
            candidate(None, "Dune", Some(1984)),
            candidate(None, "Dune", Some(2021)),
        ];
        assert_eq!(
            match_index(library, &request(None, "Dune", None)),
            Err(RevealMiss::Ambiguous)
        );
    }

    #[test]
    fn empty_library_and_untitled_request_find_nothing() {
        assert_eq!(
            match_index([], &request(Some(603), "The Matrix", None)),
            Err(RevealMiss::NotFound)
        );
        // A title that normalises to nothing must not match everything.
        assert_eq!(
            match_index([candidate(None, "Dune", None)], &request(None, "???", None)),
            Err(RevealMiss::NotFound)
        );
    }

    #[test]
    fn miss_reasons_name_the_app() {
        assert_eq!(RevealMiss::NotFound.reason("Radarr"), "not in Radarr");
        assert_eq!(
            RevealMiss::Ambiguous.reason("Sonarr"),
            "several possible matches in Sonarr"
        );
        assert_eq!(
            RevealMiss::Unavailable.reason("Radarr"),
            "Radarr is unavailable"
        );
        assert_eq!(RevealKind::Movie.app_id(), "radarr");
        assert_eq!(RevealKind::Series.app_title(), "Sonarr");
    }
}
