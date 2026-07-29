use super::Error;

/// Validate and normalize a server host string; see `crate::net`. Wrapped
/// here so callers inside the Jellyfin client keep getting `jellyfin::Error`.
pub fn normalize_host(host: &str) -> Result<String, Error> {
    crate::net::normalize_host(host).map_err(|err| Error::InvalidHost(err.to_string()))
}

pub fn stream_url(host: &str, item_id: &str) -> Result<String, Error> {
    let host = normalize_host(host)?;
    Ok(format!("{host}/videos/{item_id}/stream?static=true"))
}

/// Tallest poster edge requested from Jellyfin.
///
/// Deliberately one fixed band rather than the exact pixel size a given
/// terminal needs: a cached image then stays reusable across every window size
/// and every terminal, and downscaling to the actual cell size locally costs
/// almost nothing next to a second round trip.
pub const POSTER_MAX_HEIGHT: u32 = 600;

/// Primary-image (poster) URL for an item, or `None` when one cannot be built.
///
/// Every failure means the same thing to the caller — draw no artwork — so a
/// bad host, an empty id and a malformed tag all collapse into `None` instead
/// of an error nobody could act on differently.
///
/// `tag` only busts caches, so omitting it still yields a URL Jellyfin serves.
/// A tag that is not hex is dropped rather than pasted in: it reaches us as
/// opaque server input, and the same goes for the id, which is why both are
/// validated before they reach a URL. No credential appears here; the request
/// carries the usual auth header instead.
pub fn primary_image_url(host: &str, item_id: &str, tag: Option<&str>) -> Option<String> {
    let host = crate::net::normalize_host(host).ok()?;
    if item_id.is_empty()
        || !item_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return None;
    }
    let mut url = format!("{host}/Items/{item_id}/Images/Primary?maxHeight={POSTER_MAX_HEIGHT}");
    if let Some(tag) =
        tag.filter(|tag| !tag.is_empty() && tag.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        url.push_str("&tag=");
        url.push_str(tag);
    }
    Some(url)
}

pub use crate::net::is_plain_http;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_url() {
        assert_eq!(
            stream_url(
                "https://example.com/jellyfin/",
                "127ac3264ae6ff99c33b9bfce1f0b160"
            )
            .unwrap(),
            "https://example.com/jellyfin/videos/127ac3264ae6ff99c33b9bfce1f0b160/stream?static=true"
        );
    }

    #[test]
    fn wraps_net_validation_into_jellyfin_error() {
        assert!(matches!(
            normalize_host("example.com"),
            Err(Error::InvalidHost(_))
        ));
    }

    #[test]
    fn poster_url_keeps_the_base_path_and_carries_no_credential() {
        let url = primary_image_url(
            "https://example.com/jellyfin/",
            "127ac3264ae6ff99c33b9bfce1f0b160",
            Some("a1b2c3d4e5f6"),
        )
        .unwrap();
        assert_eq!(
            url,
            "https://example.com/jellyfin/Items/127ac3264ae6ff99c33b9bfce1f0b160\
             /Images/Primary?maxHeight=600&tag=a1b2c3d4e5f6"
        );
        // The token rides in a header, never here: a URL can reach a log.
        assert!(!url.contains("api_key"));
        assert!(!url.contains("Token"));
    }

    #[test]
    fn poster_url_without_a_tag_is_still_valid() {
        // The tag is only a cache-buster, so an item whose ImageTags the server
        // did not send still gets a URL worth trying.
        let url = primary_image_url("https://example.com", "abc123", None).unwrap();
        assert_eq!(
            url,
            "https://example.com/Items/abc123/Images/Primary?maxHeight=600"
        );
    }

    #[test]
    fn poster_url_drops_a_malformed_tag_rather_than_pasting_it_in() {
        // Jellyfin tags are hex. Anything else is unexpected server input and
        // must not reach a URL; the request is still worth making without it.
        for tag in ["../../etc/passwd", "a1b2 c3", "tag&foo=bar", ""] {
            let url = primary_image_url("https://example.com", "abc123", Some(tag)).unwrap();
            assert_eq!(
                url,
                "https://example.com/Items/abc123/Images/Primary?maxHeight=600"
            );
        }
    }

    #[test]
    fn poster_url_rejects_an_unusable_item_id_or_host() {
        assert_eq!(primary_image_url("https://example.com", "", None), None);
        assert_eq!(
            primary_image_url("https://example.com", "../../Users", None),
            None
        );
        assert_eq!(
            primary_image_url("https://example.com", "abc?x=1", None),
            None
        );
        assert_eq!(primary_image_url("example.com", "abc123", None), None);
    }
}
