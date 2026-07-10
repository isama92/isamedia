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

use tokio::sync::mpsc;

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

pub struct PlayerHandle {
    cmd_tx: mpsc::UnboundedSender<PlayerCommand>,
    pub now: NowPlaying,
}

impl PlayerHandle {
    /// Ask mpv to quit; the supervisor reports the final position and emits
    /// `Exited` when done.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(PlayerCommand::Stop);
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
pub fn spawn(
    client: Client,
    item: MediaItem,
    skip_types: Vec<String>,
    prefs: LanguagePrefs,
    emit: impl Fn(PlayerEvent) + Send + Sync + 'static,
) -> PlayerHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let now = NowPlaying {
        title: display::media_title(&item),
        ..NowPlaying::default()
    };
    tokio::spawn(async move {
        let (items, index) = if item.kind == ItemKind::Episode {
            match client.get_episodes(&item).await {
                Ok(episodes) if !episodes.is_empty() => {
                    let index = episodes
                        .iter()
                        .position(|episode| episode.id == item.id)
                        .unwrap_or(0);
                    (episodes, index)
                }
                Ok(_) => (vec![item], 0),
                Err(crate::jellyfin::Error::Unauthorized) => {
                    emit(PlayerEvent::SessionExpired);
                    emit(PlayerEvent::Exited);
                    return;
                }
                Err(err) => {
                    emit(PlayerEvent::Failed(format!(
                        "failed to fetch episodes: {err}"
                    )));
                    emit(PlayerEvent::Exited);
                    return;
                }
            }
        } else {
            (vec![item], 0)
        };
        supervisor::run(client, items, index, skip_types, prefs, cmd_rx, &emit).await;
        emit(PlayerEvent::Exited);
    });
    PlayerHandle { cmd_tx, now }
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
