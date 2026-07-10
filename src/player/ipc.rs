//! mpv JSON IPC: newline-delimited JSON over a unix socket (or a named pipe
//! on Windows). Command builders are pure functions so the exact shapes,
//! including the old-mpv (< 0.38) variants, are unit-testable.

use std::io;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(unix)]
pub type IpcStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type IpcStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// Where the mpv IPC endpoint lives for this session; must be unique per
/// player instance so replace-while-playing never collides.
pub fn socket_path(unique: u128) -> String {
    #[cfg(unix)]
    {
        // Anyone who can connect to this socket can run arbitrary mpv
        // commands (including `run`), so prefer the per-user 0700
        // $XDG_RUNTIME_DIR over world-readable /tmp, where only the umask
        // stands between the socket and other local users.
        let dir = directories::BaseDirs::new()
            .and_then(|dirs| dirs.runtime_dir().map(std::path::Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir);
        dir.join(format!("isamedia-mpv-{unique}"))
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\isamedia-mpv-{unique}")
    }
}

async fn try_connect(path: &str) -> io::Result<IpcStream> {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::connect(path).await
    }
    #[cfg(windows)]
    {
        // ERROR_PIPE_BUSY (231) means the pipe exists but mpv hasn't accepted
        // yet; treat like not-ready so the caller retries.
        tokio::net::windows::named_pipe::ClientOptions::new().open(path)
    }
}

/// Poll for the IPC endpoint while mpv boots: 300 tries x 100ms, same as jfsh.
pub async fn connect(path: &str) -> io::Result<IpcStream> {
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "socket never appeared");
    for _ in 0..300 {
        match try_connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(err) => last_err = err,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(last_err)
}

/// Everything mpv sends us: command replies and events, one JSON per line.
#[derive(Debug, Default, Deserialize)]
pub struct MpvMessage {
    pub error: Option<String>,
    pub event: Option<String>,
    pub name: Option<String>,
    pub data: Option<Value>,
    pub playlist_entry_id: Option<i64>,
}

pub fn parse_message(line: &str) -> Option<MpvMessage> {
    serde_json::from_str(line)
        .map_err(|err| tracing::warn!(%err, line, "failed to parse mpv message"))
        .ok()
}

pub fn encode_request(command: &[Value], request_id: u64) -> String {
    let mut encoded = json!({"command": command, "request_id": request_id}).to_string();
    encoded.push('\n');
    encoded
}

pub fn observe_property_cmd(observe_id: u64, name: &str) -> Vec<Value> {
    vec![json!("observe_property"), json!(observe_id), json!(name)]
}

pub fn seek_cmd(pos: f64) -> Vec<Value> {
    vec![json!("seek"), json!(pos), json!("absolute")]
}

pub fn sub_add_cmd(url: &str, title: &str, lang: &str) -> Vec<Value> {
    vec![
        json!("sub-add"),
        json!(url),
        json!("auto"),
        json!(title),
        json!(lang),
    ]
}

pub fn quit_cmd() -> Vec<Value> {
    vec![json!("quit")]
}

/// Per-file auth for mpv's stream requests. A header keeps the token out of
/// the URL (which mpv shows in logs and OSD). X-Emby-Token is used because
/// its value has no commas — mpv parses http-header-fields as a comma-
/// separated list, which would split the full MediaBrowser header.
fn add_stream_auth(opts: &mut serde_json::Map<String, Value>, token: &str) {
    if !token.is_empty() {
        opts.insert(
            "http-header-fields".into(),
            json!(format!("X-Emby-Token: {token}")),
        );
    }
}

/// `loadfile <url> replace [0] {force-media-title, start, auth}` — the index
/// arg only exists on mpv >= 0.38.
pub fn play_file_cmd(
    url: &str,
    title: &str,
    start_secs: f64,
    old_mpv: bool,
    token: &str,
) -> Vec<Value> {
    let mut opts = serde_json::Map::new();
    opts.insert("force-media-title".into(), json!(title));
    opts.insert("start".into(), json!(format!("{start_secs:.6}")));
    add_stream_auth(&mut opts, token);
    let opts = Value::Object(opts);
    if old_mpv {
        vec![json!("loadfile"), json!(url), json!("replace"), opts]
    } else {
        vec![
            json!("loadfile"),
            json!(url),
            json!("replace"),
            json!(0),
            opts,
        ]
    }
}

pub fn append_file_cmd(url: &str, title: &str, old_mpv: bool, token: &str) -> Vec<Value> {
    let mut opts = serde_json::Map::new();
    opts.insert("force-media-title".into(), json!(title));
    add_stream_auth(&mut opts, token);
    let opts = Value::Object(opts);
    if old_mpv {
        vec![json!("loadfile"), json!(url), json!("append"), opts]
    } else {
        vec![
            json!("loadfile"),
            json!(url),
            json!("append"),
            json!(0),
            opts,
        ]
    }
}

/// Only valid on mpv >= 0.38; callers must skip prepending on old mpv.
pub fn prepend_file_cmd(url: &str, title: &str, token: &str) -> Vec<Value> {
    let mut opts = serde_json::Map::new();
    opts.insert("force-media-title".into(), json!(title));
    add_stream_auth(&mut opts, token);
    vec![
        json!("loadfile"),
        json!(url),
        json!("insert-at"),
        json!(0),
        Value::Object(opts),
    ]
}

/// Strip credential values before a command reaches a log: `api_key=` query
/// params (subtitle URLs) and `X-Emby-Token:` header values. The mpv IPC
/// debug log prints full commands, URLs included.
pub fn redact_secrets(text: &str) -> String {
    let redacted = redact_after(text, "api_key=", &['&', '"', '\\', ',']);
    redact_after(&redacted, "X-Emby-Token: ", &['"', '\\', ','])
}

fn redact_after(text: &str, marker: &str, terminators: &[char]) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(marker) {
        let value_start = idx + marker.len();
        result.push_str(&rest[..value_start]);
        result.push_str("REDACTED");
        let tail = &rest[value_start..];
        let end = tail
            .find(|c| terminators.contains(&c))
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    result.push_str(rest);
    result
}

/// Parse `mpv --version` output; true when older than 0.38.0 (different
/// loadfile syntax, no insert-at). Unparseable output is assumed new.
pub fn is_old_mpv_version(version_output: &str) -> bool {
    let first_line = version_output.lines().next().unwrap_or("");
    let Some(version) = first_line.split_whitespace().nth(1) else {
        return false;
    };
    let version = version.trim_start_matches('v');
    let version = version.split('-').next().unwrap_or(version);
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    let target = [0u32, 38, 0];
    for (part, target) in parts.iter().zip(target) {
        let Ok(value) = part.parse::<u32>() else {
            return false;
        };
        if value < target {
            return true;
        }
        if value > target {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_property_change() {
        let msg =
            parse_message(r#"{"event":"property-change","id":1,"name":"time-pos","data":123.45}"#)
                .unwrap();
        assert_eq!(msg.event.as_deref(), Some("property-change"));
        assert_eq!(msg.name.as_deref(), Some("time-pos"));
        assert_eq!(msg.data.unwrap().as_f64(), Some(123.45));
    }

    #[test]
    fn parses_null_data() {
        let msg =
            parse_message(r#"{"event":"property-change","id":1,"name":"time-pos","data":null}"#)
                .unwrap();
        assert!(msg.data.is_none());
    }

    #[test]
    fn parses_start_file() {
        let msg = parse_message(r#"{"event":"start-file","playlist_entry_id":3}"#).unwrap();
        assert_eq!(msg.event.as_deref(), Some("start-file"));
        assert_eq!(msg.playlist_entry_id, Some(3));
    }

    #[test]
    fn parses_command_reply() {
        let msg = parse_message(r#"{"request_id":42,"error":"success"}"#).unwrap();
        assert_eq!(msg.error.as_deref(), Some("success"));
        assert!(msg.event.is_none());
    }

    #[test]
    fn loadfile_shapes() {
        let new = play_file_cmd("http://x/v", "T", 12.5, false, "tok");
        assert_eq!(
            serde_json::to_string(&new).unwrap(),
            r#"["loadfile","http://x/v","replace",0,{"force-media-title":"T","http-header-fields":"X-Emby-Token: tok","start":"12.500000"}]"#
        );
        let old = play_file_cmd("http://x/v", "T", 12.5, true, "tok");
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            r#"["loadfile","http://x/v","replace",{"force-media-title":"T","http-header-fields":"X-Emby-Token: tok","start":"12.500000"}]"#
        );
        let append_old = append_file_cmd("u", "t", true, "tok");
        assert_eq!(append_old.len(), 4);
        let append_new = append_file_cmd("u", "t", false, "tok");
        assert_eq!(append_new.len(), 5);
        assert_eq!(prepend_file_cmd("u", "t", "tok").len(), 5);
    }

    #[test]
    fn empty_token_adds_no_header() {
        let cmd = play_file_cmd("u", "t", 0.0, false, "");
        assert!(
            !serde_json::to_string(&cmd)
                .unwrap()
                .contains("http-header-fields")
        );
    }

    #[test]
    fn redacts_secrets_from_commands() {
        let sub_add = serde_json::to_string(&sub_add_cmd(
            "https://x/Videos/a/a/Subtitles/2/0/Stream.srt?api_key=deadbeef123",
            "English",
            "eng",
        ))
        .unwrap();
        let redacted = redact_secrets(&sub_add);
        assert!(!redacted.contains("deadbeef123"), "{redacted}");
        assert!(redacted.contains("api_key=REDACTED"), "{redacted}");

        let loadfile =
            serde_json::to_string(&play_file_cmd("http://x/v", "T", 0.0, false, "deadbeef123"))
                .unwrap();
        let redacted = redact_secrets(&loadfile);
        assert!(!redacted.contains("deadbeef123"), "{redacted}");
        assert!(redacted.contains("X-Emby-Token: REDACTED"), "{redacted}");

        // No secrets: unchanged.
        assert_eq!(redact_secrets("plain text"), "plain text");
    }

    #[test]
    fn old_mpv_detection() {
        assert!(is_old_mpv_version("mpv 0.37.0 Copyright ..."));
        assert!(is_old_mpv_version("mpv v0.35.1-dirty\nbuilt on ..."));
        assert!(!is_old_mpv_version("mpv 0.38.0-443-g7480efa62c"));
        assert!(!is_old_mpv_version("mpv 0.40.0"));
        assert!(!is_old_mpv_version("mpv 1.0.0"));
        assert!(!is_old_mpv_version("garbage"));
        assert!(!is_old_mpv_version(""));
    }

    #[test]
    fn request_encoding() {
        assert_eq!(
            encode_request(&seek_cmd(90.0), 7),
            "{\"command\":[\"seek\",90.0,\"absolute\"],\"request_id\":7}\n"
        );
    }
}
