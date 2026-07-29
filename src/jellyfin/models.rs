//! Hand-rolled DTOs for the handful of Jellyfin API shapes isamedia uses.
//! Jellyfin serializes JSON in PascalCase.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum ItemKind {
    Movie,
    Series,
    Episode,
    Video,
    BoxSet,
    CollectionFolder,
    #[serde(other)]
    #[default]
    Other,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaItem {
    pub id: String,
    pub name: Option<String>,
    #[serde(rename = "Type")]
    pub kind: ItemKind,
    pub series_id: Option<String>,
    pub series_name: Option<String>,
    /// Episode number.
    pub index_number: Option<i32>,
    /// Season number.
    pub parent_index_number: Option<i32>,
    pub production_year: Option<i32>,
    pub community_rating: Option<f32>,
    pub run_time_ticks: Option<i64>,
    /// Set on library views ("movies", "tvshows", "boxsets", ...); the item
    /// `Type` alone cannot tell library kinds apart, since every view is a
    /// `CollectionFolder`.
    pub collection_type: Option<String>,
    /// ISO timestamp; only returned when `fields=DateCreated` is requested.
    pub date_created: Option<String>,
    /// ISO timestamp; returned by default, unlike `DateCreated`.
    pub premiere_date: Option<String>,
    /// Number of children of a folder-ish item (library view, box set);
    /// views include it natively, box sets need `fields=ChildCount`.
    pub child_count: Option<i32>,
    pub path: Option<String>,
    pub user_data: Option<UserData>,
    pub media_streams: Vec<MediaStream>,
    /// Plot/description text; only returned when `fields=Overview` is requested.
    /// Shown by the `i` info panel.
    pub overview: Option<String>,
    /// Genre names; only returned when `fields=Genres` is requested.
    pub genres: Vec<String>,
    /// External database ids (`{"Tmdb": "603", "Imdb": "tt0133093"}`); only
    /// returned when `fields=ProviderIds` is requested. Left untyped because
    /// Jellyfin's key set grows with its metadata plugins; read it through
    /// `tmdb_id`/`tvdb_id` rather than indexing, since the key casing has
    /// varied across server versions.
    pub provider_ids: Option<HashMap<String, String>>,
    /// Image kind to cache-busting tag (`{"Primary": "a1b2..."}`). Returned by
    /// default on every item, so unlike `Overview` it needs no `fields=`
    /// request. Read it through `primary_image_tag` rather than indexing: the
    /// key casing is server input, the same caveat as `ProviderIds`.
    pub image_tags: Option<HashMap<String, String>>,
    /// The *series* poster tag on an episode or season. Episodes carry their own
    /// thumbnail in `ImageTags`, but the UI only ever shows show-level artwork,
    /// so this is what an episode row resolves to.
    pub series_primary_image_tag: Option<String>,
}

impl MediaItem {
    /// Look up an external id by provider name, ignoring key casing.
    fn provider_id(&self, provider: &str) -> Option<&str> {
        self.provider_ids
            .as_ref()?
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(provider))
            .map(|(_, value)| value.as_str())
    }

    /// TMDB id, as Radarr keys its movies. `None` when the field was not
    /// requested, the server has no id for the item, or the id is unparseable
    /// (Jellyfin sends these as strings, and a plugin can leave junk behind).
    pub fn tmdb_id(&self) -> Option<i64> {
        self.provider_id("Tmdb")?.parse().ok()
    }

    /// TVDB id, as Sonarr keys its series. Same caveats as `tmdb_id`.
    pub fn tvdb_id(&self) -> Option<i64> {
        self.provider_id("Tvdb")?.parse().ok()
    }

    /// This item's own primary-image tag, ignoring key casing.
    fn primary_image_tag(&self) -> Option<&str> {
        self.image_tags
            .as_ref()?
            .iter()
            .find(|(kind, _)| kind.eq_ignore_ascii_case("Primary"))
            .map(|(_, tag)| tag.as_str())
    }

    /// The item id and image tag whose poster represents this item: `(id, tag)`.
    ///
    /// Anything with a parent series resolves to the *series*, so an episode row
    /// in Resume or Next Up shows the show's poster rather than an episode
    /// still. Keying on `series_id` rather than on `kind` also picks up seasons
    /// for free, which matters because `ItemKind` has no `Season` variant and
    /// they arrive as `Other`.
    ///
    /// The tag is optional on purpose: Jellyfin serves the image without one, so
    /// an item whose `ImageTags` the server omitted is still worth a request.
    /// The cost of being wrong is one 404, which the caller caches negatively.
    pub fn poster_source(&self) -> Option<(&str, Option<&str>)> {
        if let Some(series_id) = self.series_id.as_deref().filter(|id| !id.is_empty()) {
            return Some((series_id, self.series_primary_image_tag.as_deref()));
        }
        if self.id.is_empty() {
            return None;
        }
        Some((self.id.as_str(), self.primary_image_tag()))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserData {
    pub playback_position_ticks: Option<i64>,
    pub played_percentage: Option<f64>,
    pub unplayed_item_count: Option<i32>,
    pub played: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaStream {
    #[serde(rename = "Type")]
    pub kind: Option<String>,
    pub is_external: Option<bool>,
    pub index: Option<i32>,
    pub language: Option<String>,
    pub display_title: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ItemsResponse {
    pub items: Vec<MediaItem>,
    /// Total matches server-side, beyond the requested page. `i64` rather
    /// than `usize` because some endpoints disable the count (NextUp sends
    /// `enableTotalRecordCount=false`) and a defensive type can never fail
    /// deserialization; defaults to 0 when absent.
    pub total_record_count: i64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaSegment {
    pub start_ticks: i64,
    pub end_ticks: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SegmentsResponse {
    pub items: Vec<MediaSegment>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthRequest<'a> {
    pub username: &'a str,
    pub pw: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthResponse {
    pub access_token: String,
    pub user: AuthUser,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthUser {
    pub id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackInfo<'a> {
    pub item_id: &'a str,
    pub position_ticks: i64,
    /// Reported on the Progress endpoint so Jellyfin can show the session as
    /// paused; `false` for start/stopped, which the server ignores.
    pub is_paused: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_info_serialises_pascal_case_field_names() {
        // The field names are a silent wire contract with Jellyfin; a rename
        // typo would type-check but break reporting (notably IsPaused, which
        // the dashboard reads to show a paused session).
        let value = serde_json::to_value(PlaybackInfo {
            item_id: "abc123",
            position_ticks: 42,
            is_paused: true,
        })
        .unwrap();
        assert_eq!(value["ItemId"], "abc123");
        assert_eq!(value["PositionTicks"], 42);
        assert_eq!(value["IsPaused"], true);
    }

    #[test]
    fn deserializes_item() {
        let raw = r#"{
            "Name": "The Expanse",
            "Id": "abc123",
            "Type": "Series",
            "ProductionYear": 2015,
            "CommunityRating": 8.4,
            "UserData": {
                "PlaybackPositionTicks": 0,
                "UnplayedItemCount": 12,
                "Played": false
            }
        }"#;
        let item: MediaItem = serde_json::from_str(raw).unwrap();
        assert_eq!(item.kind, ItemKind::Series);
        assert_eq!(item.name.as_deref(), Some("The Expanse"));
        assert_eq!(item.user_data.unwrap().unplayed_item_count, Some(12));
    }

    #[test]
    fn deserializes_overview_and_genres() {
        let raw = r#"{
            "Id": "m1",
            "Type": "Movie",
            "Overview": "A crew hauls ice across the belt.",
            "Genres": ["Drama", "Sci-Fi"]
        }"#;
        let item: MediaItem = serde_json::from_str(raw).unwrap();
        assert_eq!(
            item.overview.as_deref(),
            Some("A crew hauls ice across the belt.")
        );
        assert_eq!(item.genres, vec!["Drama", "Sci-Fi"]);
        // Both are optional: a bare item leaves them empty.
        let bare: MediaItem = serde_json::from_str(r#"{"Id": "m2", "Type": "Movie"}"#).unwrap();
        assert_eq!(bare.overview, None);
        assert!(bare.genres.is_empty());
    }

    #[test]
    fn unknown_kind_falls_back() {
        let item: MediaItem = serde_json::from_str(r#"{"Id": "x", "Type": "MusicAlbum"}"#).unwrap();
        assert_eq!(item.kind, ItemKind::Other);
    }

    #[test]
    fn deserializes_box_set() {
        let item: MediaItem = serde_json::from_str(
            r#"{"Id": "b1", "Name": "Trilogy", "Type": "BoxSet", "ChildCount": 3}"#,
        )
        .unwrap();
        assert_eq!(item.kind, ItemKind::BoxSet);
        assert_eq!(item.child_count, Some(3));
    }

    #[test]
    fn deserializes_library_view() {
        let raw = r#"{
            "Id": "lib1",
            "Name": "Movies",
            "Type": "CollectionFolder",
            "CollectionType": "movies",
            "ChildCount": 7
        }"#;
        let item: MediaItem = serde_json::from_str(raw).unwrap();
        assert_eq!(item.kind, ItemKind::CollectionFolder);
        assert_eq!(item.collection_type.as_deref(), Some("movies"));
        assert_eq!(item.child_count, Some(7));
    }

    #[test]
    fn deserializes_item_dates() {
        let raw = r#"{
            "Id": "v1",
            "Type": "Video",
            "DateCreated": "2019-11-04T03:08:41.0000000Z",
            "PremiereDate": "1976-11-12T00:00:00.0000000Z"
        }"#;
        let item: MediaItem = serde_json::from_str(raw).unwrap();
        assert_eq!(
            item.date_created.as_deref(),
            Some("2019-11-04T03:08:41.0000000Z")
        );
        assert_eq!(
            item.premiere_date.as_deref(),
            Some("1976-11-12T00:00:00.0000000Z")
        );
        // Both are optional: absent fields stay None.
        let bare: MediaItem = serde_json::from_str(r#"{"Id": "v2", "Type": "Video"}"#).unwrap();
        assert_eq!(bare.date_created, None);
        assert_eq!(bare.premiere_date, None);
        assert_eq!(bare.child_count, None);
    }

    #[test]
    fn items_response_total_record_count() {
        let with: ItemsResponse =
            serde_json::from_str(r#"{"Items": [], "TotalRecordCount": 250}"#).unwrap();
        assert_eq!(with.total_record_count, 250);
        // Absent (e.g. enableTotalRecordCount=false) falls back to 0.
        let without: ItemsResponse = serde_json::from_str(r#"{"Items": []}"#).unwrap();
        assert_eq!(without.total_record_count, 0);
    }

    #[test]
    fn deserializes_episode_with_streams() {
        let raw = r#"{
            "Name": "Dulcinea",
            "Id": "ep1",
            "Type": "Episode",
            "SeriesName": "The Expanse",
            "SeriesId": "abc123",
            "IndexNumber": 1,
            "ParentIndexNumber": 1,
            "RunTimeTicks": 27000000000,
            "MediaStreams": [
                {"Type": "Video", "Index": 0},
                {"Type": "Subtitle", "Index": 2, "IsExternal": true, "Language": "eng", "DisplayTitle": "English SRT"}
            ]
        }"#;
        let item: MediaItem = serde_json::from_str(raw).unwrap();
        assert_eq!(item.parent_index_number, Some(1));
        assert_eq!(item.media_streams.len(), 2);
        assert_eq!(item.media_streams[1].is_external, Some(true));
    }

    #[test]
    fn reads_provider_ids_whatever_the_key_casing() {
        let raw = r#"{
            "Id": "m1",
            "Type": "Movie",
            "ProviderIds": {"TMDB": "603", "tvdb": "1234", "Imdb": "tt0133093"}
        }"#;
        let item: MediaItem = serde_json::from_str(raw).unwrap();
        assert_eq!(item.tmdb_id(), Some(603));
        assert_eq!(item.tvdb_id(), Some(1234));
    }

    #[test]
    fn poster_source_prefers_the_series_for_an_episode() {
        // An episode's own ImageTags hold a still from the episode; the UI only
        // ever shows show-level artwork, so the series must win.
        let raw = r#"{
            "Id": "ep1",
            "Type": "Episode",
            "SeriesId": "series9",
            "ImageTags": {"Primary": "episodestill"},
            "SeriesPrimaryImageTag": "seriesposter"
        }"#;
        let item: MediaItem = serde_json::from_str(raw).unwrap();
        assert_eq!(
            item.poster_source(),
            Some(("series9", Some("seriesposter")))
        );
    }

    #[test]
    fn poster_source_falls_back_to_an_untagged_series() {
        // SeriesPrimaryImageTag absent: still worth requesting, since the tag
        // only busts caches.
        let raw = r#"{"Id": "ep2", "Type": "Episode", "SeriesId": "series9"}"#;
        let item: MediaItem = serde_json::from_str(raw).unwrap();
        assert_eq!(item.poster_source(), Some(("series9", None)));
    }

    #[test]
    fn poster_source_uses_a_season_s_parent_series() {
        // Seasons have no ItemKind variant and arrive as Other, so keying on
        // SeriesId rather than on kind is what makes them work.
        let raw = r#"{
            "Id": "season3",
            "Type": "Season",
            "SeriesId": "series9",
            "SeriesPrimaryImageTag": "seriesposter"
        }"#;
        let item: MediaItem = serde_json::from_str(raw).unwrap();
        assert_eq!(item.kind, ItemKind::Other);
        assert_eq!(
            item.poster_source(),
            Some(("series9", Some("seriesposter")))
        );
    }

    #[test]
    fn poster_source_uses_the_item_itself_for_a_movie_or_series() {
        let movie: MediaItem = serde_json::from_str(
            r#"{"Id": "m1", "Type": "Movie", "ImageTags": {"Primary": "movieposter"}}"#,
        )
        .unwrap();
        assert_eq!(movie.poster_source(), Some(("m1", Some("movieposter"))));

        // Key casing is server input, same as ProviderIds.
        let odd: MediaItem = serde_json::from_str(
            r#"{"Id": "s1", "Type": "Series", "ImageTags": {"primary": "lowercased"}}"#,
        )
        .unwrap();
        assert_eq!(odd.poster_source(), Some(("s1", Some("lowercased"))));

        // No ImageTags at all: still attempt, tag-less.
        let bare: MediaItem = serde_json::from_str(r#"{"Id": "m2", "Type": "Movie"}"#).unwrap();
        assert_eq!(bare.poster_source(), Some(("m2", None)));
    }

    #[test]
    fn poster_source_is_none_without_any_id() {
        // Defensive: `id` is not Option, so a payload missing it defaults to "".
        let item: MediaItem = serde_json::from_str(r#"{"Type": "Movie"}"#).unwrap();
        assert_eq!(item.poster_source(), None);
        // A blank SeriesId must not be mistaken for a parent either.
        let blank: MediaItem =
            serde_json::from_str(r#"{"Id": "ep3", "Type": "Episode", "SeriesId": ""}"#).unwrap();
        assert_eq!(blank.poster_source(), Some(("ep3", None)));
    }

    #[test]
    fn provider_ids_absent_or_unparseable() {
        // The field is only returned when asked for, so every caller has to
        // cope with it missing entirely.
        let bare: MediaItem = serde_json::from_str(r#"{"Id": "m2", "Type": "Movie"}"#).unwrap();
        assert_eq!(bare.tmdb_id(), None);
        assert_eq!(bare.tvdb_id(), None);
        // Present but not a number: a metadata plugin left junk behind.
        let junk: MediaItem =
            serde_json::from_str(r#"{"Id": "m3", "Type": "Movie", "ProviderIds": {"Tmdb": ""}}"#)
                .unwrap();
        assert_eq!(junk.tmdb_id(), None);
    }
}
