//! The playback supervisor: owns the mpv process, speaks JSON IPC, reports
//! playback state to Jellyfin, auto-skips configured media segments, and
//! injects external subtitles. Ported from jfsh's internal/mpv/play.go.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::config::{LanguagePrefs, TrackPreference};
use crate::jellyfin::{Client, MediaItem, display, url::stream_url};
use crate::lang;

use super::ipc::{self, IpcStream};
use super::ticks::{seconds_to_ticks, ticks_to_seconds};
use super::{PlayerCommand, PlayerEvent, TrackKind, override_key};

const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(3);
/// How long to wait for `mpv --version` to answer. A wedged mpv wrapper script
/// must not block playback from ever starting, so the probe is bounded like
/// every other external-process interaction (see the IPC connect poll).
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for mpv to exit after we ask it to quit.
const QUIT_GRACE: Duration = Duration::from_secs(5);
/// How long to wait for queued playback reports (typically the final stopped)
/// to flush after mpv is gone, bounded so a dead server cannot hold shutdown
/// hostage.
const REPORT_FLUSH_GRACE: Duration = Duration::from_secs(5);

/// Worst-case time the supervisor needs after being told to stop: wait for mpv
/// to obey `quit` (`QUIT_GRACE`), then flush the final report
/// (`REPORT_FLUSH_GRACE`), plus a 1s margin. The shell's shutdown drain waits
/// at least this long so it never abandons the supervisor mid-flush, which
/// would leak the IPC socket and lose the final Stopped report. Derived from
/// the two budgets so it stays correct if either changes;
/// `shutdown_budget_covers_supervisor` guards the invariant regardless.
pub(crate) const SHUTDOWN_BUDGET: Duration = QUIT_GRACE
    .saturating_add(REPORT_FLUSH_GRACE)
    .saturating_add(Duration::from_secs(1));

static OLD_MPV: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();

async fn is_old_mpv() -> bool {
    // `get_or_try_init` caches an `Ok` for the whole session but leaves the cell
    // unset on `Err`, so a definitive answer (or a persistent spawn failure) is
    // memoised while a transient timeout is retried on the next playback. That
    // stops a single 5s hang from locking in "modern mpv" against a genuinely
    // old binary for the rest of the session.
    OLD_MPV
        .get_or_try_init(|| async {
            let probe = tokio::process::Command::new("mpv")
                .arg("--version")
                .kill_on_drop(true)
                .output();
            match tokio::time::timeout(VERSION_PROBE_TIMEOUT, probe).await {
                Ok(Ok(output)) => {
                    let old = ipc::is_old_mpv_version(&String::from_utf8_lossy(&output.stdout));
                    if old {
                        tracing::warn!(
                            "mpv is older than 0.38.0; earlier episodes will not be \
                             prepended to the playlist"
                        );
                    }
                    Ok(old)
                }
                // A spawn failure (e.g. mpv missing) is persistent, so cache it.
                Ok(Err(err)) => {
                    tracing::debug!(%err, "failed to run mpv --version");
                    Ok(false)
                }
                // Timing out (dropping the future) kills the child via
                // `kill_on_drop`. Assume a modern mpv for this playback, but
                // return `Err` so the transient result is not memoised.
                Err(_) => {
                    tracing::warn!("mpv --version timed out; assuming a modern mpv");
                    Err(())
                }
            }
        })
        .await
        .ok()
        .copied()
        .unwrap_or(false)
}

/// The order items get loaded into mpv, which is also the order mpv assigns
/// `playlist_entry_id`s (starting at 1): selected first, then the items
/// after it, then the ones before it (skipped entirely on old mpv).
fn load_order(len: usize, index: usize, include_prior: bool) -> Vec<usize> {
    let mut order = Vec::with_capacity(len);
    order.push(index);
    order.extend(index + 1..len);
    if include_prior {
        order.extend((0..index).rev());
    }
    order
}

/// Commands applying the language preferences once, before the playlist
/// loads. alang/slang/sid are global options while mpv idles, so every
/// playlist entry inherits them — which is exactly why they are not passed
/// as per-file loadfile options: a mid-session switch could then never
/// affect the already-loaded later entries.
fn language_setup_cmds(prefs: &LanguagePrefs) -> Vec<Vec<Value>> {
    let mut cmds = Vec::new();
    if let Some(TrackPreference::Language(code)) = &prefs.audio {
        cmds.push(ipc::set_property_cmd(
            "alang",
            json!(lang::mpv_lang_list(code)),
        ));
    }
    match &prefs.subtitles {
        Some(TrackPreference::Language(code)) => cmds.push(ipc::set_property_cmd(
            "slang",
            json!(lang::mpv_lang_list(code)),
        )),
        Some(TrackPreference::Off) => cmds.push(ipc::set_property_cmd("sid", json!("no"))),
        _ => {}
    }
    cmds
}

/// Commands re-asserting the session preference between playlist entries
/// after a user switch. Resetting aid/sid to "auto" is essential: the
/// switch left a numeric track id in the option, which would beat
/// alang/slang on the next file and can map to a different language there.
fn language_reapply_cmds(prefs: &LanguagePrefs) -> Vec<Vec<Value>> {
    let mut cmds = Vec::new();
    match &prefs.audio {
        Some(TrackPreference::Language(code)) => {
            cmds.push(ipc::set_property_cmd(
                "alang",
                json!(lang::mpv_lang_list(code)),
            ));
            cmds.push(ipc::set_property_cmd("aid", json!("auto")));
        }
        Some(TrackPreference::Default) => {
            cmds.push(ipc::set_property_cmd("alang", json!("")));
            cmds.push(ipc::set_property_cmd("aid", json!("auto")));
        }
        Some(TrackPreference::Off) | None => {}
    }
    match &prefs.subtitles {
        Some(TrackPreference::Language(code)) => {
            cmds.push(ipc::set_property_cmd(
                "slang",
                json!(lang::mpv_lang_list(code)),
            ));
            cmds.push(ipc::set_property_cmd("sid", json!("auto")));
        }
        Some(TrackPreference::Off) => {
            cmds.push(ipc::set_property_cmd("slang", json!("")));
            cmds.push(ipc::set_property_cmd("sid", json!("no")));
        }
        Some(TrackPreference::Default) | None => {}
    }
    cmds
}

/// Map an aid/sid property-change value to a remembered preference, or None
/// when it should be ignored: an untagged track (no language to remember),
/// "auto"/null/malformed data, an id missing from the track cache, or audio
/// switched off (muting audio is not a language choice).
fn classify_selection(
    kind: TrackKind,
    data: Option<&Value>,
    tracks: &[ipc::Track],
) -> Option<TrackPreference> {
    let data = data?;
    if let Some(id) = data.as_i64() {
        let track_kind = match kind {
            TrackKind::Audio => "audio",
            TrackKind::Subtitle => "sub",
        };
        let track = tracks
            .iter()
            .find(|track| track.kind == track_kind && track.id == id)?;
        let tag = track.lang.as_deref()?;
        return Some(TrackPreference::Language(lang::canonical(tag)));
    }
    let off = matches!(data, Value::Bool(false)) || data.as_str() == Some("no");
    if off && kind == TrackKind::Subtitle {
        return Some(TrackPreference::Off);
    }
    None
}

/// End of the segment `pos` is inside of, if any.
fn inside_skippable_segment(segments: &[(f64, f64)], pos: f64) -> Option<f64> {
    segments
        .iter()
        .find(|(start, end)| pos >= *start && pos < *end)
        .map(|(_, end)| *end)
}

/// A playback state report for the reporter task. Reports are sent
/// fire-and-forget from the mpv event loop so a slow or unreachable server
/// (30s request timeout) can never stall position updates, segment skips,
/// or quit handling.
enum Report {
    Start {
        item_id: String,
        ticks: i64,
    },
    Progress {
        item_id: String,
        ticks: i64,
        is_paused: bool,
    },
    Stopped {
        item_id: String,
        ticks: i64,
    },
}

/// Reporter task: runs reports sequentially, preserving order. A backlog of
/// progress reports for the same item collapses to the newest one, so an
/// outage (enqueue every 3s, 30s timeout per attempt) cannot grow the queue.
/// A 401 is signalled (once) over `unauthorized_tx` so the supervisor can
/// surface the expired session; the reporter itself keeps draining.
fn spawn_reporter(
    client: Client,
    unauthorized_tx: mpsc::UnboundedSender<()>,
) -> (mpsc::UnboundedSender<Report>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Report>();
    let task = tokio::spawn(async move {
        let mut signalled = false;
        let mut signal_unauthorized = |unauthorized: bool| {
            if unauthorized && !signalled {
                signalled = true;
                tracing::warn!("session token rejected while reporting playback state");
                let _ = unauthorized_tx.send(());
            }
        };
        while let Some(mut report) = rx.recv().await {
            while let Ok(next) = rx.try_recv() {
                match (&report, &next) {
                    (Report::Progress { item_id: a, .. }, Report::Progress { item_id: b, .. })
                        if a == b =>
                    {
                        report = next;
                    }
                    _ => {
                        signal_unauthorized(send_report(&client, report).await);
                        report = next;
                    }
                }
            }
            signal_unauthorized(send_report(&client, report).await);
        }
    });
    (tx, task)
}

/// Returns true when the server rejected the session token.
async fn send_report(client: &Client, report: Report) -> bool {
    let (kind, item, result) = match &report {
        Report::Start { item_id, ticks } => (
            "start",
            item_id,
            client.report_playback_start(item_id, *ticks).await,
        ),
        Report::Progress {
            item_id,
            ticks,
            is_paused,
        } => (
            "progress",
            item_id,
            client
                .report_playback_progress(item_id, *ticks, *is_paused)
                .await,
        ),
        Report::Stopped { item_id, ticks } => (
            "stopped",
            item_id,
            client.report_playback_stopped(item_id, *ticks).await,
        ),
    };
    match result {
        Ok(()) => {
            tracing::info!(kind, item = %item, "reported playback state");
            false
        }
        Err(err) => {
            tracing::error!(%err, kind, item = %item, "failed to report playback state");
            matches!(err, crate::jellyfin::Error::Unauthorized)
        }
    }
}

struct Ipc {
    writer: WriteHalf<IpcStream>,
    request_id: u64,
}

impl Ipc {
    async fn send(&mut self, command: &[Value]) -> std::io::Result<()> {
        self.request_id += 1;
        let encoded = ipc::encode_request(command, self.request_id);
        tracing::debug!(command = %ipc::redact_secrets(encoded.trim_end()), "mpv send");
        self.writer.write_all(encoded.as_bytes()).await
    }
}

pub(super) async fn run(
    client: Client,
    items: Vec<MediaItem>,
    index: usize,
    skip_types: Vec<String>,
    prefs: LanguagePrefs,
    mut cmd_rx: mpsc::UnboundedReceiver<PlayerCommand>,
    emit: &(impl Fn(PlayerEvent) + Send + Sync),
) {
    let old_mpv = is_old_mpv().await;

    // Random per-run id for the IPC endpoint. A guessable name (time XOR pid)
    // would let a local squatter on Windows win the pipe-name race and receive
    // the loadfile commands with their auth token; a v4 UUID removes that. On
    // Unix the 0700 dir already guards the socket.
    let unique = uuid::Uuid::new_v4().as_u128();
    let socket = match ipc::socket_path(unique) {
        Ok(socket) => socket,
        Err(err) => {
            emit(PlayerEvent::Failed(format!(
                "failed to prepare mpv IPC socket directory: {err}"
            )));
            return;
        }
    };
    // The socket file and its fallback directory now exist (or will once mpv
    // starts); this guard removes them on every exit path from here, including
    // when the runtime drops this task mid-shutdown, so no explicit
    // `cleanup_socket` calls are needed below.
    let _socket_guard = ipc::SocketGuard::new(socket.clone());

    let mut child = match tokio::process::Command::new("mpv")
        // `once`, not `yes`: mpv idles waiting for the playlist we load over
        // IPC, then quits when it finishes instead of idling forever. Plain
        // `--idle` leaves mpv (and this supervisor) alive after the last
        // entry ends, so `PlayerEvent::Exited` never fires and the now-playing
        // bar freezes.
        .arg("--idle=once")
        .arg(format!("--input-ipc-server={socket}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            emit(PlayerEvent::Failed(format!(
                "failed to launch mpv (is it installed and in PATH?): {err}"
            )));
            // The fallback 0700 directory is removed by `_socket_guard`.
            return;
        }
    };

    let stream = match ipc::connect(&socket).await {
        Ok(stream) => stream,
        Err(err) => {
            emit(PlayerEvent::Failed(format!(
                "failed to connect to mpv IPC socket: {err}"
            )));
            let _ = child.kill().await;
            return;
        }
    };
    let (reader, writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let mut ipc = Ipc {
        writer,
        request_id: 0,
    };

    // Everything below returns early only through the loop; errors while
    // driving mpv are reported but non-fatal where jfsh treated them so.
    if let Err(err) = ipc.send(&ipc::observe_property_cmd(1, "time-pos")).await {
        emit(PlayerEvent::Failed(format!("mpv IPC write failed: {err}")));
        let _ = child.kill().await;
        return;
    }
    let _ = ipc.send(&ipc::observe_property_cmd(2, "duration")).await;
    // aid/sid feed the track-switch memory; track-list keeps a local cache
    // so a switched track id can be mapped back to its language without
    // request/reply matching.
    let _ = ipc.send(&ipc::observe_property_cmd(3, "aid")).await;
    let _ = ipc.send(&ipc::observe_property_cmd(4, "sid")).await;
    let _ = ipc.send(&ipc::observe_property_cmd(5, "track-list")).await;
    // Pause drives the keepalive: while paused, mpv stops emitting time-pos,
    // so the time-pos-driven progress reporting goes silent.
    let _ = ipc.send(&ipc::observe_property_cmd(6, "pause")).await;

    for command in language_setup_cmds(&prefs) {
        if let Err(err) = ipc.send(&command).await {
            tracing::error!(%err, "failed to apply language preferences");
        }
    }

    // Load the playlist. mpv assigns `playlist_entry_id`s 1,2,3,... in the
    // order entries are created — one per loadfile we actually dispatch, in
    // send order, regardless of append/insert-at position. So `loaded` holds
    // the item index behind each created entry, and `loaded[entry_id - 1]`
    // maps a start-file back to its item. A skipped loadfile (unbuildable URL
    // or a failed IPC write) creates no entry, so it is left out of `loaded`
    // too — keeping the two in lockstep instead of letting one skipped entry
    // offset every later mapping.
    let playlist = load_order(items.len(), index, !old_mpv);
    let mut loaded: Vec<usize> = Vec::with_capacity(playlist.len());
    for (position, &item_index) in playlist.iter().enumerate() {
        let item = &items[item_index];
        let url = match stream_url(&client.host, &item.id) {
            Ok(url) => url,
            Err(err) => {
                tracing::error!(%err, item = item.id, "failed to build streaming url");
                continue;
            }
        };
        let title = display::media_title(item);
        let command = if position == 0 {
            let start = ticks_to_seconds(display::resume_position_ticks(item));
            ipc::play_file_cmd(&url, &title, start, old_mpv, &client.token)
        } else if item_index > index {
            ipc::append_file_cmd(&url, &title, old_mpv, &client.token)
        } else {
            ipc::prepend_file_cmd(&url, &title, &client.token)
        };
        match ipc.send(&command).await {
            Ok(()) => loaded.push(item_index),
            Err(err) => tracing::error!(%err, "failed to load file into mpv"),
        }
    }

    let mut current = items[index].clone();
    // Seed `pos` with the selected item's resume position so a stop that lands
    // before the first `start-file` (mpv boot / a stream that never loads)
    // reports the resume ticks back, not 0, which would clobber the server-side
    // resume point. The `start-file` handler re-sets `pos` the same way once
    // playback actually begins.
    let mut pos = ticks_to_seconds(display::resume_position_ticks(&current));
    let mut skippable: Vec<(f64, f64)> = Vec::new();
    let mut last_report = Instant::now();
    // Whether mpv is currently paused, tracked from the `pause` property so a
    // long pause keeps sending keepalive progress reports (see the keepalive
    // arm below) instead of letting Jellyfin expire the play session.
    let mut paused = false;
    // Set on seek: send a progress report on the next time-pos even if the
    // debounce interval has not elapsed.
    let mut report_due = false;
    let mut last_emitted_sec = i64::MIN;
    let mut quit_deadline: Option<Instant> = None;
    let mut cmd_open = true;
    // Track-switch memory. aid/sid changes count as user actions only inside
    // the window between file-loaded and the next start-file/end-file:
    // everything outside it is the observe registration echo, mpv's per-file
    // auto-selection, or our own setup/reapply churn.
    let mut session_prefs = prefs;
    let mut prefs_dirty = false;
    let mut user_window = false;
    let mut tracks: Vec<ipc::Track> = Vec::new();

    // Background tasks report a 401 here; the supervisor owns `emit` and
    // raises SessionExpired (once) so the app can run its re-login flow.
    let (unauthorized_tx, mut unauthorized_rx) = mpsc::unbounded_channel::<()>();
    let mut session_expired = false;

    let (report_tx, reporter) = spawn_reporter(client.clone(), unauthorized_tx.clone());
    // Media segment fetches land here; tagged with the item id so a result
    // arriving after an auto-advance is dropped instead of applied.
    let (seg_tx, mut seg_rx) =
        mpsc::unbounded_channel::<(String, Vec<crate::jellyfin::MediaSegment>)>();

    // Keepalive tick: fires on the same cadence as the time-pos debounce, but
    // only sends a report while paused (playing is already covered by the
    // time-pos handler). Skip missed ticks so a slow iteration cannot burst a
    // backlog of reports.
    let mut keepalive = tokio::time::interval(PROGRESS_REPORT_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    // EOF or read error: mpv is gone.
                    Ok(None) | Err(_) => break,
                };
                if line.is_empty() {
                    continue;
                }
                let Some(msg) = ipc::parse_message(&line) else {
                    continue;
                };
                // Command replies: surface failures (e.g. a rejected loadfile).
                if msg.event.is_none() {
                    if let Some(error) = msg.error.as_deref().filter(|&e| e != "success") {
                        // error is one of mpv's fixed IPC status strings
                        // ("invalid parameter", etc.), never a token or
                        // user-derived value, so it is safe to log raw. Only
                        // line (the full reply) can carry the subtitle api_key
                        // or X-Emby-Token.
                        tracing::warn!(error, line = %ipc::redact_secrets(&line), "mpv command failed");
                    }
                    continue;
                }
                match msg.event.as_deref() {
                    Some("property-change") => match msg.name.as_deref() {
                        Some("time-pos") => {
                            let Some(data) = msg.data.as_ref().and_then(Value::as_f64) else {
                                continue;
                            };
                            pos = data;

                            if let Some(end) = inside_skippable_segment(&skippable, pos) {
                                if let Err(err) = ipc.send(&ipc::seek_cmd(end)).await {
                                    tracing::error!(%err, "failed to seek past segment");
                                } else {
                                    tracing::info!(end, "skipped media segment");
                                }
                            }

                            if report_due || last_report.elapsed() > PROGRESS_REPORT_INTERVAL {
                                // Fire-and-forget; the debounce advances no
                                // matter how the report fares, so a down
                                // server is attempted once per interval, not
                                // on every time-pos event.
                                let _ = report_tx.send(Report::Progress {
                                    item_id: current.id.clone(),
                                    ticks: seconds_to_ticks(pos),
                                    is_paused: paused,
                                });
                                report_due = false;
                                last_report = Instant::now();
                            }

                            let second = pos.floor() as i64;
                            if second != last_emitted_sec {
                                last_emitted_sec = second;
                                emit(PlayerEvent::Position { secs: pos });
                            }
                        }
                        Some("duration") => {
                            if let Some(secs) = msg.data.as_ref().and_then(Value::as_f64) {
                                emit(PlayerEvent::Duration { secs });
                            }
                        }
                        Some("track-list") => {
                            if let Some(data) = msg.data.as_ref() {
                                tracks = ipc::parse_track_list(data);
                            }
                        }
                        Some(name @ ("aid" | "sid")) => {
                            if !user_window {
                                continue;
                            }
                            let kind = if name == "aid" {
                                TrackKind::Audio
                            } else {
                                TrackKind::Subtitle
                            };
                            let Some(selection) =
                                classify_selection(kind, msg.data.as_ref(), &tracks)
                            else {
                                continue;
                            };
                            let field = match kind {
                                TrackKind::Audio => &mut session_prefs.audio,
                                TrackKind::Subtitle => &mut session_prefs.subtitles,
                            };
                            // mpv can repeat the property; only a genuine
                            // change is remembered (this also bounds config
                            // writes to distinct switches).
                            if field.as_ref() == Some(&selection) {
                                continue;
                            }
                            *field = Some(selection.clone());
                            prefs_dirty = true;
                            tracing::info!(?kind, ?selection, "user switched track");
                            emit(PlayerEvent::TrackSwitched {
                                override_key: override_key(&current).to_string(),
                                kind,
                                selection,
                            });
                        }
                        Some("pause") => {
                            // Report the pause/resume immediately so the server
                            // reflects it without waiting for the next keepalive
                            // tick. mpv can repeat the property, so only a
                            // genuine change reports.
                            if let Some(is_paused) = msg.data.as_ref().and_then(Value::as_bool)
                                && is_paused != paused
                            {
                                paused = is_paused;
                                let _ = report_tx.send(Report::Progress {
                                    item_id: current.id.clone(),
                                    ticks: seconds_to_ticks(pos),
                                    is_paused: paused,
                                });
                                last_report = Instant::now();
                            }
                        }
                        _ => {}
                    },

                    Some("start-file") => {
                        user_window = false;
                        let entry = msg.playlist_entry_id.unwrap_or(0) - 1;
                        let Some(&item_index) = usize::try_from(entry)
                            .ok()
                            .and_then(|entry| loaded.get(entry))
                        else {
                            // jfsh aborted the whole session here; keep playing
                            // with the last known item instead.
                            tracing::warn!(entry, "start-file for unknown playlist entry");
                            continue;
                        };
                        current = items[item_index].clone();
                        tracing::info!(item = %current.id, "start-file");

                        // `pos` still holds the previous file's position here.
                        // Only the initially selected file was loaded with a
                        // start= option (its resume position); every other
                        // playlist entry begins at 0.
                        pos = if item_index == index {
                            ticks_to_seconds(display::resume_position_ticks(&current))
                        } else {
                            0.0
                        };

                        let _ = report_tx.send(Report::Start {
                            item_id: current.id.clone(),
                            ticks: seconds_to_ticks(pos),
                        });

                        skippable.clear();
                        if !skip_types.is_empty() {
                            let client = client.clone();
                            let item_id = current.id.clone();
                            let skip_types = skip_types.clone();
                            let seg_tx = seg_tx.clone();
                            let unauthorized_tx = unauthorized_tx.clone();
                            tokio::spawn(async move {
                                match client.get_media_segments(&item_id, &skip_types).await {
                                    Ok(segments) => {
                                        let _ = seg_tx.send((item_id, segments));
                                    }
                                    Err(crate::jellyfin::Error::Unauthorized) => {
                                        let _ = unauthorized_tx.send(());
                                    }
                                    Err(err) => {
                                        tracing::error!(%err, "failed to get media segments");
                                    }
                                }
                            });
                        }

                        for subtitle in display::external_subtitles(&current) {
                            // sub-add takes no per-file options, so the token
                            // must ride in the URL; redact_secrets keeps it
                            // out of the debug log.
                            let url = format!(
                                "{}{}?api_key={}",
                                client.host, subtitle.path, client.token
                            );
                            if let Err(err) = ipc
                                .send(&ipc::sub_add_cmd(&url, &subtitle.title, &subtitle.language))
                                .await
                            {
                                tracing::error!(%err, title = subtitle.title, "failed to add subtitle");
                            }
                        }

                        last_emitted_sec = i64::MIN;
                        emit(PlayerEvent::Started {
                            title: display::media_title(&current),
                        });
                    }

                    Some("seek") => {
                        // Report on the next time-pos regardless of the
                        // debounce, like jfsh resetting its timer on seek.
                        report_due = true;
                    }

                    Some("file-loaded") => {
                        // mpv finished this file's own track selection; any
                        // aid/sid change from here on is the user's doing.
                        user_window = true;
                    }

                    Some("end-file") => {
                        user_window = false;
                        // A switch happened during the finished file: point
                        // alang/slang at the new choice and put aid/sid back
                        // on auto so the next entry selects by language, not
                        // by a track id that means something else there.
                        if prefs_dirty {
                            prefs_dirty = false;
                            for command in language_reapply_cmds(&session_prefs) {
                                if let Err(err) = ipc.send(&command).await {
                                    tracing::error!(%err, "failed to reapply language preferences");
                                }
                            }
                        }
                        let _ = report_tx.send(Report::Stopped {
                            item_id: current.id.clone(),
                            ticks: seconds_to_ticks(pos),
                        });
                    }

                    Some("shutdown") => {
                        user_window = false;
                        let _ = report_tx.send(Report::Stopped {
                            item_id: current.id.clone(),
                            ticks: seconds_to_ticks(pos),
                        });
                    }

                    _ => {}
                }
            }

            _ = keepalive.tick() => {
                // While paused, time-pos is frozen and the reporting above goes
                // silent; send a keepalive on the report cadence so the session
                // stays alive and the dashboard reflects the paused state.
                if paused {
                    let _ = report_tx.send(Report::Progress {
                        item_id: current.id.clone(),
                        ticks: seconds_to_ticks(pos),
                        is_paused: true,
                    });
                }
            }

            _ = unauthorized_rx.recv(), if !session_expired => {
                session_expired = true;
                tracing::info!("session expired during playback");
                emit(PlayerEvent::SessionExpired);
            }

            segments = seg_rx.recv() => {
                if let Some((item_id, segments)) = segments
                    && item_id == current.id
                {
                    skippable = segments
                        .iter()
                        .map(|s| (ticks_to_seconds(s.start_ticks), ticks_to_seconds(s.end_ticks)))
                        .collect();
                }
            }

            cmd = cmd_rx.recv(), if cmd_open => {
                if cmd.is_none() {
                    cmd_open = false;
                    continue;
                }
                // Only command: Stop. Ask mpv to quit and keep draining events
                // (shutdown -> report stopped) until EOF.
                if quit_deadline.is_none() {
                    quit_deadline = Some(Instant::now() + QUIT_GRACE);
                    if ipc.send(&ipc::quit_cmd()).await.is_err() {
                        break;
                    }
                }
            }

            _ = async { tokio::time::sleep_until(quit_deadline.unwrap()).await },
                if quit_deadline.is_some() => {
                tracing::warn!("mpv did not exit after quit; killing it");
                break;
            }
        }
    }

    let _ = child.kill().await;
    let _ = child.wait().await;
    // The socket is removed by `_socket_guard` when this function returns.

    // Let queued reports (typically the final stopped) land before the
    // caller emits Exited and the UI refetches watch state; bounded so a
    // dead server cannot hold shutdown hostage.
    drop(report_tx);
    if tokio::time::timeout(REPORT_FLUSH_GRACE, reporter)
        .await
        .is_err()
    {
        tracing::warn!("gave up waiting for final playback reports");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_budget_covers_supervisor() {
        // The shell's drain deadline uses SHUTDOWN_BUDGET; it must cover the
        // supervisor's own worst case so a slow quit is not abandoned mid-flush.
        assert!(SHUTDOWN_BUDGET >= QUIT_GRACE + REPORT_FLUSH_GRACE);
    }

    #[test]
    fn load_order_middle_of_series() {
        // Selected 3 of 0..6: play 3, append 4,5, prepend 2,1,0.
        assert_eq!(load_order(6, 3, true), vec![3, 4, 5, 2, 1, 0]);
    }

    #[test]
    fn load_order_first_and_last() {
        assert_eq!(load_order(4, 0, true), vec![0, 1, 2, 3]);
        assert_eq!(load_order(4, 3, true), vec![3, 2, 1, 0]);
    }

    #[test]
    fn load_order_old_mpv_skips_prior() {
        assert_eq!(load_order(6, 3, false), vec![3, 4, 5]);
    }

    #[test]
    fn load_order_single_item() {
        assert_eq!(load_order(1, 0, true), vec![0]);
    }

    #[test]
    fn skippable_segment_lookup() {
        let segments = vec![(10.0, 40.0), (300.0, 330.0)];
        assert_eq!(inside_skippable_segment(&segments, 15.0), Some(40.0));
        assert_eq!(inside_skippable_segment(&segments, 40.0), None);
        assert_eq!(inside_skippable_segment(&segments, 299.9), None);
        assert_eq!(inside_skippable_segment(&segments, 300.0), Some(330.0));
        assert_eq!(inside_skippable_segment(&segments, 5.0), None);
    }

    fn prefs(audio: Option<TrackPreference>, subtitles: Option<TrackPreference>) -> LanguagePrefs {
        LanguagePrefs { audio, subtitles }
    }

    fn shapes(cmds: &[Vec<Value>]) -> Vec<String> {
        cmds.iter()
            .map(|cmd| serde_json::to_string(cmd).unwrap())
            .collect()
    }

    #[test]
    fn setup_cmds_per_preference() {
        assert!(language_setup_cmds(&prefs(None, None)).is_empty());
        // Default track = leave mpv entirely alone.
        assert!(language_setup_cmds(&prefs(Some(TrackPreference::Default), None)).is_empty());

        let cmds = language_setup_cmds(&prefs(
            Some(TrackPreference::Language("ita".into())),
            Some(TrackPreference::Off),
        ));
        assert_eq!(
            shapes(&cmds),
            vec![
                r#"["set_property","alang","ita,it"]"#,
                r#"["set_property","sid","no"]"#,
            ]
        );

        let cmds = language_setup_cmds(&prefs(None, Some(TrackPreference::Language("ger".into()))));
        assert_eq!(
            shapes(&cmds),
            vec![r#"["set_property","slang","ger,deu,de"]"#]
        );
    }

    #[test]
    fn reapply_cmds_reset_track_ids() {
        assert!(language_reapply_cmds(&prefs(None, None)).is_empty());

        let cmds = language_reapply_cmds(&prefs(
            Some(TrackPreference::Language("jpn".into())),
            Some(TrackPreference::Off),
        ));
        assert_eq!(
            shapes(&cmds),
            vec![
                r#"["set_property","alang","jpn,ja"]"#,
                r#"["set_property","aid","auto"]"#,
                r#"["set_property","slang",""]"#,
                r#"["set_property","sid","no"]"#,
            ]
        );

        // A switch back to the default track clears alang so the next file
        // is not still steered by the old preference.
        let cmds = language_reapply_cmds(&prefs(
            Some(TrackPreference::Default),
            Some(TrackPreference::Language("eng".into())),
        ));
        assert_eq!(
            shapes(&cmds),
            vec![
                r#"["set_property","alang",""]"#,
                r#"["set_property","aid","auto"]"#,
                r#"["set_property","slang","eng,en"]"#,
                r#"["set_property","sid","auto"]"#,
            ]
        );
    }

    #[test]
    fn classify_selection_table() {
        let tracks = ipc::parse_track_list(&serde_json::json!([
            {"id": 1, "type": "audio", "lang": "jpn"},
            {"id": 2, "type": "audio"},
            {"id": 1, "type": "sub", "lang": "en"},
        ]));

        // Tagged track: remembered under its canonical /B code.
        assert_eq!(
            classify_selection(TrackKind::Audio, Some(&serde_json::json!(1)), &tracks),
            Some(TrackPreference::Language("jpn".into()))
        );
        assert_eq!(
            classify_selection(TrackKind::Subtitle, Some(&serde_json::json!(1)), &tracks),
            Some(TrackPreference::Language("eng".into()))
        );
        // Untagged track, unknown id, "auto", null: nothing to remember.
        assert_eq!(
            classify_selection(TrackKind::Audio, Some(&serde_json::json!(2)), &tracks),
            None
        );
        assert_eq!(
            classify_selection(TrackKind::Audio, Some(&serde_json::json!(9)), &tracks),
            None
        );
        assert_eq!(
            classify_selection(TrackKind::Audio, Some(&serde_json::json!("auto")), &tracks),
            None
        );
        assert_eq!(classify_selection(TrackKind::Audio, None, &tracks), None);
        // Off: a subtitle choice, not an audio one.
        assert_eq!(
            classify_selection(
                TrackKind::Subtitle,
                Some(&serde_json::json!(false)),
                &tracks
            ),
            Some(TrackPreference::Off)
        );
        assert_eq!(
            classify_selection(TrackKind::Subtitle, Some(&serde_json::json!("no")), &tracks),
            Some(TrackPreference::Off)
        );
        assert_eq!(
            classify_selection(TrackKind::Audio, Some(&serde_json::json!(false)), &tracks),
            None
        );
    }
}
