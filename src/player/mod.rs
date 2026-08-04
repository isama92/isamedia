//! Non-blocking mpv playback: `spawn` launches a supervisor task that owns
//! the mpv process and its IPC socket, reports playback state to Jellyfin,
//! and streams `PlayerEvent`s back to the UI. The UI keeps a `PlayerHandle`
//! and stays fully interactive.

pub mod ipc;
mod supervisor;
pub mod ticks;

/// Shutdown budget the shell's quit drain must respect; defined by the
/// supervisor, which owns the mpv-quit and report-flush timeouts it sums.
pub(crate) use supervisor::SHUTDOWN_BUDGET;

use tokio::sync::{mpsc, oneshot};

use crate::config::{LanguagePrefs, TrackPreference};
use crate::jellyfin::{Client, MediaItem, display, models::ItemKind};

#[derive(Debug, Clone, Copy)]
pub enum PlayerCommand {
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Subtitle,
}

#[derive(Debug)]
pub enum PlayerEvent {
    /// A (new) file started playing in mpv.
    Started {
        title: String,
    },
    /// Whole-second position updates.
    Position {
        secs: f64,
    },
    Duration {
        secs: f64,
    },
    /// The user switched a track inside mpv (auto-selection at file load is
    /// filtered out by the supervisor). `override_key` is the config key the
    /// choice should persist under: the movie id, or the series id for
    /// episodes.
    TrackSwitched {
        override_key: String,
        kind: TrackKind,
        selection: TrackPreference,
    },
    /// Something went wrong; shown as the browse error line.
    Failed(String),
    /// The server answered 401 to a playback-path request (episode fetch,
    /// playstate report, segment fetch): the session token is dead and the
    /// app should run its re-login flow.
    SessionExpired,
    /// The player is gone. Always the final event, exactly once.
    Exited,
}

/// UI-side cache of what is playing, fed from `PlayerEvent`s.
#[derive(Debug, Clone, Default)]
pub struct NowPlaying {
    pub title: String,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
}

/// Orders one playback's reports behind those of the player it replaced: the
/// incoming reporter holds its first report until the outgoing player's task
/// has ended, which happens only after mpv is reaped and its own reports have
/// flushed.
///
/// Without it, restarting the *same* item lets the outgoing player's final
/// `Stopped { ticks: <old position> }` reach the server after the incoming
/// player's `Start { ticks: <resume position> }`, leaving a stale resume point
/// behind until the next progress report overwrites it.
pub struct ReportGate {
    done: oneshot::Receiver<()>,
    /// Cut-off for the wait, absolute and stamped at hand-over rather than
    /// where the reporter starts waiting: the first report comes after
    /// `ipc::connect`, which polls for up to 30s, and an absolute cut-off also
    /// keeps a chain of rapid replaces bounded by one grace instead of one per
    /// link.
    deadline: tokio::time::Instant,
}

impl ReportGate {
    fn armed(done: oneshot::Receiver<()>) -> Self {
        Self {
            done,
            deadline: tokio::time::Instant::now() + supervisor::REPORT_GATE_GRACE,
        }
    }

    /// Nothing is ever *sent* down the channel: the sender drops when the old
    /// player's task ends, so `Ok` and `Err` both mean "that player is done"
    /// and a panicking supervisor needs no special handling.
    ///
    /// Bounded because a wedged old player must not silence the new one's
    /// reports for good. Past the deadline we give up on ordering rather than
    /// drop reports, the same trade-off the report flush in `supervisor::run`
    /// already makes.
    async fn wait(self) {
        if tokio::time::timeout_at(self.deadline, self.done)
            .await
            .is_err()
        {
            tracing::warn!("previous player still running; reporting playback state unordered");
        }
    }
}

pub struct PlayerHandle {
    cmd_tx: mpsc::UnboundedSender<PlayerCommand>,
    /// Resolves when this player's task ends; handed on as a `ReportGate` to
    /// the player that replaces this one.
    finished: Option<oneshot::Receiver<()>>,
    pub now: NowPlaying,
}

impl PlayerHandle {
    /// Ask mpv to quit; the supervisor reports the final position and emits
    /// `Exited` when done.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(PlayerCommand::Stop);
    }

    /// Hand this player's ordering gate to the one replacing it, arming its
    /// deadline now. Only meaningful once `stop()` has been called, since the
    /// gate resolves when this player's task ends; pass it straight to `spawn`
    /// and never await it on the render thread.
    pub fn take_gate(&mut self) -> Option<ReportGate> {
        self.finished.take().map(ReportGate::armed)
    }
}

/// The key a remembered track switch persists under: whole-show for
/// episodes (a Series item's own id equals its episodes' series id, so the
/// two always agree), per-movie otherwise.
pub fn override_key(item: &MediaItem) -> &str {
    item.series_id.as_deref().unwrap_or(&item.id)
}

/// Start playing `item`. Episodes are expanded to their full series as an
/// mpv playlist positioned at the selected episode, like jfsh.
///
/// `after` is the gate of the player this one replaces, if any: mpv starts
/// immediately either way, only the playback reports wait on it (see
/// `ReportGate`).
pub fn spawn(
    client: Client,
    item: MediaItem,
    skip_types: Vec<String>,
    prefs: LanguagePrefs,
    after: Option<ReportGate>,
    emit: impl Fn(PlayerEvent) + Send + Sync + 'static,
) -> PlayerHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    let now = NowPlaying {
        title: display::media_title(&item),
        ..NowPlaying::default()
    };
    tokio::spawn(async move {
        // Held rather than sent on: it drops when this task ends, which is
        // after the report flush inside `run`, so the next player unblocks at
        // the right moment on every exit path below, panics included.
        let _finished_tx = finished_tx;
        // `run` takes the gate only once it has a reporter to hold behind it.
        // Anything left here means this player never reported at all, and
        // dropping it would let the *next* player report while the one we
        // replaced is still flushing, so it is honoured before this task ends.
        let mut after = after;
        // `None` means playback never started; the event explaining why has
        // already been emitted.
        let expanded = if item.kind == ItemKind::Episode {
            match client.get_episodes(&item).await {
                Ok(episodes) if !episodes.is_empty() => {
                    let index = episodes
                        .iter()
                        .position(|episode| episode.id == item.id)
                        .unwrap_or(0);
                    Some(supervisor::Playlist {
                        items: episodes,
                        index,
                    })
                }
                Ok(_) => Some(supervisor::Playlist {
                    items: vec![item],
                    index: 0,
                }),
                Err(crate::jellyfin::Error::Unauthorized) => {
                    emit(PlayerEvent::SessionExpired);
                    None
                }
                Err(err) => {
                    emit(PlayerEvent::Failed(format!(
                        "failed to fetch episodes: {err}"
                    )));
                    None
                }
            }
        } else {
            Some(supervisor::Playlist {
                items: vec![item],
                index: 0,
            })
        };
        if let Some(playlist) = expanded {
            supervisor::run(
                client, playlist, skip_types, prefs, cmd_rx, &mut after, &emit,
            )
            .await;
        }
        emit(PlayerEvent::Exited);
        // Emitted first: the UI is never held behind another player's reports.
        if let Some(after) = after {
            after.wait().await;
        }
    });
    PlayerHandle {
        cmd_tx,
        finished: Some(finished_rx),
        now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_key_prefers_series() {
        let movie = MediaItem {
            id: "movie-1".into(),
            ..MediaItem::default()
        };
        assert_eq!(override_key(&movie), "movie-1");

        let episode = MediaItem {
            id: "ep-9".into(),
            series_id: Some("show-3".into()),
            ..MediaItem::default()
        };
        assert_eq!(override_key(&episode), "show-3");
    }
}
