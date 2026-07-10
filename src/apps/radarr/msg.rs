//! Results of spawned async work, sent back into the event loop. Every
//! variant that answers a supersedable request carries the generation it was
//! issued under; handlers drop stale generations FIRST, before looking at
//! the result (a stale 401 belongs to a key that may since have been
//! replaced and must not kick the fresh session).

use crate::radarr::{
    Client, Error, HistoryRecord, Movie, QualityProfile, QueueItem, Release, RootFolder,
};

pub enum Msg {
    ConnectDone {
        auth_gen: u64,
        result: Result<Client, Error>,
    },
    MoviesLoaded {
        fetch_gen: u64,
        result: Result<Vec<Movie>, Error>,
    },
    /// Results of an add-flow lookup (movie/lookup); shares `fetch_gen` with
    /// the list fetch since only one is ever in flight at a time.
    LookupLoaded {
        fetch_gen: u64,
        result: Result<Vec<Movie>, Error>,
    },
    /// Root folders + quality profiles for the add wizard, fetched on its
    /// own generation so a lookup can't invalidate it.
    AddOptionsLoaded {
        prereq_gen: u64,
        result: Result<(Vec<RootFolder>, Vec<QualityProfile>), Error>,
    },
    /// A movie was added; on success the created movie (with its new id) is
    /// what the detail opens on. Boxed to keep the enum's variants a similar
    /// size (a bare `Movie` dwarfs the others).
    MovieAdded {
        fetch_gen: u64,
        result: Result<Box<Movie>, Error>,
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
    KeyringError(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    AutoSearch,
    Grab,
    DeleteFile,
}
