use super::Error;

/// Validate and normalize a server host string: requires an http(s) scheme
/// and a hostname, strips trailing slashes, keeps base paths
/// (e.g. https://example.com/jellyfin).
pub fn normalize_host(host: &str) -> Result<String, Error> {
    let host = host.trim();
    let scheme_end = host.find("://");
    let rest = match scheme_end {
        None => {
            return Err(Error::InvalidHost(
                "host must include http:// or https://".into(),
            ));
        }
        Some(idx) => {
            let scheme = host[..idx].to_ascii_lowercase();
            if scheme != "http" && scheme != "https" {
                return Err(Error::InvalidHost("host must use http:// or https://".into()));
            }
            &host[idx + 3..]
        }
    };
    let hostname = rest.split('/').next().unwrap_or("");
    if hostname.is_empty() {
        return Err(Error::InvalidHost("host must include a hostname".into()));
    }
    Ok(host.trim_end_matches('/').to_string())
}

pub fn stream_url(host: &str, item_id: &str) -> Result<String, Error> {
    let host = normalize_host(host)?;
    Ok(format!("{host}/videos/{item_id}/stream?static=true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from jfsh internal/jellyfin/url_test.go
    #[test]
    fn normalize_host_cases() {
        assert_eq!(
            normalize_host("https://example.com").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            normalize_host("http://example.com/").unwrap(),
            "http://example.com"
        );
        assert_eq!(
            normalize_host("https://example.com/jellyfin/").unwrap(),
            "https://example.com/jellyfin"
        );
        assert!(normalize_host("example.com").is_err());
        assert!(normalize_host("ftp://example.com").is_err());
    }

    #[test]
    fn streaming_url() {
        assert_eq!(
            stream_url("https://example.com/jellyfin/", "127ac3264ae6ff99c33b9bfce1f0b160").unwrap(),
            "https://example.com/jellyfin/videos/127ac3264ae6ff99c33b9bfce1f0b160/stream?static=true"
        );
    }
}
