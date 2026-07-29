//! What identifies a poster, and how that becomes a URL.
//!
//! Deliberately separate from the cache: the identity has to be stable and
//! comparable, while the URL depends on which host is configured right now, and
//! a re-auth can change the latter without changing the former.

use ratatui::layout::Rect;

use crate::arr::models::poster_path;
use crate::jellyfin::models::MediaItem;
use crate::radarr::models::Movie;
use crate::sonarr::models::Series;

/// Which server a poster is fetched from. Also the key into the registered
/// hosts, so a poster cannot be fetched from a backend the user has not set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Jellyfin,
    Radarr,
    Sonarr,
}

/// A poster's identity, independent of the size it will be drawn at.
///
/// The artwork version is part of the identity — Jellyfin's image tag, and the
/// `?h=` or proxy hash inside an *arr cover path. That is why no cache
/// invalidation logic exists anywhere: re-arted item, different key, and the
/// stale entry is simply never asked for again.
///
/// Note what is *not* here: no URL and no credential. A Jellyfin poster URL is
/// built from the live host at fetch time, and the auth rides in a header.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageKey {
    /// A Jellyfin item's Primary image, by item id and artwork tag.
    Jellyfin { item: String, tag: Option<String> },
    /// A cover path on Radarr, already carrying that server's `UrlBase`.
    Radarr { path: String },
    /// A cover path on Sonarr.
    Sonarr { path: String },
}

impl ImageKey {
    pub fn source(&self) -> Source {
        match self {
            ImageKey::Jellyfin { .. } => Source::Jellyfin,
            ImageKey::Radarr { .. } => Source::Radarr,
            ImageKey::Sonarr { .. } => Source::Sonarr,
        }
    }

    /// The URL to fetch, resolved against the server's configured host, or
    /// `None` when no same-origin URL can be built.
    ///
    /// The two arms join differently and must not be swapped: a Jellyfin path is
    /// ours to construct so it appends to the full host (base path included),
    /// while an *arr cover path already contains that server's `UrlBase` and so
    /// goes onto the origin only. See `crate::net::resolve_local`.
    pub fn url(&self, host: &str) -> Option<String> {
        match self {
            ImageKey::Jellyfin { item, tag } => {
                crate::jellyfin::url::primary_image_url(host, item, tag.as_deref())
            }
            ImageKey::Radarr { path } | ImageKey::Sonarr { path } => {
                crate::net::resolve_local(host, &api_cover_path(path))
            }
        }
    }
}

/// Move an *arr cover path onto the API route serving the same file.
///
/// `images[].url` points at `/MediaCover/{id}/{file}`, which both servers hand to
/// their *static* resource controller under `[Authorize(Policy = "UI")]`. That
/// policy's scheme is whatever login method the user configured — a Forms cookie
/// or Basic — and never the API-key scheme, which is registered separately as
/// `"API"`. An `X-Api-Key` header is therefore ignored there and the request comes
/// back unauthorised; their own web UI only works because a browser sends a
/// session cookie. `/api/v3/mediacover/{id}/{file}` serves the identical bytes
/// from a `[V3ApiController]`, where the key is the accepted credential.
///
/// Also asks for the pre-resized poster rather than the original. Both servers
/// write posters at heights 500 and 250 next to the full-size file
/// (`MediaCoverService.EnsureResizedCovers`), and `MediaCoverController` falls
/// back to the full-size one when a resized sibling is missing, so this needs no
/// client-side retry. The original is typically around 1000x1500 and a megabyte,
/// where the widest slot the layout ever asks for is about 192x288 pixels, so the
/// decoder was throwing away almost everything it downloaded. 500 rather than 250
/// because 250 would be short of a tall terminal's slot.
///
/// Any `UrlBase` prefix and cache-busting query survive, and a
/// `/MediaCoverProxy/...` path is deliberately left alone: it has no `/api/v3`
/// equivalent, so lookup-result artwork is not reachable with an API key at all.
fn api_cover_path(path: &str) -> String {
    /// One of the two heights both servers pre-render posters at.
    const RESIZED_POSTER_HEIGHT: u16 = 500;

    if !path.contains("/MediaCover/") {
        // A proxy path, or a shape we do not recognise: leave it untouched rather
        // than rewriting it onto a route that may not exist.
        return path.to_string();
    }
    let routed = path.replacen("/MediaCover/", "/api/v3/mediacover/", 1);
    let (before, query) = routed
        .split_once('?')
        .map_or((routed.as_str(), ""), |(before, query)| (before, query));
    if let Some(stem) = before.strip_suffix("/poster.jpg") {
        let mut resized = format!("{stem}/poster-{RESIZED_POSTER_HEIGHT}.jpg");
        if !query.is_empty() {
            resized.push('?');
            resized.push_str(query);
        }
        return resized;
    }
    // A banner fallback or an unfamiliar filename: no resized sibling worth
    // guessing at, and banners are already small.
    routed
}

/// The poster for a Jellyfin item: show-level art, so an episode or season
/// resolves to its series. `None` when the server referenced no artwork.
pub fn jellyfin(item: &MediaItem) -> Option<ImageKey> {
    let (id, tag) = item.poster_source()?;
    Some(ImageKey::Jellyfin {
        item: id.to_string(),
        tag: tag.map(str::to_string),
    })
}

/// The poster for a Radarr movie, on the library list and on lookup hits alike.
pub fn radarr(movie: &Movie) -> Option<ImageKey> {
    Some(ImageKey::Radarr {
        path: poster_path(&movie.images)?.to_string(),
    })
}

/// The poster for a Sonarr series, on the library list and on lookup hits alike.
pub fn sonarr(series: &Series) -> Option<ImageKey> {
    Some(ImageKey::Sonarr {
        path: poster_path(&series.images)?.to_string(),
    })
}

/// The cell dimensions a poster was encoded for.
///
/// Part of the cache key because an encoded payload is size-specific: a window
/// resize is therefore an ordinary cache miss that re-encodes, with no
/// resize-specific code path anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PosterSize {
    pub cols: u16,
    pub rows: u16,
}

impl PosterSize {
    /// The size to encode for a reserved slot.
    ///
    /// The width is quantised so that dragging a window edge does not queue one
    /// re-encode per column crossed; a step of 2 is invisible at these sizes and
    /// cuts the churn in half. Under kitty this matters more than it looks: each
    /// re-encode mints a fresh image id and retransmits, orphaning the old one in
    /// the terminal's store.
    /// Rows are left alone: they change far less often than columns during a
    /// horizontal drag, and quantising them would waste a row on short slots.
    pub fn for_slot(slot: Rect) -> Self {
        Self {
            // Round odd widths up to the next even one. `saturating_add` rather
            // than `next_multiple_of` so a pathological width cannot overflow.
            cols: slot.width.saturating_add(slot.width % 2),
            rows: slot.height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(raw: &str) -> MediaItem {
        serde_json::from_str(raw).expect("fixture parses")
    }

    #[test]
    fn jellyfin_key_uses_the_series_for_an_episode() {
        let key = jellyfin(&item(
            r#"{"Id": "ep1", "Type": "Episode", "SeriesId": "s9",
                "ImageTags": {"Primary": "episodestill"},
                "SeriesPrimaryImageTag": "seriesposter"}"#,
        ))
        .unwrap();
        assert_eq!(
            key,
            ImageKey::Jellyfin {
                item: "s9".into(),
                tag: Some("seriesposter".into()),
            }
        );
        assert_eq!(key.source(), Source::Jellyfin);
    }

    #[test]
    fn jellyfin_url_carries_no_credential_and_keeps_the_base_path() {
        let key = jellyfin(&item(
            r#"{"Id": "m1", "Type": "Movie", "ImageTags": {"Primary": "abc123"}}"#,
        ))
        .unwrap();
        let url = key.url("https://example.com/jellyfin").unwrap();
        assert_eq!(
            url,
            "https://example.com/jellyfin/Items/m1/Images/Primary?maxHeight=600&tag=abc123"
        );
    }

    #[test]
    fn arr_keys_come_from_the_poster_cover() {
        let movie: Movie = serde_json::from_str(
            r#"{"id": 42, "images": [
                {"coverType": "fanart", "url": "/MediaCover/42/fanart.jpg"},
                {"coverType": "poster", "url": "/MediaCover/42/poster.jpg?h=abcdef"}
            ]}"#,
        )
        .unwrap();
        let key = radarr(&movie).unwrap();
        assert_eq!(
            key,
            ImageKey::Radarr {
                path: "/MediaCover/42/poster.jpg?h=abcdef".into()
            }
        );
        // The version lives inside the path, so re-arted items key differently
        // and no explicit invalidation is ever needed.
        let reart: Movie = serde_json::from_str(
            r#"{"id": 42, "images": [{"coverType": "poster", "url": "/MediaCover/42/poster.jpg?h=999999"}]}"#,
        )
        .unwrap();
        assert_ne!(radarr(&reart).unwrap(), key);
    }

    #[test]
    fn arr_url_joins_onto_the_origin_not_the_base_path() {
        let movie: Movie = serde_json::from_str(
            r#"{"id": 1, "images": [{"coverType": "poster", "url": "/radarr/MediaCover/1/poster.jpg"}]}"#,
        )
        .unwrap();
        let url = radarr(&movie)
            .unwrap()
            .url("https://example.com/radarr")
            .unwrap();
        assert_eq!(
            url,
            "https://example.com/radarr/api/v3/mediacover/1/poster-500.jpg"
        );
        assert!(!url.contains("/radarr/radarr/"));
    }

    #[test]
    fn arr_covers_use_the_api_route_so_the_api_key_is_accepted() {
        // The static /MediaCover path is behind the UI login policy, which ignores
        // X-Api-Key entirely; the /api/v3 route serves the same file under the API
        // policy. Getting this wrong means every *arr poster silently 401s.
        assert_eq!(
            api_cover_path("/MediaCover/42/poster.jpg?lastWrite=637"),
            "/api/v3/mediacover/42/poster-500.jpg?lastWrite=637"
        );
        // A UrlBase prefix survives, and only the first segment is rewritten.
        assert_eq!(
            api_cover_path("/radarr/MediaCover/7/poster.jpg"),
            "/radarr/api/v3/mediacover/7/poster-500.jpg"
        );
        // A proxy path has no API equivalent, so it must be left untouched rather
        // than rewritten into a route that does not exist.
        assert_eq!(
            api_cover_path("/MediaCoverProxy/deadbeef/poster.jpg"),
            "/MediaCoverProxy/deadbeef/poster.jpg"
        );
    }

    #[test]
    fn a_non_poster_cover_is_routed_but_not_resized() {
        // Only posters have a resized sibling worth naming. A banner is already
        // small, and guessing at heights for cover types we do not request would
        // just add 404s the server has to fall back from.
        assert_eq!(
            api_cover_path("/MediaCover/3/banner.jpg"),
            "/api/v3/mediacover/3/banner.jpg"
        );
        assert_eq!(
            api_cover_path("/MediaCover/3/something-else.png"),
            "/api/v3/mediacover/3/something-else.png"
        );
    }

    #[test]
    fn a_cdn_cover_url_yields_no_fetchable_url() {
        // Belt and braces on top of not deserializing remoteUrl: even if a server
        // put an absolute CDN link in `url`, it must not be fetched.
        let movie: Movie = serde_json::from_str(
            r#"{"id": 1, "images": [{"coverType": "poster",
                "url": "https://image.tmdb.org/t/p/original/abc.jpg"}]}"#,
        )
        .unwrap();
        let key = radarr(&movie).unwrap();
        assert_eq!(key.url("https://example.com"), None);
    }

    #[test]
    fn no_artwork_yields_no_key() {
        let movie: Movie = serde_json::from_str(r#"{"id": 1, "images": []}"#).unwrap();
        assert_eq!(radarr(&movie), None);
        let series: Series = serde_json::from_str(r#"{"id": 1}"#).unwrap();
        assert_eq!(sonarr(&series), None);
    }

    #[test]
    fn sonarr_keys_are_distinct_from_radarr_ones() {
        // Same path shape on two different servers must not share a cache entry.
        let radarr_key = ImageKey::Radarr {
            path: "/MediaCover/1/poster.jpg".into(),
        };
        let sonarr_key = ImageKey::Sonarr {
            path: "/MediaCover/1/poster.jpg".into(),
        };
        assert_ne!(radarr_key, sonarr_key);
        assert_eq!(sonarr_key.source(), Source::Sonarr);
    }

    #[test]
    fn poster_size_quantises_width_to_damp_a_drag_resize() {
        assert_eq!(
            PosterSize::for_slot(Rect::new(0, 0, 15, 11)),
            PosterSize { cols: 16, rows: 11 }
        );
        // Two adjacent odd/even widths collapse onto one encode.
        assert_eq!(
            PosterSize::for_slot(Rect::new(0, 0, 15, 11)),
            PosterSize::for_slot(Rect::new(0, 0, 16, 11))
        );
    }
}
