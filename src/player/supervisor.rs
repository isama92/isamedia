//! The playback supervisor: owns the mpv process, speaks JSON IPC, reports
//! playback state to Jellyfin, auto-skips configured media segments, and
//! injects external subtitles. Ported from jfsh's internal/mpv/play.go.

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::jellyfin::{Client, MediaItem, display, url::stream_url};

use super::ipc::{self, IpcStream};
use super::ticks::{seconds_to_ticks, ticks_to_seconds};
use super::{PlayerCommand, PlayerEvent};

const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(3);
/// How long to wait for mpv to exit after we ask it to quit.
const QUIT_GRACE: Duration = Duration::from_secs(5);

static OLD_MPV: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();

async fn is_old_mpv() -> bool {
    *OLD_MPV
        .get_or_init(|| async {
            match tokio::process::Command::new("mpv")
                .arg("--version")
                .output()
                .await
            {
                Ok(output) => {
                    let old = ipc::is_old_mpv_version(&String::from_utf8_lossy(&output.stdout));
                    if old {
                        tracing::warn!(
                            "mpv is older than 0.38.0; earlier episodes will not be \
                             prepended to the playlist"
                        );
                    }
                    old
                }
                Err(err) => {
                    tracing::debug!(%err, "failed to run mpv --version");
                    false
                }
            }
        })
        .await
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
    Start { item_id: String, ticks: i64 },
    Progress { item_id: String, ticks: i64 },
    Stopped { item_id: String, ticks: i64 },
}

/// Reporter task: runs reports sequentially, preserving order. A backlog of
/// progress reports for the same item collapses to the newest one, so an
/// outage (enqueue every 3s, 30s timeout per attempt) cannot grow the queue.
fn spawn_reporter(client: Client) -> (mpsc::UnboundedSender<Report>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Report>();
    let task = tokio::spawn(async move {
        while let Some(mut report) = rx.recv().await {
            while let Ok(next) = rx.try_recv() {
                match (&report, &next) {
                    (Report::Progress { item_id: a, .. }, Report::Progress { item_id: b, .. })
                        if a == b =>
                    {
                        report = next;
                    }
                    _ => {
                        send_report(&client, report).await;
                        report = next;
                    }
                }
            }
            send_report(&client, report).await;
        }
    });
    (tx, task)
}

async fn send_report(client: &Client, report: Report) {
    let (kind, item, result) = match &report {
        Report::Start { item_id, ticks } => (
            "start",
            item_id,
            client.report_playback_start(item_id, *ticks).await,
        ),
        Report::Progress { item_id, ticks } => (
            "progress",
            item_id,
            client.report_playback_progress(item_id, *ticks).await,
        ),
        Report::Stopped { item_id, ticks } => (
            "stopped",
            item_id,
            client.report_playback_stopped(item_id, *ticks).await,
        ),
    };
    match result {
        Ok(()) => tracing::info!(kind, item = %item, "reported playback state"),
        Err(err) => tracing::error!(%err, kind, item = %item, "failed to report playback state"),
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
    mut cmd_rx: mpsc::UnboundedReceiver<PlayerCommand>,
    emit: &(impl Fn(PlayerEvent) + Send + Sync),
) {
    let old_mpv = is_old_mpv().await;

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ (std::process::id() as u128);
    let socket = match ipc::socket_path(unique) {
        Ok(socket) => socket,
        Err(err) => {
            emit(PlayerEvent::Failed(format!(
                "failed to prepare mpv IPC socket directory: {err}"
            )));
            return;
        }
    };

    let mut child = match tokio::process::Command::new("mpv")
        .arg("--idle")
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
            // No socket exists yet, but the fallback 0700 directory does.
            ipc::cleanup_socket(&socket);
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
            ipc::cleanup_socket(&socket);
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
        ipc::cleanup_socket(&socket);
        return;
    }
    let _ = ipc.send(&ipc::observe_property_cmd(2, "duration")).await;

    // Load the playlist; `playlist[entry_id - 1]` maps back to an item index.
    let playlist = load_order(items.len(), index, !old_mpv);
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
        if let Err(err) = ipc.send(&command).await {
            tracing::error!(%err, "failed to load file into mpv");
        }
    }

    let mut pos = 0.0_f64;
    let mut current = items[index].clone();
    let mut skippable: Vec<(f64, f64)> = Vec::new();
    let mut last_report = Instant::now();
    let mut last_emitted_sec = i64::MIN;
    let mut quit_deadline: Option<Instant> = None;
    let mut cmd_open = true;

    let (report_tx, reporter) = spawn_reporter(client.clone());
    // Media segment fetches land here; tagged with the item id so a result
    // arriving after an auto-advance is dropped instead of applied.
    let (seg_tx, mut seg_rx) =
        mpsc::unbounded_channel::<(String, Vec<crate::jellyfin::MediaSegment>)>();

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
                        tracing::warn!(error, line, "mpv command failed");
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

                            if last_report.elapsed() > PROGRESS_REPORT_INTERVAL {
                                // Fire-and-forget; the debounce advances no
                                // matter how the report fares, so a down
                                // server is attempted once per interval, not
                                // on every time-pos event.
                                let _ = report_tx.send(Report::Progress {
                                    item_id: current.id.clone(),
                                    ticks: seconds_to_ticks(pos),
                                });
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
                        _ => {}
                    },

                    Some("start-file") => {
                        let entry = msg.playlist_entry_id.unwrap_or(0) - 1;
                        let Some(&item_index) = usize::try_from(entry)
                            .ok()
                            .and_then(|entry| playlist.get(entry))
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
                            tokio::spawn(async move {
                                match client.get_media_segments(&item_id, &skip_types).await {
                                    Ok(segments) => {
                                        let _ = seg_tx.send((item_id, segments));
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
                        // Report immediately after a seek: pretend the last
                        // report is old, like jfsh resetting its debounce.
                        last_report = Instant::now()
                            .checked_sub(PROGRESS_REPORT_INTERVAL + Duration::from_secs(1))
                            .unwrap_or_else(Instant::now);
                    }

                    Some("end-file") | Some("shutdown") => {
                        let _ = report_tx.send(Report::Stopped {
                            item_id: current.id.clone(),
                            ticks: seconds_to_ticks(pos),
                        });
                    }

                    _ => {}
                }
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
    ipc::cleanup_socket(&socket);

    // Let queued reports (typically the final stopped) land before the
    // caller emits Exited and the UI refetches watch state; bounded so a
    // dead server cannot hold shutdown hostage.
    drop(report_tx);
    if tokio::time::timeout(Duration::from_secs(5), reporter)
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
}
