//! Fetching one poster's bytes and turning them into an encoded protocol.
//!
//! Posters get their own `reqwest::Client` rather than borrowing a backend's.
//! The comment on `jellyfin::Client`'s builder scopes its blanket 30 second
//! timeout to "small JSON", which artwork is not; and this client should outlive
//! a re-auth, so posters keep working while a session is being replaced.

use std::io::Cursor;
use std::time::Duration;

use image::{DynamicImage, ImageReader, Limits};
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;

/// Per-request budget. Deliberately shorter than the 30s the JSON clients use: a
/// poster is decorative, and a screenful of requests each holding a connection
/// open for half a minute on a slow link is worse than drawing nothing.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard ceiling on a fetched image. A 4K poster JPEG is comfortably under 2 MiB;
/// this exists so a misconfigured or hostile server cannot make us buffer its
/// entire response. Enforced *during* the body read, not after.
const MAX_IMAGE_BYTES: usize = 8 << 20;

/// Decode limits, which are the real decompression-bomb defence: a 200 KB PNG
/// can declare 30000x30000 pixels, and a byte cap would wave that straight
/// through into a multi-gigabyte allocation.
const MAX_PIXELS_PER_SIDE: u32 = 4096;
const MAX_DECODE_ALLOC: u64 = 64 << 20;

/// The shared client for every poster request.
///
/// Redirects are refused outright, and that is a security requirement rather than
/// a tidiness one. `crate::net::resolve_local` constrains the URL we *build*, but
/// reqwest's default policy follows up to ten hops, and on a cross-host hop it
/// strips only `Authorization`, `Cookie`, `cookie2`, `Proxy-Authorization` and
/// `WWW-Authenticate`. A custom header survives, so a 302 from Radarr or Sonarr
/// would hand `X-Api-Key` — which grants full control of the instance — to
/// whatever host the redirect named. Plain `http` is a supported (if warned
/// about) setup, so that needs a network position, not a server compromise.
///
/// Nothing is lost: neither `/api/v3/mediacover/{id}/{file}` nor
/// `/Items/{id}/Images/Primary` redirects. A 3xx now falls through the
/// `!status.is_success()` arm below and is remembered as absent.
///
/// No compression features either: JPEG, PNG and WebP are already entropy-coded,
/// so gzip would cost real CPU for a percent or two. Because reqwest is built
/// without them it never sends `Accept-Encoding`, so no server will compress a
/// response we then cannot decode.
pub fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
}

/// Fetch the bytes at `url`. `None` means "no usable image here", which the
/// caller remembers so the same 404 is not re-requested on every scroll.
///
/// Nothing in here logs the URL or the header: a Jellyfin poster URL is built
/// from a host that may carry a base path, and the header is a live credential.
pub async fn fetch(
    client: &reqwest::Client,
    url: &str,
    header: (&'static str, &str),
) -> Option<Vec<u8>> {
    let response = match client.get(url).header(header.0, header.1).send().await {
        Ok(response) => response,
        Err(err) => {
            // `without_url` because a reqwest error's Display can embed the URL,
            // and a redirect target is not ours to vouch for.
            tracing::debug!(err = %err.without_url(), "poster request failed");
            return None;
        }
    };
    let status = response.status();
    if !status.is_success() {
        tracing::debug!(%status, "poster unavailable");
        return None;
    }
    read_capped(response).await
}

/// Read a response body, abandoning it the moment it exceeds the cap rather than
/// buffering the whole thing and checking afterwards.
async fn read_capped(mut response: reqwest::Response) -> Option<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_IMAGE_BYTES as u64)
    {
        tracing::debug!("poster declared a body over the cap");
        return None;
    }
    let mut buf = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > MAX_IMAGE_BYTES {
                    tracing::debug!("poster body exceeded the cap mid-read");
                    return None;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(err = %err.without_url(), "poster body read failed");
                return None;
            }
        }
    }
    (!buf.is_empty()).then_some(buf)
}

/// Decode `bytes` and encode a protocol sized for `size`.
///
/// Blocking and CPU-bound in all three phases — decode, resize and (for Sixel)
/// colour quantisation — so this must run inside `spawn_blocking`, never on a
/// runtime worker and certainly never on the render thread.
pub fn encode(picker: &Picker, bytes: &[u8], size: Size) -> Option<Protocol> {
    let image = decode(bytes)?;
    // Always go through `new_protocol`: it scales to `size * font_size` and
    // letterboxes, which is what keeps the halfblock path (whose cell is two
    // vertical pixels) in proportion without any arithmetic here.
    picker
        .new_protocol(image, size, Resize::Fit(None))
        .inspect_err(|err| tracing::debug!(%err, "poster encode failed"))
        .ok()
}

fn decode(bytes: &[u8]) -> Option<DynamicImage> {
    // Guess from the content rather than trusting a Content-Type header.
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .inspect_err(|err| tracing::debug!(%err, "poster format unrecognised"))
        .ok()?;
    reader.limits(decode_limits());
    reader
        .decode()
        .inspect_err(|err| tracing::debug!(%err, "poster decode failed"))
        .ok()
}

// `Limits` is #[non_exhaustive], so a struct literal (and functional-update
// syntax) is unavailable outside the image crate; assigning onto the default is
// the only way to build one.
#[allow(clippy::field_reassign_with_default)]
fn decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_PIXELS_PER_SIDE);
    limits.max_image_height = Some(MAX_PIXELS_PER_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_limits_are_bounded_in_every_dimension() {
        // A missing limit here is a decompression bomb waiting to happen, and the
        // default leaves both dimensions unbounded.
        let limits = decode_limits();
        assert_eq!(limits.max_image_width, Some(MAX_PIXELS_PER_SIDE));
        assert_eq!(limits.max_image_height, Some(MAX_PIXELS_PER_SIDE));
        assert_eq!(limits.max_alloc, Some(MAX_DECODE_ALLOC));
        assert!(Limits::default().max_image_width.is_none());
    }

    #[test]
    fn garbage_bytes_decode_to_nothing_rather_than_panicking() {
        assert!(decode(b"").is_none());
        assert!(decode(b"not an image at all").is_none());
        // A truncated PNG header: plausible enough to be sniffed, too short to
        // decode. This is the shape a partially written cache file would take.
        assert!(decode(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).is_none());
    }

    #[test]
    fn a_real_image_round_trips_through_the_halfblock_encoder() {
        // Proves the decode-then-encode path end to end with no terminal and no
        // network: halfblocks is pure cell writes, so it works under `cargo test`.
        let mut png = Vec::new();
        let image = image::DynamicImage::new_rgb8(24, 36);
        image
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encoding a test png");

        let decoded = decode(&png).expect("the png we just wrote decodes");
        assert_eq!(decoded.width(), 24);

        let picker = Picker::halfblocks();
        let protocol = encode(&picker, &png, Size::new(6, 4)).expect("halfblocks always encodes");
        // Fit letterboxes, so the result never exceeds the requested slot.
        assert!(protocol.size().width <= 6);
        assert!(protocol.size().height <= 4);
    }

    /// Serve one canned HTTP response on a loopback port and report what the
    /// client sent. A hand-rolled listener rather than a mock crate: tokio's `net`
    /// feature is already enabled, so this needs no new dependency, and CLAUDE.md
    /// asks for a mock over reaching for the network.
    async fn serve_once(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding a loopback port");
        let url = format!(
            "http://{}/poster.jpg",
            listener.local_addr().expect("local addr")
        );
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accepting");
            let mut buf = vec![0u8; 2048];
            let read = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            request
        });
        (url, handle)
    }

    #[tokio::test]
    async fn a_redirect_is_refused_rather_than_followed() {
        // The security property: following this would forward X-Api-Key to
        // wherever the Location header points, which for an *arr key means full
        // control of the instance.
        let (url, server) = serve_once(
            "HTTP/1.1 302 Found\r\nLocation: http://evil.test/steal\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let client = client().expect("building the poster client");

        let result = fetch(&client, &url, ("X-Api-Key", "sekret-key")).await;

        assert!(result.is_none(), "a redirect must not yield bytes");
        let request = server.await.expect("server task");
        assert!(
            request.contains("sekret-key"),
            "sanity: the key should reach the configured host"
        );
    }

    #[tokio::test]
    async fn a_successful_body_is_returned_with_the_key_in_a_header() {
        // The companion case, so the redirect test above cannot pass simply
        // because `fetch` is broken.
        let (url, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: image/jpeg\r\n\r\nbytes",
        )
        .await;
        let client = client().expect("building the poster client");

        let result = fetch(&client, &url, ("X-Api-Key", "sekret-key")).await;

        assert_eq!(result.as_deref(), Some(&b"bytes"[..]));
        let request = server.await.expect("server task");
        // Header names go on the wire lowercased, so compare case-insensitively.
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-api-key: sekret-key"),
            "{request}"
        );
        // The credential travels as a header, never in the request line.
        let request_line = request.lines().next().unwrap_or_default();
        assert!(!request_line.contains("sekret-key"), "{request_line}");
    }

    #[tokio::test]
    async fn an_error_status_is_not_mistaken_for_artwork() {
        let (url, _server) =
            serve_once("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n").await;
        let client = client().expect("building the poster client");
        assert!(fetch(&client, &url, ("X-Api-Key", "k")).await.is_none());
    }

    #[test]
    fn jpeg_support_is_compiled_in() {
        // The trap this whole feature hinges on: ratatui-image pins its own
        // `image` to features = ["png"], so disabling its default features (which
        // we must, to drop chafa's native dependency) would leave JPEG out and
        // every poster silently blank. Jellyfin and *arr artwork is JPEG.
        assert!(
            image::ImageFormat::Jpeg.reading_enabled(),
            "JPEG decoding is disabled: check the `image` features in Cargo.toml"
        );
        assert!(image::ImageFormat::Png.reading_enabled());
    }
}
