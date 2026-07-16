use crate::jellyfin::{Client, Error, ItemsResponse, MediaItem};

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
    /// One page of a Libraries-tab listing. `start_index == 0` replaces the
    /// list, anything else appends; `fetch_gen` drops pages superseded by a
    /// sort, filter, or navigation change. `limit` is the page size that was
    /// requested, so a failed page can be retried with the same size (a
    /// position-preserving refresh uses a larger limit than `PAGE_SIZE`).
    LibraryItemsLoaded {
        fetch_gen: u64,
        start_index: usize,
        limit: usize,
        result: Result<ItemsResponse, Error>,
    },
    /// Result of loading a single series item, to open a show from a hub
    /// episode (which carries only the series id, not the series item).
    /// `fetch_gen` drops a result superseded by a newer navigation. The item is
    /// boxed so this variant doesn't inflate `Msg`'s size (an unboxed
    /// `MediaItem` is far larger than the other variants' payloads).
    SeriesLoaded {
        fetch_gen: u64,
        result: Result<Box<MediaItem>, Error>,
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
