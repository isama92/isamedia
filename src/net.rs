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

/// The scheme and authority of a host, dropping any base path.
///
/// `normalize_host` deliberately keeps a base path so `https://example.com/radarr`
/// works. Radarr and Sonarr, though, hand back artwork paths that *already*
/// include their own `UrlBase`, so appending one to a normalized host would
/// repeat the prefix (`https://example.com/radarr/radarr/MediaCover/...`).
/// Those paths join onto this instead.
pub fn origin(host: &str) -> Result<String, InvalidHost> {
    let host = normalize_host(host)?;
    // `normalize_host` has already rejected a host with no scheme, so the
    // fallback arm is unreachable in practice.
    let Some((scheme, rest)) = host.split_once("://") else {
        return Ok(host);
    };
    let authority = rest
        .split_once('/')
        .map_or(rest, |(authority, _)| authority);
    Ok(format!("{scheme}://{authority}"))
}

/// Join a server-supplied media path onto `host`, refusing anything that would
/// leave that server. `None` when no same-origin URL can be built.
///
/// This is what keeps artwork fetches on the user's own machines. Radarr and
/// Sonarr publish two links per image: a path on themselves, and a `remoteUrl`
/// pointing at a metadata site's CDN. isamedia only ever uses the former, and
/// for items not yet in the library the server proxies the art itself, so
/// nothing is lost by refusing the latter. An absolute URL is accepted only
/// when it resolves to the same origin — compared as whole origins rather than
/// by prefix, so `https://example.com.evil.test` cannot pass as
/// `https://example.com`.
pub fn resolve_local(host: &str, path: &str) -> Option<String> {
    let origin = origin(host).ok()?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    // Server-rooted first, so a relative path whose *query* happens to contain
    // "://" is not mistaken for an absolute URL. No current path does that, but
    // misreading one would silently drop the poster.
    if path.starts_with('/') {
        return Some(format!("{origin}{path}"));
    }
    if path.contains("://") {
        let candidate = self::origin(path).ok()?;
        return candidate
            .eq_ignore_ascii_case(&origin)
            .then(|| path.to_string());
    }
    // Not a shape these APIs produce, so treat it as unusable rather than guessing.
    None
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

    #[test]
    fn origin_drops_the_base_path_but_keeps_the_port() {
        assert_eq!(
            origin("https://example.com").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            origin("https://example.com/radarr/").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            origin("http://example.com:7878").unwrap(),
            "http://example.com:7878"
        );
        assert_eq!(
            origin("http://example.com:7878/sonarr").unwrap(),
            "http://example.com:7878"
        );
        assert!(origin("example.com").is_err());
    }

    #[test]
    fn resolve_local_does_not_repeat_the_base_path() {
        // The regression this function exists for: Radarr's own cover paths
        // already carry its UrlBase, so joining one onto the normalized host
        // would produce ".../radarr/radarr/MediaCover/...".
        let resolved = resolve_local(
            "https://example.com/radarr",
            "/radarr/MediaCover/1/poster.jpg?lastWrite=637",
        )
        .unwrap();
        assert_eq!(
            resolved,
            "https://example.com/radarr/MediaCover/1/poster.jpg?lastWrite=637"
        );
        assert!(!resolved.contains("/radarr/radarr/"));
    }

    #[test]
    fn resolve_local_handles_a_server_without_a_base_path() {
        assert_eq!(
            resolve_local("http://example.com:7878", "/MediaCover/9/poster.jpg").unwrap(),
            "http://example.com:7878/MediaCover/9/poster.jpg"
        );
        // The lookup shape: art for an item the server has not added yet, which
        // it proxies on our behalf.
        assert_eq!(
            resolve_local(
                "http://example.com:7878",
                "/MediaCoverProxy/abc123/poster.jpg"
            )
            .unwrap(),
            "http://example.com:7878/MediaCoverProxy/abc123/poster.jpg"
        );
    }

    #[test]
    fn resolve_local_refuses_to_leave_the_configured_server() {
        // "Local only" as an assertion. Every one of these must fetch nothing
        // rather than reach a third party.
        for path in [
            "https://image.tmdb.org/t/p/original/abc.jpg",
            "https://artworks.thetvdb.com/banners/posters/1-1.jpg",
            "http://other.example.net/MediaCover/1/poster.jpg",
            // Prefix-match bypass: a longer hostname that starts with ours.
            "https://example.com.evil.test/MediaCover/1/poster.jpg",
            // Not a shape these APIs emit, so not worth guessing at.
            "MediaCover/1/poster.jpg",
            "",
            "   ",
        ] {
            assert_eq!(
                resolve_local("https://example.com", path),
                None,
                "should have refused {path:?}"
            );
        }
    }

    #[test]
    fn resolve_local_keeps_a_same_origin_absolute_url() {
        // Some server versions return an absolute URL; that is fine as long as
        // it points back at the server we are already talking to.
        assert_eq!(
            resolve_local(
                "https://example.com/radarr",
                "https://example.com/radarr/MediaCover/1/poster.jpg"
            )
            .unwrap(),
            "https://example.com/radarr/MediaCover/1/poster.jpg"
        );
    }

    #[test]
    fn resolve_local_cannot_be_talked_into_another_host_by_a_relative_path() {
        // A protocol-relative path stays a path: the origin prefix pins the
        // scheme and authority, so the "//" only ever lands in the path.
        let resolved = resolve_local("https://example.com", "//evil.test/poster.jpg").unwrap();
        assert!(resolved.starts_with("https://example.com/"));
    }

    #[test]
    fn resolve_local_reads_a_server_rooted_path_before_looking_for_a_scheme() {
        // A relative path whose query happens to contain "://" must still be
        // treated as relative. No current *arr path does this, but taking the
        // absolute branch would fail `origin()` and silently drop the poster.
        assert_eq!(
            resolve_local(
                "https://example.com",
                "/MediaCover/1/poster.jpg?referer=https://example.org/x"
            )
            .unwrap(),
            "https://example.com/MediaCover/1/poster.jpg?referer=https://example.org/x"
        );
    }
}
