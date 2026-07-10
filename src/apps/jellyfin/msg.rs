use crate::jellyfin::{Client, Error, MediaItem};

/// Messages sent back to the Jellyfin app by its spawned tasks. Carried
/// through the shell as `Box<dyn Any>` and downcast in `on_event`.
pub enum Msg {
    /// Result of an authentication/connect attempt. `gen` guards against a
    /// stale result arriving after the user cancelled back to the login form.
    AuthDone {
        auth_gen: u64,
        result: Result<Client, Error>,
    },
    /// Result of a list fetch; `fetch_gen` drops results superseded by a
    /// newer fetch (fast tab switching).
    ItemsLoaded {
        fetch_gen: u64,
        result: Result<Vec<MediaItem>, Error>,
    },
    /// Result of a watched/unwatched toggle; `fetch_gen` is the view
    /// generation the toggle was issued from, so a result landing after the
    /// user moved to another tab or series is dropped.
    WatchedToggled {
        fetch_gen: u64,
        result: Result<(), Error>,
    },
    /// Playback lifecycle from the mpv supervisor; `player_gen` distinguishes
    /// the current player from a replaced one that is still shutting down.
    Player {
        player_gen: u64,
        event: crate::player::PlayerEvent,
    },
    /// A background keyring write/delete failed; surfaced on the error line.
    KeyringError(String),
}
