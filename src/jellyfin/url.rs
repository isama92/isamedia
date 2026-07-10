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
}
