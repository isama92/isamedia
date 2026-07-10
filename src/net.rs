//! Server-address helpers shared by every backend client. Kept free of any
//! backend-specific error type so each client can map validation failures
//! into its own `Error` enum.

/// A host string that failed validation; the message is user-facing.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidHost(String);

/// Validate and normalize a server host string: requires an http(s) scheme
/// and a hostname, strips trailing slashes, keeps base paths
/// (e.g. https://example.com/jellyfin).
pub fn normalize_host(host: &str) -> Result<String, InvalidHost> {
    let host = host.trim();
    let scheme_end = host.find("://");
    let rest = match scheme_end {
        None => {
            return Err(InvalidHost("host must include http:// or https://".into()));
        }
        Some(idx) => {
            let scheme = host[..idx].to_ascii_lowercase();
            if scheme != "http" && scheme != "https" {
                return Err(InvalidHost("host must use http:// or https://".into()));
            }
            &host[idx + 3..]
        }
    };
    let hostname = rest.split('/').next().unwrap_or("");
    if hostname.is_empty() {
        return Err(InvalidHost("host must include a hostname".into()));
    }
    Ok(host.trim_end_matches('/').to_string())
}

/// True when the host uses unencrypted http://, so callers can warn that
/// credentials and traffic cross the network in cleartext.
pub fn is_plain_http(host: &str) -> bool {
    host.trim().to_ascii_lowercase().starts_with("http://")
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
    fn plain_http_detection() {
        assert!(is_plain_http("http://example.com"));
        assert!(is_plain_http("  HTTP://example.com"));
        assert!(!is_plain_http("https://example.com"));
    }
}
