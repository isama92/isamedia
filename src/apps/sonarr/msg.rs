//! Results of spawned async work, sent back into the event loop. Every
//! variant that answers a supersedable request carries the generation it was
//! issued under; handlers drop stale generations FIRST, before looking at
//! the result (a stale 401 belongs to a key that may since have been
//! replaced and must not kick the fresh session).

use crate::sonarr::{
    Client, Command, Episode, Error, HistoryRecord, QualityProfile, QueueItem, Release, RootFolder,
    Series,
};

pub enum Msg {
    ConnectDone {
        auth_gen: u64,
        result: Result<Client, Error>,
    },
    SeriesLoaded {
        fetch_gen: u64,
        result: Result<Vec<Series>, Error>,
    },
    EpisodesLoaded {
        fetch_gen: u64,
        result: Result<Vec<Episode>, Error>,
    },
    /// Results of an add-flow lookup (series/lookup), as raw JSON so the whole
    /// object can be forwarded to the add. Its own generation, so a lookup
    /// abandoned by leaving the add screen can't surface on the list.
    LookupLoaded {
        add_lookup_gen: u64,
        result: Result<Vec<serde_json::Value>, Error>,
    },
    /// Root folders + quality profiles for the add wizard, fetched on its
    /// own generation so a lookup can't invalidate it.
    AddOptionsLoaded {
        prereq_gen: u64,
        result: Result<(Vec<RootFolder>, Vec<QualityProfile>), Error>,
    },
    /// A series was added; on success the created series (with its new id) is
    /// what the show page opens on. Its own generation, bumped when the user
    /// leaves the add screen, so a committed add never yanks them back. Boxed
    /// to keep the enum variants a similar size (a bare `Series` dwarfs the
    /// others).
    SeriesAdded {
        add_gen: u64,
        result: Result<Box<Series>, Error>,
    },
    /// Queue polls run on their own generation: a background 10s poll must
    /// not invalidate (or be invalidated by) a list fetch in flight, yet
    /// both draw from the same shared allocator.
    QueueLoaded {
        queue_gen: u64,
        result: Result<Vec<QueueItem>, Error>,
    },
    ReleasesLoaded {
        fetch_gen: u64,
        result: Result<Vec<Release>, Error>,
    },
    HistoryLoaded {
        fetch_gen: u64,
        result: Result<Vec<HistoryRecord>, Error>,
    },
    /// A mutation (search command, grab, file delete) finished. `fetch_gen`
    /// is the view generation the action was issued from; a result landing
    /// after the user refetched or navigated away is dropped.
    CommandDone {
        fetch_gen: u64,
        kind: CommandKind,
        result: Result<(), Error>,
    },
    /// The tracked auto-search command for a just-added series was created
    /// (carries its id, so it can be polled) or failed to start. `auto_search_gen`
    /// is the monitor's own generation: it runs independently of the view, so
    /// the monitor survives navigating away, and a result whose monitor has
    /// been cleared (or belongs to a superseded Browse) finds no match and is
    /// dropped.
    AutoSearchStarted {
        auto_search_gen: u64,
        result: Result<Command, Error>,
    },
    /// One poll of a tracked auto-search command's status. Matched to its
    /// monitor by `auto_search_gen`, same as `AutoSearchStarted`.
    AutoSearchPolled {
        auto_search_gen: u64,
        result: Result<Command, Error>,
    },
    KeyringError(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    AutoSearch,
    Grab,
    DeleteFile,
    /// Carries the deleted series' id, so the result can leave its pages only
    /// when the user is still viewing that series (navigation doesn't bump the
    /// generation counter).
    DeleteSeries(i64),
    DeleteQueue,
    /// A monitored toggle (series, season, or episode): the refetched indicator
    /// is the only feedback, so this carries no payload and sets no notice. The
    /// refetch is driven by the current level.
    SetMonitored,
}
