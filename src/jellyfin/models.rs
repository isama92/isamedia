//! Hand-rolled DTOs for the handful of Jellyfin API shapes isamedia uses.
//! Jellyfin serializes JSON in PascalCase.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum ItemKind {
    Movie,
    Series,
    Episode,
    Video,
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
    pub path: Option<String>,
    pub user_data: Option<UserData>,
    pub media_streams: Vec<MediaStream>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn unknown_kind_falls_back() {
        let item: MediaItem = serde_json::from_str(r#"{"Id": "x", "Type": "MusicAlbum"}"#).unwrap();
        assert_eq!(item.kind, ItemKind::Other);
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
}
