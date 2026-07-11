//! The Radarr browse screen: movie list, movie detail (file info or status,
//! search entry point) and interactive search — a flat `Level` enum rather
//! than a stack, since every pop target is deterministic.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::apps::auto_search::{self, Monitor, TickAction};
use crate::apps::delete_prompt::{self, DeletePrompt};
use crate::apps::downloads;
use crate::event::AppSender;
use crate::radarr::display::{self, MOVIE_SORTS, MovieSort, MovieStatus};
use crate::radarr::{
    Client, Command, HistoryRecord, Movie, QualityProfile, QueueItem, Release, RootFolder,
};
use crate::ui::form::{Field, Form, FormEvent};
use crate::ui::input::TextInput;
use crate::ui::text::{truncate, wrap_text};
use crate::ui::{help, list, prompt, theme};

use super::msg::{CommandKind, Msg};

/// Stable field ids for the add/edit forms; results are read by id.
mod field {
    use crate::ui::form::FieldId;
    pub const ROOT: FieldId = 0;
    pub const QUALITY: FieldId = 1;
    pub const MIN_AVAIL: FieldId = 2;
    pub const MOVE_FILES: FieldId = 3;
    pub const MONITORED: FieldId = 4;
    pub const SEARCH: FieldId = 5;
}

const SPINNER_FRAMES: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

/// Queue polls fire every this many 250ms ticks (~10s). Unlike the Sonarr
/// tab, the movie list itself shows download state, so polling runs on
/// every level.
const QUEUE_POLL_TICKS: u32 = 40;
/// How many times the poll interval may double on repeated failures (~10s →
/// ~80s) before it stops growing.
const QUEUE_BACKOFF_MAX_SHIFT: u32 = 3;
/// The fully-backed-off poll interval. Derived from the base and the shift cap
/// so the two can't drift: it is exactly the interval at the last backoff step
/// (`QUEUE_POLL_TICKS << QUEUE_BACKOFF_MAX_SHIFT`), which `queue_poll_interval`
/// clamps to. Change the shift and the cap follows automatically.
const QUEUE_POLL_MAX_TICKS: u32 = QUEUE_POLL_TICKS << QUEUE_BACKOFF_MAX_SHIFT;

/// Right-hand column budget of a movie row: the status text and a gap
/// (fits "unreleased 2026-09-12").
const MOVIE_META_WIDTH: u16 = 24;

/// Where the user is inside the Radarr app. Pops are deterministic:
/// Search → MovieDetail → MovieList.
#[allow(clippy::large_enum_variant)] // one instance per Browse, size is irrelevant
enum Level {
    MovieList,
    /// Always entered on open, file or not; search starts from here.
    MovieDetail {
        movie: Movie,
    },
    /// Interactive search results for one movie (missing or upgrade).
    Search {
        movie: Movie,
    },
    /// Add a new movie: a persistent search box over movie/lookup and its
    /// results, from which the add wizard starts.
    Add,
    /// The download queue as a list the user can mark and remove from.
    Downloads,
}

/// A pending yes/no decision; modal while set.
enum Confirm {
    DeleteFile { file_id: i64 },
    Grab { index: usize },
}

/// Whether the open form is adding a new movie or editing a library one.
enum FormMode {
    /// Editing the movie with this id (options change via the bulk editor).
    Edit { id: i64 },
    /// Adding a new movie from this raw lookup object, cloned when the form
    /// opened. Snapshotting the body (rather than an index into
    /// `add_results_raw`) means a lookup that lands under the open modal form
    /// cannot swap the list out from under Save, which would POST a different
    /// movie than the form title showed. Mirrors how the delete prompts
    /// capture their id at open time.
    Add { body: serde_json::Value },
}

/// The open full-body form: the reusable widget, what it commits to, and the
/// header line. Modal while `Some`.
struct ActiveForm {
    form: Form,
    mode: FormMode,
    title: String,
}

/// An `o` press captured while the picker lists were still loading: the movie
/// and its current values, held until `AddOptionsLoaded` lands so the form can
/// open by itself instead of asking the user to press `o` again.
struct PendingEdit {
    id: i64,
    root: Option<String>,
    quality: i64,
    avail: Option<String>,
}

/// What the browse screen wants its parent (the Radarr app) to do.
pub enum BrowseAction {
    Quit,
}

/// The periodic queue-poll interval (in ticks) for a given backoff exponent:
/// the base cadence doubled per failed poll, capped so a downed server is
/// retried with growing intervals instead of a fixed loop. `checked_shl` keeps
/// an out-of-range exponent from overflowing the shift.
fn queue_poll_interval(backoff: u32) -> u32 {
    QUEUE_POLL_TICKS
        .checked_shl(backoff)
        .unwrap_or(QUEUE_POLL_MAX_TICKS)
        .min(QUEUE_POLL_MAX_TICKS)
}

/// A movie's "Title (Year)" label for the auto-search status line.
fn movie_label(movie: &Movie) -> String {
    let title = movie.title.as_deref().unwrap_or("movie");
    match movie.year {
        Some(year) if year > 0 => format!("{title} ({year})"),
        _ => title.to_string(),
    }
}

pub struct Browse {
    pub client: Client,
    sender: AppSender,

    level: Level,

    all_movies: Vec<Movie>,
    /// Indices into `all_movies` for the currently visible rows (filter
    /// applied). Holding indices rather than cloned `Movie` keeps filtering a
    /// large library cheap on every keystroke.
    filtered: Vec<usize>,
    movie_cursor: usize,
    releases: Vec<Release>,
    release_cursor: usize,
    /// Download queue, refreshed by the poll; drives every ↓ marker and the
    /// Downloads view.
    queue: Vec<QueueItem>,
    /// Selection + prompt state for the Downloads level.
    downloads: downloads::Downloads,
    /// History of the movie currently being searched interactively.
    history: Vec<HistoryRecord>,

    filter: TextInput,
    filter_active: bool,
    filter_focused: bool,
    sort: MovieSort,

    /// Sort popup cursor into `MOVIE_SORTS`; `None` when closed.
    sort_menu: Option<usize>,
    /// "Auto search / Interactive search / Cancel" popup cursor.
    search_menu: Option<usize>,
    confirm: Option<Confirm>,
    /// The delete-movie prompt (files / import-list-exclusion toggles); modal
    /// while set, opened with `z` on the detail.
    delete_prompt: Option<DeletePrompt>,
    /// Cursor into the selected release's rejection list; `None` when closed.
    rejections_popup: Option<usize>,

    // The add-new flow (Level::Add): a persistent search box, its lookup
    // results, the wizard prerequisites, and the wizard itself when open.
    add_search: TextInput,
    add_search_focused: bool,
    add_results: Vec<Movie>,
    /// The raw lookup JSON behind `add_results`, index-aligned; the add
    /// forwards the whole object (POST /movie needs titleSlug/images the
    /// typed model drops).
    add_results_raw: Vec<serde_json::Value>,
    add_cursor: usize,
    root_folders: Vec<RootFolder>,
    quality_profiles: Vec<QualityProfile>,
    /// Generations for the three add-flow async paths, each independent so
    /// one can't invalidate another: the lookup (bumped when the user leaves
    /// the screen, so an abandoned lookup can't surface on the list), the
    /// prerequisites, and the add POST (bumped on leave, so a committed add
    /// never yanks the user back).
    add_lookup_gen: u64,
    add_prereq_gen: u64,
    add_gen: u64,
    /// An add POST is in flight; blocks a second submit until it lands.
    add_submitting: bool,
    /// The open add/edit form (opened with `a` / `o`); modal while set and
    /// drawn full-body in place of the level content.
    form: Option<ActiveForm>,
    /// An `o` press waiting on the picker lists to load; the form opens
    /// automatically once `AddOptionsLoaded` arrives. Dropped if the user
    /// leaves the level meanwhile (see `pop_level`).
    pending_edit: Option<PendingEdit>,
    /// The just-committed add asked for an auto-search; read once its
    /// `MovieAdded` lands, to start a background monitor. Only one add is ever
    /// in flight (`add_submitting` guards a second submit), so one flag suffices.
    pending_search: bool,
    /// Background auto-search monitors, one per add-with-search still resolving.
    /// Each runs on its own generation, independent of the view, so it survives
    /// navigation; see [`crate::apps::auto_search`].
    monitors: Vec<Monitor>,

    /// The detail-level overview is expanded past its collapsed cap. When set,
    /// the detail body (full overview plus file details) scrolls with
    /// `overview_scroll`.
    overview_expanded: bool,
    overview_scroll: usize,
    /// Set every frame while rendering the detail: whether the overview is
    /// longer than the collapsed cap, i.e. whether there is anything to expand.
    /// Read by the key handler and help so `i` is only offered when it does
    /// something (mirrors the render-derived `last_height`).
    overview_truncatable: bool,

    show_full_help: bool,
    loading: bool,
    /// A queue poll in flight; polls never overlap and never show a spinner.
    queue_loading: bool,
    pub error: Option<String>,
    /// One-line advisory (e.g. "search started"), shown in the error row
    /// when there is no error; cleared on the next fetch.
    pub notice: Option<String>,
    /// Allocator for fetch/queue generations. Owned by the app and shared
    /// across Browse instances, so a fetch still in flight from a
    /// pre-reconnect instance can never collide with this instance.
    gen_counter: Arc<AtomicU64>,
    /// Generation of this instance's latest list fetch; older results are stale.
    fetch_gen: u64,
    /// Generation of the latest queue poll, independent of list fetches.
    queue_gen: u64,
    /// Backoff exponent for the periodic queue poll: 0 when healthy, bumped
    /// (capped) on each failed poll so a downed server is retried with growing
    /// intervals, reset on the next success.
    queue_backoff: u32,
    /// `tick_count` at the last periodic poll, so the (possibly backed-off)
    /// interval is measured from when it actually last ran.
    last_poll_tick: u32,
    /// Whether this is the visible tab; the periodic poll pauses while hidden.
    active: bool,
    has_fetched: bool,
    tick_count: u32,
    spinner_frame: usize,
    /// Terminal height from the last draw, for page jumps and scroll clamps.
    last_height: u16,
}

impl Browse {
    pub fn new(client: Client, sender: AppSender, gen_counter: Arc<AtomicU64>) -> Self {
        let mut browse = Self {
            client,
            sender,
            level: Level::MovieList,
            all_movies: Vec::new(),
            filtered: Vec::new(),
            movie_cursor: 0,
            releases: Vec::new(),
            release_cursor: 0,
            queue: Vec::new(),
            downloads: downloads::Downloads::default(),
            history: Vec::new(),
            filter: TextInput::default(),
            filter_active: false,
            filter_focused: false,
            sort: MOVIE_SORTS[0],
            sort_menu: None,
            search_menu: None,
            confirm: None,
            delete_prompt: None,
            rejections_popup: None,
            add_search: TextInput::default(),
            add_search_focused: false,
            add_results: Vec::new(),
            add_results_raw: Vec::new(),
            add_cursor: 0,
            root_folders: Vec::new(),
            quality_profiles: Vec::new(),
            add_lookup_gen: 0,
            add_prereq_gen: 0,
            add_gen: 0,
            add_submitting: false,
            form: None,
            pending_edit: None,
            pending_search: false,
            monitors: Vec::new(),
            overview_expanded: false,
            overview_scroll: 0,
            overview_truncatable: false,
            show_full_help: false,
            loading: false,
            queue_loading: false,
            error: None,
            notice: None,
            gen_counter,
            fetch_gen: 0,
            queue_gen: 0,
            queue_backoff: 0,
            last_poll_tick: 0,
            active: true,
            has_fetched: false,
            tick_count: 0,
            spinner_frame: 0,
            last_height: 24,
        };
        browse.fetch_movies();
        // The list shows ↓ markers, so it needs the queue from first paint.
        browse.fetch_queue();
        browse
    }

    pub fn on_tick(&mut self) {
        if self.loading {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
        }
        self.tick_count = self.tick_count.wrapping_add(1);
        // Every level shows download state, but only poll while this tab is
        // visible: a hidden tab would otherwise poll the server forever.
        // Auto-search monitors fetch the queue themselves when they need it,
        // so pausing here can't stall them.
        if self.active && self.queue_poll_due() {
            self.last_poll_tick = self.tick_count;
            self.fetch_queue();
        }
        self.tick_monitors();
    }

    /// Whether the (possibly backed-off) periodic poll interval has elapsed.
    fn queue_poll_due(&self) -> bool {
        self.tick_count.wrapping_sub(self.last_poll_tick) >= queue_poll_interval(self.queue_backoff)
    }

    /// Called by the app when this tab shows or hides. While hidden the
    /// periodic queue poll pauses; on return it resets the backoff and
    /// refreshes the markers at once so they aren't left stale.
    pub fn set_active(&mut self, active: bool) {
        let returning = active && !self.active;
        self.active = active;
        if returning {
            self.queue_backoff = 0;
            self.last_poll_tick = self.tick_count;
            self.fetch_queue();
        }
    }

    /// True while the add-search box has focus, so the shell knows a keystroke
    /// is text input and not a global shortcut.
    pub fn input_focused(&self) -> bool {
        self.add_search_focused
    }

    /// Mark a list fetch as started and allocate its generation; any result
    /// carrying an older generation is stale and dropped on arrival.
    fn begin_fetch(&mut self) -> u64 {
        self.loading = true;
        self.error = None;
        // The connect-time notice has been seen once the user navigates.
        if self.has_fetched {
            self.notice = None;
        }
        self.has_fetched = true;
        self.fetch_gen = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
        self.fetch_gen
    }

    fn fetch_movies(&mut self) {
        let fetch_gen = self.begin_fetch();
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.get_movies().await;
            sender.send(Msg::MoviesLoaded { fetch_gen, result });
        });
    }

    /// One queue poll; skipped while another is in flight so a slow server
    /// never stacks requests.
    fn fetch_queue(&mut self) {
        if self.queue_loading {
            return;
        }
        self.queue_loading = true;
        self.queue_gen = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let queue_gen = self.queue_gen;
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.get_queue().await;
            sender.send(Msg::QueueLoaded { queue_gen, result });
        });
    }

    /// Interactive search: releases and the movie's history fetched
    /// concurrently under one generation. The history only feeds the
    /// "grabbed before" marker, so it may trickle in after the releases.
    fn fetch_releases(&mut self, movie_id: i64) {
        let fetch_gen = self.begin_fetch();
        self.releases.clear();
        self.history.clear();
        self.release_cursor = 0;
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.search_releases(movie_id).await;
            sender.send(Msg::ReleasesLoaded { fetch_gen, result });
        });
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.get_history(movie_id).await;
            sender.send(Msg::HistoryLoaded { fetch_gen, result });
        });
    }

    /// Run a mutation under the CURRENT view generation: if the user
    /// refetches or navigates somewhere that refetches before it lands, the
    /// result is stale and dropped.
    fn spawn_command(
        &mut self,
        kind: CommandKind,
        task: impl Future<Output = Result<(), crate::radarr::Error>> + Send + 'static,
    ) {
        self.loading = true;
        let fetch_gen = self.fetch_gen;
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = task.await;
            sender.send(Msg::CommandDone {
                fetch_gen,
                kind,
                result,
            });
        });
    }

    /// Returns true when the server rejected the API key (the caller must
    /// drop to the setup screen). Staleness is checked FIRST; see msg.rs.
    #[must_use]
    pub fn on_movies_loaded(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<Movie>, crate::radarr::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // stale response from a superseded fetch
        }
        // A list refresh landing while the user is on the add screen must not
        // touch its loading/error/list state (the lookup owns those); still
        // honour a 401 so a revoked key is caught immediately.
        if matches!(self.level, Level::Add) {
            return matches!(result, Err(crate::radarr::Error::Unauthorized));
        }
        self.loading = false;
        match result {
            Ok(mut list) => {
                display::sort_movies(&mut list, self.sort);
                // A reload with a detail open (refresh, post-delete, post-
                // grab) must also refresh the embedded copy the detail
                // renders from.
                let embedded = match &mut self.level {
                    Level::MovieList | Level::Add | Level::Downloads => None,
                    Level::MovieDetail { movie } | Level::Search { movie } => Some(movie),
                };
                if let Some(movie) = embedded
                    && let Some(updated) = list.iter().find(|m| m.id == movie.id)
                {
                    *movie = updated.clone();
                }
                self.all_movies = list;
                self.apply_filter();
            }
            Err(crate::radarr::Error::Unauthorized) => return true,
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Same contract as `on_movies_loaded`. A failed poll is cosmetic (the
    /// markers just go stale until the next one) and must not stomp the
    /// screen with an error every 10 seconds — except a 401, which means the
    /// key was revoked and is reported like everywhere else.
    #[must_use]
    pub fn on_queue_loaded(
        &mut self,
        queue_gen: u64,
        result: Result<Vec<QueueItem>, crate::radarr::Error>,
    ) -> bool {
        if queue_gen != self.queue_gen {
            return false; // stale poll from a superseded queue fetch
        }
        self.queue_loading = false;
        match result {
            Ok(queue) => {
                self.queue = queue;
                self.downloads.prune(&self.queue);
                self.queue_backoff = 0;
            }
            Err(crate::radarr::Error::Unauthorized) => return true,
            Err(err) => {
                // Grow the poll interval so a downed server is retried with
                // exponential backoff, not a fixed ~10s loop.
                self.queue_backoff = (self.queue_backoff + 1).min(QUEUE_BACKOFF_MAX_SHIFT);
                tracing::debug!(%err, "queue poll failed");
            }
        }
        false
    }

    /// Same contract as `on_movies_loaded`.
    #[must_use]
    pub fn on_releases_loaded(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<Release>, crate::radarr::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // stale response from a superseded search
        }
        self.loading = false;
        match result {
            Ok(releases) => {
                self.releases = releases;
                self.release_cursor = 0;
            }
            Err(crate::radarr::Error::Unauthorized) => return true,
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Same contract as `on_movies_loaded`. A missing history only costs
    /// the "grabbed before" marker, so failures are logged, not shown.
    #[must_use]
    pub fn on_history_loaded(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<HistoryRecord>, crate::radarr::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // stale response from a superseded search
        }
        match result {
            Ok(history) => self.history = history,
            Err(crate::radarr::Error::Unauthorized) => return true,
            Err(err) => tracing::debug!(%err, "movie history fetch failed"),
        }
        false
    }

    /// Unlike the list loaders, a command *failure* is surfaced even when
    /// superseded: a delete/grab/toggle that didn't take is a discrete outcome
    /// the user must learn about, not a stale snapshot to discard silently. A
    /// stale *success* is still dropped (the newer fetch already reflects it).
    #[must_use]
    pub fn on_command_done(
        &mut self,
        fetch_gen: u64,
        kind: CommandKind,
        result: Result<(), crate::radarr::Error>,
    ) -> bool {
        if matches!(result, Err(crate::radarr::Error::Unauthorized)) {
            return true; // a revoked key must re-auth regardless of staleness
        }
        if let Err(err) = &result {
            self.error = Some(err.to_string());
            // A newer fetch owns `loading` on the stale path; only clear it
            // when this command is still the current generation.
            if fetch_gen == self.fetch_gen {
                self.loading = false;
            }
            return false;
        }
        if fetch_gen != self.fetch_gen {
            return false; // superseded success: the newer fetch already reflects it
        }
        self.loading = false;
        match kind {
            CommandKind::AutoSearch => {
                self.notice = Some("search started; grabbed downloads appear in the queue".into());
                // Surface the ↓ marker as soon as Radarr grabs something.
                self.fetch_queue();
            }
            CommandKind::Grab => {
                self.pop_to_detail();
                // Set AFTER the refetch, which clears the notice row.
                self.notice = Some("release sent to the download client".into());
            }
            CommandKind::DeleteFile => {
                // Delete fires from the detail itself; stay there and let
                // the refetch swap the embedded movie to its file-less form.
                self.fetch_movies();
                self.fetch_queue();
                self.notice = Some("movie file deleted".into());
            }
            CommandKind::DeleteMovie(deleted_id) => {
                // The movie is gone. Only leave its detail if the user is
                // still looking at it: a delete landing after they navigated
                // to another movie (the generation counter doesn't bump on
                // navigation) must not yank them away. The refetch drops it
                // from the list regardless.
                if matches!(&self.level,
                    Level::MovieDetail { movie } | Level::Search { movie }
                        if movie.id == deleted_id)
                {
                    self.level = Level::MovieList;
                }
                self.fetch_movies();
                self.fetch_queue();
                self.notice = Some("movie deleted".into());
            }
            CommandKind::DeleteQueue => {
                self.downloads.clear_marks();
                self.fetch_queue();
                self.notice = Some("removed from queue".into());
            }
            CommandKind::SetMonitored => {
                // Refetch so the list and the embedded detail copy repaint from
                // the server; the flipped indicator is feedback enough.
                self.fetch_movies();
            }
            CommandKind::SetOptions => {
                // Refetch so the list and detail repaint the new options, then
                // (after the fetch clears the notice row) confirm.
                self.fetch_movies();
                self.notice = Some("options updated".into());
            }
        }
        false
    }

    /// Add-flow lookup results. Checked against `add_lookup_gen`, so a lookup
    /// the user abandoned by leaving the add screen is dropped instead of
    /// surfacing an error on the list. Keeps the raw JSON index-aligned with
    /// the typed list the rows render from.
    #[must_use]
    pub fn on_lookup_loaded(
        &mut self,
        add_lookup_gen: u64,
        result: Result<Vec<serde_json::Value>, crate::radarr::Error>,
    ) -> bool {
        if add_lookup_gen != self.add_lookup_gen {
            return false; // stale response from a superseded/abandoned lookup
        }
        self.loading = false;
        match result {
            Ok(values) => {
                self.add_results.clear();
                self.add_results_raw.clear();
                // Movie deserializes any object (all fields default), so typed
                // and raw stay aligned.
                for value in values {
                    if let Ok(movie) = serde_json::from_value::<Movie>(value.clone()) {
                        self.add_results.push(movie);
                        self.add_results_raw.push(value);
                    }
                }
                self.add_cursor = 0;
            }
            Err(crate::radarr::Error::Unauthorized) => return true,
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Add-wizard prerequisites. Checked against `add_prereq_gen`, not the
    /// list generation. A non-401 failure just leaves the pickers empty (the
    /// wizard then refuses to open), so it is logged, not shown.
    #[must_use]
    pub fn on_add_options_loaded(
        &mut self,
        prereq_gen: u64,
        result: Result<(Vec<RootFolder>, Vec<QualityProfile>), crate::radarr::Error>,
    ) -> bool {
        if prereq_gen != self.add_prereq_gen {
            return false;
        }
        match result {
            Ok((folders, profiles)) => {
                self.root_folders = folders;
                self.quality_profiles = profiles;
                // A deferred `o` press was waiting on these: open its wizard
                // now, unless the user has since left an editable level.
                if let Some(pending) = self.pending_edit.take() {
                    self.loading = false;
                    self.notice = None;
                    if matches!(self.level, Level::MovieList | Level::MovieDetail { .. }) {
                        self.open_edit_form(
                            pending.id,
                            pending.root.as_deref(),
                            pending.quality,
                            pending.avail.as_deref(),
                        );
                    }
                }
            }
            Err(crate::radarr::Error::Unauthorized) => return true,
            Err(err) => {
                if self.pending_edit.take().is_some() {
                    self.loading = false;
                    self.notice = None;
                    self.error = Some(err.to_string());
                } else {
                    tracing::debug!(%err, "add options fetch failed");
                }
            }
        }
        false
    }

    /// The add finished. Checked against `add_gen`, which is bumped when the
    /// user leaves the add screen, so a committed add that lands after they
    /// moved on is dropped rather than yanking them to the new movie. On
    /// success we jump to the created movie and refetch the library so it is
    /// present when the user pops back.
    #[must_use]
    pub fn on_movie_added(
        &mut self,
        add_gen: u64,
        result: Result<Box<Movie>, crate::radarr::Error>,
    ) -> bool {
        let is_current = add_gen == self.add_gen;
        // A *failed* add the user has already left has nothing to surface, so
        // drop it. A *successful* one still has work to do (below), so it falls
        // through even when the user moved on.
        if result.is_err() && !is_current {
            return false;
        }
        let pending_search = std::mem::take(&mut self.pending_search);
        match result {
            Ok(movie) => {
                // The add already succeeded server-side, so the tracked
                // auto-search the user opted into and the library refetch run
                // whether or not they are still on the add screen: the monitor
                // is view-independent (see auto_search), and the refetch makes
                // the new movie present when they next look. Only the
                // navigation and the add-screen chrome are gated on staying.
                if pending_search {
                    self.start_auto_search(movie.id, movie_label(&movie));
                }
                if is_current {
                    self.add_submitting = false;
                    self.overview_expanded = false;
                    self.overview_scroll = 0;
                    self.level = Level::MovieDetail { movie: *movie };
                    self.add_results.clear();
                    self.add_results_raw.clear();
                    self.add_search.clear();
                    self.add_search_focused = false;
                }
                self.fetch_movies();
                self.fetch_queue();
                if is_current {
                    // Set AFTER the refetch, which clears the notice row.
                    self.notice = Some("movie added".into());
                }
            }
            // is_current is guaranteed here (stale errors returned above).
            Err(crate::radarr::Error::Unauthorized) => {
                self.add_submitting = false;
                return true;
            }
            // The add failed (e.g. already in the library); stay on the
            // results so the user can pick something else.
            Err(err) => {
                self.add_submitting = false;
                self.loading = false;
                self.error = Some(err.to_string());
            }
        }
        false
    }

    /// Start tracking a background auto-search for a just-added movie: fire the
    /// tracked `MoviesSearch` command and record a monitor on its own
    /// generation. The baseline snapshot lets the outcome check tell a fresh
    /// grab from a download that was already queued.
    fn start_auto_search(&mut self, movie_id: i64, title: String) {
        let generation = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let baseline = self
            .queue
            .iter()
            .filter(|item| item.movie_id == Some(movie_id))
            .map(|item| item.id)
            .collect();
        self.monitors
            .push(Monitor::new(generation, movie_id, title, baseline));
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.movie_search(movie_id).await;
            sender.send(Msg::AutoSearchStarted {
                auto_search_gen: generation,
                result,
            });
        });
    }

    /// Queue items for `movie_id` that were not present when its search started.
    fn queue_new_items(&self, monitor: &Monitor) -> usize {
        self.queue
            .iter()
            .filter(|item| {
                item.movie_id == Some(monitor.target_id) && !monitor.is_baseline(item.id)
            })
            .count()
    }

    /// Advance every monitor one tick and run the side effects they request
    /// (poll a command, refresh the queue, drop an expired monitor). Snapshot
    /// the per-monitor new-item counts first so the queue and the monitor list
    /// aren't borrowed at once.
    fn tick_monitors(&mut self) {
        if self.monitors.is_empty() {
            return;
        }
        let new_counts: Vec<usize> = self
            .monitors
            .iter()
            .map(|monitor| self.queue_new_items(monitor))
            .collect();
        let mut polls: Vec<(u64, i64)> = Vec::new();
        let mut want_queue = false;
        let mut expired: Vec<u64> = Vec::new();
        for (monitor, &new_items) in self.monitors.iter_mut().zip(new_counts.iter()) {
            match monitor.tick(new_items) {
                TickAction::Idle => {}
                TickAction::Poll(command_id) => polls.push((monitor.generation, command_id)),
                TickAction::FetchQueue => want_queue = true,
                TickAction::Drop => expired.push(monitor.generation),
            }
        }
        if !expired.is_empty() {
            self.monitors
                .retain(|monitor| !expired.contains(&monitor.generation));
        }
        if want_queue {
            self.fetch_queue();
        }
        for (generation, command_id) in polls {
            self.spawn_command_poll(generation, command_id);
        }
    }

    /// Poll one tracked command's status; the result rides `AutoSearchPolled`,
    /// matched back to its monitor by generation.
    fn spawn_command_poll(&self, generation: u64, command_id: i64) {
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.get_command(command_id).await;
            sender.send(Msg::AutoSearchPolled {
                auto_search_gen: generation,
                result,
            });
        });
    }

    /// A tracked search command was created (or failed to start). Matched to
    /// its monitor by generation; a result with no live monitor is dropped
    /// (the monitor was cleared, or belongs to a superseded Browse).
    #[must_use]
    pub fn on_auto_search_started(
        &mut self,
        auto_search_gen: u64,
        result: Result<Command, crate::radarr::Error>,
    ) -> bool {
        let Some(index) = self
            .monitors
            .iter()
            .position(|monitor| monitor.generation == auto_search_gen)
        else {
            return false;
        };
        match result {
            // A real *arr always returns a positive command id; guard against a
            // malformed response so a defaulted 0 can't have us poll command 0.
            Ok(command) if command.id > 0 => self.monitors[index].started(command.id),
            Ok(_) => {
                self.monitors[index].failed_to_start();
                tracing::debug!("auto-search command created without an id");
            }
            Err(crate::radarr::Error::Unauthorized) => {
                self.monitors.remove(index);
                return true;
            }
            Err(err) => {
                self.monitors[index].failed_to_start();
                tracing::debug!(%err, "auto-search command failed to start");
            }
        }
        false
    }

    /// A status poll for a tracked command landed. Same generation contract as
    /// `on_auto_search_started`; on completion the queue is refreshed to
    /// confirm whether a release was grabbed.
    #[must_use]
    pub fn on_auto_search_polled(
        &mut self,
        auto_search_gen: u64,
        result: Result<Command, crate::radarr::Error>,
    ) -> bool {
        let Some(index) = self
            .monitors
            .iter()
            .position(|monitor| monitor.generation == auto_search_gen)
        else {
            return false;
        };
        match result {
            Ok(command) => {
                if self.monitors[index].polled(command.status.as_deref()) {
                    self.fetch_queue();
                }
            }
            Err(crate::radarr::Error::Unauthorized) => {
                self.monitors.remove(index);
                return true;
            }
            Err(err) => {
                self.monitors[index].clear_poll_inflight();
                tracing::debug!(%err, "auto-search poll failed");
            }
        }
        false
    }

    /// The cross-tab status line summarising this app's auto-search monitors.
    pub fn status_line(&self) -> Option<Line<'static>> {
        auto_search::status_line("Radarr", &self.monitors)
    }

    /// Return from the search level to the movie detail and refetch: the
    /// caller just changed server state.
    fn pop_to_detail(&mut self) {
        let movie = match std::mem::replace(&mut self.level, Level::MovieList) {
            Level::Search { movie } => movie,
            other => {
                self.level = other;
                return;
            }
        };
        self.fetch_movies();
        self.fetch_queue();
        self.overview_expanded = false;
        self.overview_scroll = 0;
        self.level = Level::MovieDetail { movie };
    }

    fn apply_filter(&mut self) {
        if !self.filter_active || self.filter.value().is_empty() {
            self.filtered = (0..self.all_movies.len()).collect();
        } else {
            let needle = self.filter.value().to_lowercase();
            self.filtered = self
                .all_movies
                .iter()
                .enumerate()
                .filter(|(_, movie)| {
                    movie
                        .title
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&needle)
                })
                .map(|(index, _)| index)
                .collect();
        }
        if self.movie_cursor >= self.filtered.len() {
            self.movie_cursor = 0;
        }
    }

    fn selected_movie(&self) -> Option<&Movie> {
        self.filtered
            .get(self.movie_cursor)
            .and_then(|&index| self.all_movies.get(index))
    }

    fn selected_release(&self) -> Option<&Release> {
        self.releases.get(self.release_cursor)
    }

    /// The cursor of the current level's list, with the list length; `None`
    /// on the (list-less) movie detail.
    fn cursor_and_len(&mut self) -> Option<(&mut usize, usize)> {
        match &self.level {
            Level::MovieList => Some((&mut self.movie_cursor, self.filtered.len())),
            Level::MovieDetail { .. } => None,
            Level::Search { .. } => Some((&mut self.release_cursor, self.releases.len())),
            Level::Add => Some((&mut self.add_cursor, self.add_results.len())),
            Level::Downloads => Some((&mut self.downloads.cursor, self.queue.len())),
        }
    }

    fn open_selected(&mut self) {
        match &self.level {
            Level::MovieList => {
                if let Some(movie) = self.selected_movie().cloned() {
                    self.overview_expanded = false;
                    self.overview_scroll = 0;
                    self.level = Level::MovieDetail { movie };
                }
            }
            // Search is always offered here — Radarr grabs upgrades for
            // downloaded movies too.
            Level::MovieDetail { .. } => self.search_menu = Some(0),
            Level::Search { .. } => {
                if self.selected_release().is_some() {
                    self.confirm = Some(Confirm::Grab {
                        index: self.release_cursor,
                    });
                }
            }
            Level::Add => self.start_add_flow(),
            // Nothing to open; Space is intercepted earlier to toggle a mark.
            Level::Downloads => {}
        }
    }

    /// Pop one level; returns false at the movie list so the caller can
    /// give Esc its list-level meaning (clear filter) instead.
    fn pop_level(&mut self) -> bool {
        // Esc/back while the pickers are still loading cancels a deferred `o`.
        self.pending_edit = None;
        match std::mem::replace(&mut self.level, Level::MovieList) {
            Level::MovieList => false,
            Level::Downloads => {
                self.downloads.reset();
                true
            }
            Level::MovieDetail { .. } => {
                self.overview_expanded = false;
                self.overview_scroll = 0;
                true
            }
            Level::Search { movie } => {
                // The release search runs under a 120s timeout (see
                // arr::RELEASE_SEARCH_TIMEOUT); bump the fetch generation so a
                // result — or error — landing after we leave can't stamp
                // `releases`/`error` onto the detail we return to. Only this
                // level carries such a long-lived fetch: bumping on the other
                // pops would instead drop a benign, often wanted, list refetch
                // (e.g. the one a just-completed add fires).
                self.fetch_gen = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
                self.loading = false;
                self.error = None;
                self.releases.clear();
                self.history.clear();
                self.overview_expanded = false;
                self.overview_scroll = 0;
                self.level = Level::MovieDetail { movie };
                true
            }
            Level::Add => {
                self.add_results.clear();
                self.add_results_raw.clear();
                self.add_search.clear();
                self.add_search_focused = false;
                self.form = None;
                self.add_submitting = false;
                // Supersede any in-flight lookup and add so a late result
                // can't surface an error on, or navigate away from, the list.
                self.add_lookup_gen = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
                self.add_gen = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
                self.loading = false;
                self.error = None;
                true
            }
        }
    }

    fn refresh(&mut self) {
        match &self.level {
            Level::Downloads => self.fetch_queue(),
            Level::MovieList | Level::MovieDetail { .. } => {
                self.fetch_movies();
                self.fetch_queue();
            }
            Level::Search { movie } => {
                let id = movie.id;
                self.fetch_releases(id);
            }
            Level::Add => {
                self.fetch_add_options();
                let term = self.add_search.value().to_string();
                if !term.trim().is_empty() {
                    self.fetch_lookup(term);
                }
            }
        }
    }

    fn execute_confirm(&mut self, confirm: Confirm) {
        match confirm {
            Confirm::DeleteFile { file_id } => {
                let client = self.client.clone();
                self.spawn_command(CommandKind::DeleteFile, async move {
                    client.delete_movie_file(file_id).await
                });
            }
            Confirm::Grab { index } => {
                let Some(release) = self.releases.get(index) else {
                    return;
                };
                let (guid, indexer_id) = (release.guid.clone(), release.indexer_id);
                let client = self.client.clone();
                self.spawn_command(CommandKind::Grab, async move {
                    client.grab_release(&guid, indexer_id).await
                });
            }
        }
    }

    fn on_search_menu_choice(&mut self, choice: usize) {
        let Level::MovieDetail { movie } = &self.level else {
            return;
        };
        match choice {
            0 => {
                let id = movie.id;
                let client = self.client.clone();
                self.spawn_command(CommandKind::AutoSearch, async move {
                    client.movie_search(id).await.map(|_| ())
                });
            }
            1 => {
                let movie = movie.clone();
                self.fetch_releases(movie.id);
                self.level = Level::Search { movie };
            }
            _ => {}
        }
    }

    /// Open the add-new screen: a fresh search box (focused) and its wizard
    /// prerequisites fetched in the background.
    fn enter_add(&mut self) {
        self.level = Level::Add;
        self.add_search.clear();
        self.add_search_focused = true;
        self.add_search.focused = true;
        self.add_results.clear();
        self.add_results_raw.clear();
        self.add_cursor = 0;
        self.form = None;
        self.add_submitting = false;
        self.loading = false;
        self.error = None;
        self.notice = None;
        // Root folders and quality profiles rarely change; only fetch when we
        // don't already have them (`r` forces a refetch).
        if self.root_folders.is_empty() || self.quality_profiles.is_empty() {
            self.fetch_add_options();
        }
    }

    /// Fetch the root folders and quality profiles the wizard picks from, on
    /// their own generation so a lookup can't invalidate them. The two GETs
    /// are independent, so they run concurrently.
    fn fetch_add_options(&mut self) {
        self.add_prereq_gen = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let prereq_gen = self.add_prereq_gen;
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = tokio::try_join!(client.get_root_folders(), client.get_quality_profiles());
            sender.send(Msg::AddOptionsLoaded { prereq_gen, result });
        });
    }

    /// Run a lookup for the add screen; an empty term just clears the list.
    /// Uses `add_lookup_gen`, independent of list fetches, so leaving the
    /// screen can cancel it without touching a list fetch in flight.
    fn fetch_lookup(&mut self, term: String) {
        if term.trim().is_empty() {
            self.add_results.clear();
            self.add_results_raw.clear();
            self.add_cursor = 0;
            return;
        }
        self.loading = true;
        self.error = None;
        self.add_lookup_gen = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let add_lookup_gen = self.add_lookup_gen;
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.lookup_movies(&term).await;
            sender.send(Msg::LookupLoaded {
                add_lookup_gen,
                result,
            });
        });
    }

    /// Open the add form for the highlighted result; refuses if the
    /// prerequisites haven't loaded (or none are configured). Each field opens
    /// on a sensible default (first root folder, a 1080p quality profile,
    /// "Released" availability, monitored + search on).
    fn start_add_flow(&mut self) {
        if self.add_submitting {
            self.notice = Some("an add is already in progress".into());
            return;
        }
        if self.add_cursor >= self.add_results.len() {
            return;
        }
        if self.root_folders.is_empty() || self.quality_profiles.is_empty() {
            self.notice =
                Some("add options unavailable: need a root folder and a quality profile".into());
            return;
        }
        // Snapshot the raw lookup object now, index-aligned with the title
        // below, so Save commits this movie even if a still-in-flight lookup
        // replaces `add_results_raw` while the form is open.
        let Some(body) = self.add_results_raw.get(self.add_cursor).cloned() else {
            return;
        };
        let title = format!(
            "Add movie — {}",
            movie_label(&self.add_results[self.add_cursor])
        );
        let fields = vec![
            Field::text(field::ROOT, "Root folder", self.default_root_folder()),
            Field::select(
                field::QUALITY,
                "Quality profile",
                self.quality_choices(),
                preferred_quality_index(&self.quality_profiles),
            ),
            Field::select(
                field::MIN_AVAIL,
                "Minimum availability",
                availability_choices(),
                released_availability_index(),
            ),
            Field::toggle(field::MONITORED, "Monitor this movie?", true),
            Field::toggle(field::SEARCH, "Search & download now?", true),
        ];
        self.form = Some(ActiveForm {
            form: Form::new(fields, "Add"),
            mode: FormMode::Add { body },
            title,
        });
    }

    /// Forward the add form's lookup object, augmented with the chosen fields,
    /// to POST /movie. The whole object is forwarded (not a subset) because
    /// Radarr requires fields the typed model drops (titleSlug, images).
    fn commit_add(&mut self, mut body: serde_json::Value, form: &Form) {
        // The root folder is typed free-text; the quality profile is still a
        // pick from the configured list.
        let root_folder = form.text(field::ROOT).trim().to_string();
        let quality_profile_id = self
            .quality_profiles
            .get(form.selected(field::QUALITY))
            .map(|p| p.id);
        let (false, Some(quality_profile_id)) = (root_folder.is_empty(), quality_profile_id) else {
            self.notice = Some("add cancelled: no root folder or quality profile".into());
            return;
        };
        let min_availability = MIN_AVAILABILITY
            .get(form.selected(field::MIN_AVAIL))
            .map(|(_, value)| *value)
            .unwrap_or("announced");
        let monitored = form.selected(field::MONITORED) == 0;
        let search = form.selected(field::SEARCH) == 0;
        if let Some(object) = body.as_object_mut() {
            object.insert(
                "qualityProfileId".into(),
                serde_json::json!(quality_profile_id),
            );
            object.insert("rootFolderPath".into(), serde_json::json!(root_folder));
            object.insert("monitored".into(), serde_json::json!(monitored));
            object.insert(
                "minimumAvailability".into(),
                serde_json::json!(min_availability),
            );
            // The search is run and tracked separately (see `start_auto_search`),
            // so the server must not fire its own untracked one — otherwise the
            // movie would be searched twice.
            object.insert(
                "addOptions".into(),
                serde_json::json!({ "searchForMovie": false }),
            );
        }
        self.pending_search = search;
        self.add_submitting = true;
        self.loading = true;
        self.error = None;
        self.notice = Some("adding...".into());
        self.add_gen = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let add_gen = self.add_gen;
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.add_movie(&body).await.map(Box::new);
            sender.send(Msg::MovieAdded { add_gen, result });
        });
    }

    /// The first configured root folder's path, used to pre-fill the add form's
    /// editable Root folder field (empty when none is configured).
    fn default_root_folder(&self) -> String {
        self.root_folders
            .first()
            .map(|folder| folder.path.clone())
            .unwrap_or_default()
    }

    /// Owned quality-profile labels for the picker, built once when a form opens
    /// (so the `Form` never borrows the caches).
    fn quality_choices(&self) -> Vec<String> {
        self.quality_profiles
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    /// Open the options-edit wizard for the movie under the cursor (list) or
    /// the one the detail is showing, seeding each step on the movie's current
    /// value. Copies the id/values out before touching the caches so the
    /// `self.level` borrow ends first. When the pickers haven't loaded yet,
    /// fetch them and stash the request so the wizard opens by itself once
    /// they land (the breadcrumb spinner covers the wait), rather than making
    /// the user press `o` twice.
    fn start_edit_flow(&mut self) {
        let (id, cur_root, cur_quality, cur_avail) = match &self.level {
            Level::MovieList => match self.selected_movie() {
                Some(movie) => (
                    movie.id,
                    movie.root_folder_path.clone(),
                    movie.quality_profile_id,
                    movie.minimum_availability.clone(),
                ),
                None => return,
            },
            Level::MovieDetail { movie } => (
                movie.id,
                movie.root_folder_path.clone(),
                movie.quality_profile_id,
                movie.minimum_availability.clone(),
            ),
            // `o` is a no-op on search/add/downloads.
            _ => return,
        };
        if self.root_folders.is_empty() || self.quality_profiles.is_empty() {
            self.pending_edit = Some(PendingEdit {
                id,
                root: cur_root,
                quality: cur_quality,
                avail: cur_avail,
            });
            self.fetch_add_options();
            self.loading = true;
            self.notice = Some("loading options...".into());
            return;
        }
        self.open_edit_form(id, cur_root.as_deref(), cur_quality, cur_avail.as_deref());
    }

    /// Build and open the edit form, each field opened on the movie's current
    /// value. Assumes the picker lists are loaded. The Move-files toggle starts
    /// hidden and is revealed by the key handler once the root folder changes.
    fn open_edit_form(&mut self, id: i64, root: Option<&str>, quality: i64, avail: Option<&str>) {
        let seed_quality = current_quality_index(&self.quality_profiles, quality)
            .unwrap_or_else(|| preferred_quality_index(&self.quality_profiles));
        let seed_avail = current_availability_index(avail);
        let title = format!(
            "Edit options — {}",
            self.all_movies
                .iter()
                .find(|movie| movie.id == id)
                .map(movie_label)
                .unwrap_or_else(|| "movie".into())
        );
        let fields = vec![
            Field::text(field::ROOT, "Root folder", root.unwrap_or_default()),
            // Right after the root, since it only matters when the root changes.
            Field::toggle(field::MOVE_FILES, "Move existing files?", false).hidden(),
            Field::select(
                field::QUALITY,
                "Quality profile",
                self.quality_choices(),
                seed_quality,
            ),
            Field::select(
                field::MIN_AVAIL,
                "Minimum availability",
                availability_choices(),
                seed_avail,
            ),
        ];
        self.form = Some(ActiveForm {
            form: Form::new(fields, "Save"),
            mode: FormMode::Edit { id },
            title,
        });
    }

    /// Commit the open form: dispatch to the add or edit path.
    fn commit_form(&mut self) {
        let Some(active) = self.form.take() else {
            return;
        };
        match active.mode {
            FormMode::Edit { id } => self.commit_edit(id, &active.form),
            FormMode::Add { body } => self.commit_add(body, &active.form),
        }
    }

    /// Resolve each field to the value to send, sending only genuinely changed
    /// fields. A root cleared to empty counts as no change (the client ties
    /// moveFiles to rootFolderPath being present, so it sends neither).
    fn commit_edit(&mut self, id: i64, form: &Form) {
        let typed_root = form.text(field::ROOT).trim().to_string();
        let root_folder =
            (form.changed(field::ROOT) && !typed_root.is_empty()).then_some(typed_root);
        let quality_profile_id = form
            .changed(field::QUALITY)
            .then(|| {
                self.quality_profiles
                    .get(form.selected(field::QUALITY))
                    .map(|p| p.id)
            })
            .flatten();
        let minimum_availability = form
            .changed(field::MIN_AVAIL)
            .then(|| {
                MIN_AVAILABILITY
                    .get(form.selected(field::MIN_AVAIL))
                    .map(|(_, v)| v.to_string())
            })
            .flatten();
        // Guard on the resolved body, not the raw changed flags, so clearing the
        // root (or any other no-op) never fires an empty editor PUT.
        if root_folder.is_none() && quality_profile_id.is_none() && minimum_availability.is_none() {
            self.notice = Some("no changes".into());
            return;
        }
        let move_files = form.selected(field::MOVE_FILES) == 0;
        let client = self.client.clone();
        self.spawn_command(CommandKind::SetOptions, async move {
            client
                .edit_movie_options(
                    id,
                    root_folder.as_deref(),
                    move_files,
                    quality_profile_id,
                    minimum_availability.as_deref(),
                )
                .await
        });
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<BrowseAction> {
        if self.filter_focused {
            match key.code {
                KeyCode::Esc => {
                    self.filter_focused = false;
                    self.filter.clear();
                    self.filter_active = false;
                    self.apply_filter();
                }
                KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    self.filter_focused = false;
                    if self.filter.value().is_empty() {
                        self.filter_active = false;
                    }
                }
                _ => {
                    if self.filter.on_key(key) {
                        self.apply_filter();
                    }
                }
            }
            self.filter.focused = self.filter_focused;
            return None;
        }

        // The add-screen search box: enter runs the lookup, esc/tab/arrows
        // just unfocus so the results can be navigated.
        if self.add_search_focused {
            match key.code {
                KeyCode::Enter => {
                    self.add_search_focused = false;
                    let term = self.add_search.value().to_string();
                    self.fetch_lookup(term);
                }
                KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    self.add_search_focused = false;
                }
                _ => {
                    self.add_search.on_key(key);
                }
            }
            self.add_search.focused = self.add_search_focused;
            return None;
        }

        // The shell only claims ctrl+c/arrows/digits; don't let other
        // ctrl/alt-modified chars fall through to single-letter shortcuts.
        if crate::app::modified_char(&key) {
            return None;
        }

        // The add/edit form is modal: while open it owns every key. Feed the key
        // to the form under a short borrow that yields the event by value, so
        // the arms below can touch `self` freely.
        if self.form.is_some() {
            let event = self
                .form
                .as_mut()
                .map(|active| active.form.on_key(key))
                .unwrap_or(FormEvent::Consumed);
            match event {
                FormEvent::Consumed => {}
                FormEvent::Changed(field::ROOT) => {
                    // Edit only: reveal (and default to No) the Move-files row
                    // once the root folder differs from the movie's current one.
                    if let Some(active) = self.form.as_mut()
                        && matches!(active.mode, FormMode::Edit { .. })
                    {
                        let moved = active.form.changed(field::ROOT);
                        if !moved {
                            active.form.reset(field::MOVE_FILES);
                        }
                        active.form.set_visible(field::MOVE_FILES, moved);
                    }
                }
                FormEvent::Changed(_) => {}
                FormEvent::Save => self.commit_form(),
                // Edit returns to the movie list/detail, add to the results
                // list; the underlying `Level` is untouched either way.
                FormEvent::Cancel => self.form = None,
            }
            return None;
        }

        // The delete-downloads prompt is modal: toggles + commit/cancel.
        if self.downloads.is_prompting() {
            if let downloads::DeleteAction::Commit {
                ids,
                remove_from_client,
                blocklist,
            } = self.downloads.handle_prompt_key(key)
            {
                let client = self.client.clone();
                self.spawn_command(CommandKind::DeleteQueue, async move {
                    client
                        .delete_queue_items(&ids, remove_from_client, blocklist)
                        .await
                });
            }
            return None;
        }

        // Popups are modal: while open each owns every key. Only y/n act,
        // per the `prompt::draw_confirm` key contract: Enter is what opened
        // the grab confirm, so honouring it would let a double-tap confirm.
        if self.confirm.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(confirm) = self.confirm.take() {
                        self.execute_confirm(confirm);
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.confirm = None;
                }
                _ => {}
            }
            return None;
        }

        // The delete-movie prompt is modal. Take it so the handler owns it
        // (which ends the borrow before we touch `self`); it goes back only
        // when the key was merely consumed — commit and cancel both close it.
        if let Some(mut prompt) = self.delete_prompt.take() {
            match prompt.handle_key(key) {
                delete_prompt::DeleteAction::Commit {
                    id,
                    delete_files,
                    add_exclusion,
                } => {
                    let client = self.client.clone();
                    self.spawn_command(CommandKind::DeleteMovie(id), async move {
                        client.delete_movie(id, delete_files, add_exclusion).await
                    });
                }
                delete_prompt::DeleteAction::Cancelled => {}
                delete_prompt::DeleteAction::Consumed => self.delete_prompt = Some(prompt),
            }
            return None;
        }

        if let Some(selected) = self.sort_menu {
            match key.code {
                KeyCode::Up => {
                    self.sort_menu = Some(selected.saturating_sub(1));
                }
                KeyCode::Down => {
                    self.sort_menu = Some((selected + 1).min(MOVIE_SORTS.len() - 1));
                }
                KeyCode::Enter => {
                    self.sort_menu = None;
                    let sort = MOVIE_SORTS.get(selected).copied().unwrap_or(MOVIE_SORTS[0]);
                    if sort != self.sort {
                        self.sort = sort;
                        display::sort_movies(&mut self.all_movies, sort);
                        self.movie_cursor = 0;
                        self.apply_filter();
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('v') => self.sort_menu = None,
                _ => {}
            }
            return None;
        }
        if let Some(selected) = self.search_menu {
            match key.code {
                KeyCode::Up => {
                    self.search_menu = Some(selected.saturating_sub(1));
                }
                KeyCode::Down => {
                    self.search_menu = Some((selected + 1).min(SEARCH_MENU_OPTIONS.len() - 1));
                }
                KeyCode::Enter => {
                    self.search_menu = None;
                    self.on_search_menu_choice(selected);
                }
                KeyCode::Esc | KeyCode::Char('q') => self.search_menu = None,
                _ => {}
            }
            return None;
        }
        if let Some(selected) = self.rejections_popup {
            let count = self
                .selected_release()
                .map(|release| release.rejections.len())
                .unwrap_or(0);
            match key.code {
                KeyCode::Up => {
                    self.rejections_popup = Some(selected.saturating_sub(1));
                }
                KeyCode::Down => {
                    self.rejections_popup = Some((selected + 1).min(count.saturating_sub(1)));
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('x') => {
                    self.rejections_popup = None;
                }
                _ => {}
            }
            return None;
        }

        // While the detail overview is expanded, the arrow keys scroll the
        // detail body and Esc collapses it. Everything else falls through so
        // the usual detail actions (search, delete, refresh) still work.
        if self.overview_expanded && matches!(self.level, Level::MovieDetail { .. }) {
            let page = self.last_height.saturating_sub(4).max(1) as usize;
            match key.code {
                KeyCode::Up => {
                    self.overview_scroll = self.overview_scroll.saturating_sub(1);
                    return None;
                }
                KeyCode::Down => {
                    self.overview_scroll += 1;
                    return None;
                }
                KeyCode::PageUp | KeyCode::Left => {
                    self.overview_scroll = self.overview_scroll.saturating_sub(page);
                    return None;
                }
                KeyCode::PageDown | KeyCode::Right => {
                    self.overview_scroll += page;
                    return None;
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.overview_scroll = 0;
                    return None;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.overview_scroll = usize::MAX;
                    return None;
                }
                KeyCode::Esc | KeyCode::Backspace => {
                    self.overview_expanded = false;
                    self.overview_scroll = 0;
                    return None;
                }
                _ => {}
            }
        }

        let page_jump = (self.last_height / 5).max(1) as usize;
        match key.code {
            KeyCode::Up => {
                if let Some((cursor, _)) = self.cursor_and_len() {
                    *cursor = cursor.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some((cursor, len)) = self.cursor_and_len()
                    && *cursor + 1 < len
                {
                    *cursor += 1;
                }
            }
            // `d` opens Downloads on the main list only; it is no longer a
            // page-down alias, so it is unbound on every other level.
            KeyCode::Char('d') if matches!(self.level, Level::MovieList) => {
                self.level = Level::Downloads;
                self.downloads.reset();
                self.fetch_queue();
            }
            KeyCode::PageUp | KeyCode::Left => {
                if let Some((cursor, _)) = self.cursor_and_len() {
                    *cursor = cursor.saturating_sub(page_jump);
                }
            }
            KeyCode::PageDown | KeyCode::Right => {
                if let Some((cursor, len)) = self.cursor_and_len() {
                    *cursor = (*cursor + page_jump).min(len.saturating_sub(1));
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                if let Some((cursor, _)) = self.cursor_and_len() {
                    *cursor = 0;
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if let Some((cursor, len)) = self.cursor_and_len() {
                    *cursor = len.saturating_sub(1);
                }
            }

            KeyCode::Char('/') if matches!(self.level, Level::MovieList) => {
                self.filter_active = true;
                self.filter_focused = true;
                self.filter.focused = true;
            }
            // Re-focus the add search box and scroll its results back to top.
            KeyCode::Char('/') if matches!(self.level, Level::Add) => {
                self.add_search_focused = true;
                self.add_search.focused = true;
                self.add_cursor = 0;
            }
            KeyCode::Char('a') if matches!(self.level, Level::MovieList) => self.enter_add(),
            KeyCode::Esc | KeyCode::Backspace => {
                if !self.pop_level() && self.filter_active && key.code == KeyCode::Esc {
                    self.filter.clear();
                    self.filter_active = false;
                    self.apply_filter();
                }
            }
            // On the Downloads level Space marks the highlighted row instead
            // of opening anything.
            KeyCode::Char(' ') if matches!(self.level, Level::Downloads) => {
                self.downloads.toggle_mark(&self.queue);
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.open_selected(),

            KeyCode::Char('v') if matches!(self.level, Level::MovieList) => {
                self.sort_menu = Some(
                    MOVIE_SORTS
                        .iter()
                        .position(|&sort| sort == self.sort)
                        .unwrap_or(0),
                );
            }
            // Toggle the full description in place. Only bound when the
            // overview is actually capped (`overview_truncatable`); the extra
            // `overview_expanded` keeps `i` able to collapse even if the pane
            // grew enough that the overview would now fit.
            KeyCode::Char('i')
                if matches!(self.level, Level::MovieDetail { .. })
                    && (self.overview_truncatable || self.overview_expanded) =>
            {
                self.overview_expanded = !self.overview_expanded;
                self.overview_scroll = 0;
            }
            KeyCode::Char('x') => match &self.level {
                Level::MovieDetail { movie } => {
                    if let Some(file) = &movie.movie_file {
                        self.confirm = Some(Confirm::DeleteFile { file_id: file.id });
                    }
                }
                Level::Search { .. }
                    if self
                        .selected_release()
                        .is_some_and(|release| !release.rejections.is_empty()) =>
                {
                    self.rejections_popup = Some(0);
                }
                Level::Downloads => self.downloads.begin_delete(&self.queue),
                _ => {}
            },
            KeyCode::Char('z') => {
                if let Level::MovieDetail { movie } = &self.level {
                    self.delete_prompt = Some(DeletePrompt::new(
                        movie.id,
                        movie
                            .title
                            .clone()
                            .unwrap_or_else(|| "this movie".to_string()),
                    ));
                }
            }
            KeyCode::Char('m') => self.toggle_monitored(),
            KeyCode::Char('o') => self.start_edit_flow(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('?') => self.show_full_help = !self.show_full_help,
            KeyCode::Char('q') => return Some(BrowseAction::Quit),
            _ => {}
        }
        None
    }

    /// Flip the monitored flag on the selected movie (list) or the movie the
    /// detail is showing. Copy the id/new-state out before spawning so the
    /// borrow of `self.level` ends before `spawn_command` takes `&mut self`.
    fn toggle_monitored(&mut self) {
        let (id, want) = match &self.level {
            Level::MovieList => match self.selected_movie() {
                Some(movie) => (movie.id, !movie.monitored),
                None => return,
            },
            Level::MovieDetail { movie } => (movie.id, !movie.monitored),
            _ => return,
        };
        let client = self.client.clone();
        self.spawn_command(CommandKind::SetMonitored, async move {
            client.set_movie_monitored(id, want).await
        });
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.last_height = area.height;
        // The background poll can shrink the queue under a stale cursor.
        self.downloads.clamp(self.queue.len());
        let has_message = self.error.is_some() || self.notice.is_some();
        let show_filter = self.filter_active && matches!(self.level, Level::MovieList);
        // The add search box gives way to the add form once it opens.
        let show_add_input = matches!(self.level, Level::Add) && self.form.is_none();
        let show_input = show_filter || show_add_input;
        let help_height = if self.show_full_help {
            help::rows(&self.help_sections())
        } else {
            1
        };

        let rows = Layout::vertical([
            Constraint::Length(if has_message { 1 } else { 0 }),
            Constraint::Length(2), // breadcrumb row + spacer
            Constraint::Length(if show_input { 2 } else { 0 }),
            Constraint::Fill(1), // level content
            Constraint::Length(help_height),
        ])
        .split(area);

        if let Some(error) = &self.error {
            Line::styled(format!("Error: {error}"), theme::error())
                .render(rows[0], frame.buffer_mut());
        } else if let Some(notice) = &self.notice {
            Line::styled(notice.clone(), theme::accent()).render(rows[0], frame.buffer_mut());
        }
        self.draw_breadcrumb(frame, rows[1]);
        if show_filter {
            self.draw_filter_row(frame, rows[2]);
        } else if show_add_input {
            self.draw_add_row(frame, rows[2]);
        }
        // An open form replaces the level content full-body.
        if let Some(active) = &self.form {
            active.form.draw(frame, rows[3], &active.title);
        } else {
            match &self.level {
                Level::MovieList => self.draw_movie_list(frame, rows[3]),
                Level::MovieDetail { .. } => self.draw_movie_detail(frame, rows[3]),
                Level::Search { .. } => self.draw_search(frame, rows[3]),
                Level::Add => self.draw_add_results(frame, rows[3]),
                Level::Downloads => self.draw_downloads(frame, rows[3]),
            }
        }
        self.draw_help(frame, rows[4]);

        if let Some(selected) = self.sort_menu {
            let labels: Vec<&str> = MOVIE_SORTS.iter().map(|sort| sort.label()).collect();
            prompt::draw_menu(frame, area, "Sort by", &labels, selected);
        }
        if let Some(selected) = self.search_menu {
            prompt::draw_menu(frame, area, "Movie search", &SEARCH_MENU_OPTIONS, selected);
        }
        if let Some(selected) = self.rejections_popup {
            let rejections: Vec<&str> = self
                .selected_release()
                .map(|release| release.rejections.iter().map(String::as_str).collect())
                .unwrap_or_default();
            prompt::draw_menu(frame, area, "Rejections", &rejections, selected);
        }
        if let Some(confirm) = &self.confirm {
            let question = match confirm {
                Confirm::DeleteFile { .. } => "Delete this movie file?",
                Confirm::Grab { index } => {
                    if self
                        .releases
                        .get(*index)
                        .is_some_and(|release| release.rejected)
                    {
                        "Release has rejections. Download anyway?"
                    } else {
                        "Download this release?"
                    }
                }
            };
            prompt::draw_confirm(frame, area, question);
        }
        if let Some(prompt) = &self.delete_prompt {
            prompt.draw(frame, area);
        }
        self.downloads.draw_prompt(frame, area);
    }

    fn breadcrumb(&self) -> String {
        let title = |movie: &Movie| movie.title.clone().unwrap_or_default();
        match &self.level {
            Level::MovieList => "Movies".to_string(),
            Level::MovieDetail { movie } => title(movie),
            Level::Search { movie } => format!("{} / search", title(movie)),
            Level::Add => {
                let term = self.add_search.value();
                if term.is_empty() {
                    "Add movie".to_string()
                } else {
                    format!("Add movie / {term}")
                }
            }
            Level::Downloads => {
                let marked = self.downloads.marked();
                if marked > 0 {
                    format!("Downloads ({marked} marked)")
                } else {
                    format!("Downloads ({})", self.queue.len())
                }
            }
        }
    }

    fn draw_breadcrumb(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(
                format!(" {} ", self.breadcrumb()),
                Style::new()
                    .fg(theme::on_accent())
                    .bg(theme::accent_color()),
            ),
        ];
        if self.loading {
            spans.push(Span::styled(
                format!(" {}", SPINNER_FRAMES[self.spinner_frame]),
                Style::new().fg(theme::accent_bright()),
            ));
        }
        Line::from(spans).render(area, frame.buffer_mut());
    }

    fn draw_filter_row(&self, frame: &mut Frame, area: Rect) {
        let [row, _] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
        let [prompt_area, input_area] =
            Layout::horizontal([Constraint::Length(10), Constraint::Fill(1)]).areas(row);
        Line::styled("  Filter: ", Style::new().fg(theme::fg()))
            .render(prompt_area, frame.buffer_mut());
        self.filter.render(input_area, frame.buffer_mut());
    }

    fn draw_add_row(&self, frame: &mut Frame, area: Rect) {
        let [row, _] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
        let [prompt_area, input_area] =
            Layout::horizontal([Constraint::Length(10), Constraint::Fill(1)]).areas(row);
        Line::styled("  Search: ", Style::new().fg(theme::fg()))
            .render(prompt_area, frame.buffer_mut());
        self.add_search.render(input_area, frame.buffer_mut());
    }

    /// Add-screen results: title, "year • rating • runtime", one truncated
    /// overview line, and a blank spacer (4 rows each).
    fn draw_add_results(&self, frame: &mut Frame, area: Rect) {
        let buf = frame.buffer_mut();
        if self.add_results.is_empty() {
            let message = if self.loading {
                "  Searching..."
            } else if self.add_search.value().trim().is_empty() {
                "  Type a title or tmdb:<id>, then enter to search."
            } else {
                "  No results."
            };
            Line::styled(message.to_string(), theme::dim()).render(area, buf);
            return;
        }
        let avail_height = area.height as usize;
        let items_per_page = (avail_height / 4).max(1);
        let first = list::window_start(self.add_cursor, self.add_results.len(), items_per_page);
        let last = (first + items_per_page).min(self.add_results.len());
        let text_width = area.width.saturating_sub(6) as usize;

        let gutter = |accent: bool| {
            if accent {
                Span::styled(" │ ", Style::new().fg(theme::accent_bright()))
            } else {
                Span::raw("   ")
            }
        };
        let row = |x: u16, y: u16| Rect::new(x, y, area.width.saturating_sub(1), 1);

        let mut y = area.y;
        for (i, movie) in self.add_results.iter().enumerate().take(last).skip(first) {
            let selected = i == self.add_cursor;
            let title = truncate(movie.title.as_deref().unwrap_or("(untitled)"), text_width);
            let meta = truncate(&display::movie_meta(movie), text_width);
            let overview = truncate(movie.overview.as_deref().unwrap_or_default(), text_width);

            let title_style = if selected {
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme::fg())
            };
            let meta_style = if selected {
                Style::new().fg(theme::accent_color())
            } else {
                theme::dim()
            };

            if y + 2 < area.y + area.height {
                Line::from(vec![gutter(selected), Span::styled(title, title_style)])
                    .render(row(area.x, y), buf);
                Line::from(vec![gutter(selected), Span::styled(meta, meta_style)])
                    .render(row(area.x, y + 1), buf);
                Line::from(vec![gutter(selected), Span::styled(overview, theme::dim())])
                    .render(row(area.x, y + 2), buf);
            }
            y += 4;
        }
        if self.add_results.len() > items_per_page {
            list::draw_scrollbar(buf, area, self.add_cursor, self.add_results.len());
        }
    }

    fn status_span(status: &MovieStatus) -> Span<'static> {
        match status {
            MovieStatus::Downloaded(quality) => {
                Span::styled(quality.clone(), Style::new().fg(theme::fg()))
            }
            MovieStatus::Downloading(percent) => Span::styled(
                format!("{} {percent}%", display::GLYPH_DOWNLOADING),
                Style::new().fg(theme::accent_bright()),
            ),
            MovieStatus::Missing => Span::styled("missing".to_string(), theme::error()),
            MovieStatus::Unreleased(date) => Span::styled(
                match date {
                    Some(date) => format!("unreleased {date}"),
                    None => "unreleased".to_string(),
                },
                theme::dim(),
            ),
        }
    }

    fn draw_movie_list(&self, frame: &mut Frame, area: Rect) {
        let buf = frame.buffer_mut();
        if self.filtered.is_empty() {
            let message = if self.loading {
                "  Loading..."
            } else {
                "  No movies."
            };
            Line::styled(message.to_string(), theme::dim()).render(area, buf);
            return;
        }
        let avail_height = area.height as usize;
        let items_per_page = (avail_height / 3).max(1);
        let first = list::window_start(self.movie_cursor, self.filtered.len(), items_per_page);
        let last = (first + items_per_page).min(self.filtered.len());
        let now = display::now_utc_iso();
        let text_width = area.width.saturating_sub(6) as usize;
        let title_width = area.width.saturating_sub(MOVIE_META_WIDTH + 6).max(4) as usize;

        let mut y = area.y;
        for (i, movie) in self
            .filtered
            .iter()
            .filter_map(|&index| self.all_movies.get(index))
            .enumerate()
            .take(last)
            .skip(first)
        {
            let queue_entry = display::movie_queue_entry(&self.queue, movie.id);
            let status = display::movie_status(movie, queue_entry, &now);
            let unreleased = matches!(status, MovieStatus::Unreleased(_));
            let selected = i == self.movie_cursor;

            let title = truncate(movie.title.as_deref().unwrap_or("(untitled)"), title_width);
            // Reserve room for the unmonitored marker so a long meta line
            // can't clip it, mirroring the Sonarr episode row.
            let mon_word = if movie.monitored {
                ""
            } else {
                display::monitored_label(false)
            };
            let meta_reserve = if mon_word.is_empty() {
                0
            } else {
                mon_word.chars().count() + 3 // " • " separator
            };
            let meta = truncate(
                &display::movie_meta(movie),
                text_width.saturating_sub(meta_reserve),
            );

            // Unmonitored movies dim like unreleased ones, and carry an
            // explicit marker on the meta line so the state is unmistakable.
            let title_style = if selected {
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD)
            } else if unreleased || !movie.monitored {
                theme::dim()
            } else {
                Style::new().fg(theme::fg())
            };
            let gutter = |accent: bool| {
                if accent {
                    Span::styled(" │ ", Style::new().fg(theme::accent_bright()))
                } else {
                    Span::raw("   ")
                }
            };

            if y + 1 < area.y + area.height {
                let title_area = Rect::new(area.x, y, area.width.saturating_sub(1), 1);
                let [left, right] =
                    Layout::horizontal([Constraint::Fill(1), Constraint::Length(MOVIE_META_WIDTH)])
                        .areas(title_area);
                Line::from(vec![gutter(selected), Span::styled(title, title_style)])
                    .render(left, buf);
                Line::from(vec![Self::status_span(&status), Span::raw(" ")])
                    .right_aligned()
                    .render(right, buf);
                let meta_style = if selected {
                    Style::new().fg(theme::accent_color())
                } else {
                    theme::dim()
                };
                let mut meta_spans = vec![gutter(selected)];
                if !meta.is_empty() {
                    meta_spans.push(Span::styled(meta, meta_style));
                }
                if !mon_word.is_empty() {
                    let prefix = if meta_spans.len() > 1 { " • " } else { "" };
                    meta_spans.push(Span::styled(format!("{prefix}{mon_word}"), theme::dim()));
                }
                Line::from(meta_spans).render(
                    Rect::new(area.x, y + 1, area.width.saturating_sub(1), 1),
                    buf,
                );
            }
            y += 3;
        }
        if self.filtered.len() > items_per_page {
            list::draw_scrollbar(buf, area, self.movie_cursor, self.filtered.len());
        }
    }

    /// The download queue as a markable list. The shared renderer handles
    /// layout and selection; this only resolves the movie name from the cache
    /// (falling back to the release title on the queue item).
    fn draw_downloads(&self, frame: &mut Frame, area: Rect) {
        self.downloads
            .draw_list(frame, area, &self.queue, |item| downloads::DownloadRow {
                name: item
                    .movie_id
                    .and_then(|id| self.all_movies.iter().find(|movie| movie.id == id))
                    .and_then(|movie| movie.title.clone())
                    .or_else(|| item.title.clone())
                    .unwrap_or_else(|| "(unknown)".to_string()),
                ..Default::default()
            });
    }

    /// Movie header — title and "year • rating • runtime", pinned above the
    /// scrollable detail body. Returns the area left below it for the body
    /// (overview plus the file block / status line).
    fn draw_movie_header(&self, frame: &mut Frame, area: Rect, movie: &Movie) -> Rect {
        let buf = frame.buffer_mut();

        let mut y = area.y;
        let mut line = |text: Line, y: &mut u16| {
            if *y < area.y + area.height {
                text.render(Rect::new(area.x, *y, area.width.saturating_sub(1), 1), buf);
            }
            *y += 1;
        };

        line(
            Line::from(Span::styled(
                format!("  {}", movie.title.as_deref().unwrap_or("(untitled)")),
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD),
            )),
            &mut y,
        );
        // Always surface the monitored state in the header (there is room),
        // appended to the meta line like the Sonarr season label.
        let monitored = display::monitored_label(movie.monitored);
        let meta = match display::movie_meta(movie) {
            m if m.is_empty() => monitored.to_string(),
            m => format!("{m} • {monitored}"),
        };
        line(
            Line::from(Span::styled(format!("  {meta}"), theme::dim())),
            &mut y,
        );
        y += 1;

        let bottom = area.y + area.height;
        Rect::new(area.x, y.min(bottom), area.width, bottom.saturating_sub(y))
    }

    /// The movie detail: title/meta pinned by `draw_movie_header`, then a
    /// scrollable body of the overview and the file block (or status line).
    /// Collapsed, the overview is capped to ~40% of the pane and the body does
    /// not scroll; `i` (only offered when capped) expands it to the full
    /// overview, and the body then scrolls with `overview_scroll`.
    fn draw_movie_detail(&mut self, frame: &mut Frame, area: Rect) {
        // Build the pinned header and the body under an immutable borrow of
        // `self.level`, then store the derived flag and render the scroll
        // window under a mutable one.
        let (body, lines, truncatable) = {
            let Level::MovieDetail { movie } = &self.level else {
                return;
            };
            let body = self.draw_movie_header(frame, area, movie);
            let text_width = body.width.saturating_sub(6) as usize;

            let overview_lines =
                wrap_text(movie.overview.as_deref().unwrap_or_default(), text_width);
            // Cap relative to the full detail area (as before) so the collapsed
            // view always leaves room for the file block below the overview.
            let cap = ((area.height as usize) * 2 / 5).max(1);
            let truncatable = overview_lines.len() > cap;
            let shown = if self.overview_expanded {
                overview_lines.len()
            } else {
                overview_lines.len().min(cap)
            };

            let mut lines: Vec<Line> = Vec::new();
            for text in overview_lines.iter().take(shown) {
                lines.push(Line::from(Span::styled(
                    format!("  {text}"),
                    Style::new().fg(theme::fg()),
                )));
            }
            if truncatable {
                let hint = if self.overview_expanded {
                    "  (i: collapse)"
                } else {
                    "  … (i: expand)"
                };
                lines.push(Line::from(Span::styled(hint, theme::dim())));
            }
            lines.push(Line::from(""));

            match &movie.movie_file {
                // No file: one status line instead of the file block.
                None => {
                    let queue_entry = display::movie_queue_entry(&self.queue, movie.id);
                    let status = display::movie_status(movie, queue_entry, &display::now_utc_iso());
                    let hint = match status {
                        MovieStatus::Missing => Span::styled("  (enter: search)", theme::dim()),
                        _ => Span::raw(""),
                    };
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Self::status_span(&status),
                        hint,
                    ]));
                }
                Some(file) => {
                    let path_width = body.width.saturating_sub(16) as usize;
                    let none = || "-".to_string();
                    let mut entries: Vec<(&str, String)> = vec![
                        (
                            "Path",
                            file.path
                                .as_deref()
                                .map(|path| truncate(path, path_width))
                                .unwrap_or_else(none),
                        ),
                        ("Size", display::format_size(file.size)),
                        (
                            "Quality",
                            file.quality
                                .as_ref()
                                .and_then(|q| q.quality.name.clone())
                                .unwrap_or_else(none),
                        ),
                    ];
                    let languages: Vec<&str> = file
                        .languages
                        .iter()
                        .filter_map(|language| language.name.as_deref())
                        .collect();
                    let language = if languages.is_empty() {
                        file.language
                            .as_ref()
                            .and_then(|language| language.name.clone())
                            .unwrap_or_else(none)
                    } else {
                        languages.join(", ")
                    };
                    entries.push(("Language", language));
                    if let Some(info) = &file.media_info {
                        let join = |parts: Vec<Option<String>>| {
                            let joined: Vec<String> = parts.into_iter().flatten().collect();
                            if joined.is_empty() {
                                none()
                            } else {
                                joined.join(" • ")
                            }
                        };
                        entries.push((
                            "Video",
                            join(vec![
                                info.video_codec.clone(),
                                info.resolution.clone(),
                                info.video_dynamic_range.clone(),
                            ]),
                        ));
                        entries.push((
                            "Audio",
                            join(vec![
                                info.audio_codec.clone(),
                                info.audio_channels.map(|channels| format!("{channels} ch")),
                                info.audio_languages.clone(),
                            ]),
                        ));
                        entries.push((
                            "Subtitles",
                            info.subtitles
                                .clone()
                                .filter(|subs| !subs.is_empty())
                                .unwrap_or_else(none),
                        ));
                        entries.push((
                            "Runtime",
                            info.run_time
                                .clone()
                                .or_else(|| display::format_runtime(movie.runtime))
                                .unwrap_or_else(none),
                        ));
                    }
                    if let Some(edition) = file
                        .edition
                        .as_deref()
                        .filter(|edition| !edition.is_empty())
                    {
                        entries.push(("Edition", edition.to_string()));
                    }

                    for (label, value) in entries {
                        lines.push(Line::from(vec![
                            Span::styled(format!("  {label:<11}"), theme::selected()),
                            Span::styled(format!(" {value}"), Style::new().fg(theme::fg())),
                        ]));
                    }
                }
            }

            (body, lines, truncatable)
        };

        self.overview_truncatable = truncatable;

        let height = body.height as usize;
        if height == 0 {
            self.overview_scroll = 0;
            return;
        }

        // Only the expanded body scrolls; collapsed always fits (or clips as
        // before) and shows no scrollbar.
        let total = lines.len();
        if self.overview_expanded {
            self.overview_scroll = self.overview_scroll.min(total.saturating_sub(height));
        } else {
            self.overview_scroll = 0;
        }

        let buf = frame.buffer_mut();
        for (row, line) in lines
            .into_iter()
            .skip(self.overview_scroll)
            .take(height)
            .enumerate()
        {
            line.render(
                Rect::new(body.x, body.y + row as u16, body.width.saturating_sub(1), 1),
                buf,
            );
        }
        if self.overview_expanded && total > height {
            list::draw_scrollbar(
                buf,
                body,
                self.overview_scroll,
                total.saturating_sub(height) + 1,
            );
        }
    }

    fn draw_search(&self, frame: &mut Frame, area: Rect) {
        let buf = frame.buffer_mut();
        if self.releases.is_empty() {
            let message = if self.loading {
                "  Searching indexers... this can take a while."
            } else {
                "  No releases found."
            };
            Line::styled(message.to_string(), theme::dim()).render(area, buf);
            return;
        }
        let avail_height = area.height as usize;
        let items_per_page = (avail_height / 3).max(1);
        let first = list::window_start(self.release_cursor, self.releases.len(), items_per_page);
        let last = (first + items_per_page).min(self.releases.len());
        let text_width = area.width.saturating_sub(6) as usize;

        let mut y = area.y;
        for (i, release) in self.releases.iter().enumerate().take(last).skip(first) {
            let selected = i == self.release_cursor;
            let title = truncate(release.title.as_deref().unwrap_or("(untitled)"), text_width);
            let gutter = |accent: bool| {
                if accent {
                    Span::styled(" │ ", Style::new().fg(theme::accent_bright()))
                } else {
                    Span::raw("   ")
                }
            };
            let title_style = if selected {
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme::fg())
            };
            let detail_style = if selected {
                Style::new().fg(theme::accent_color())
            } else {
                theme::dim()
            };
            let mut detail = vec![
                gutter(selected),
                Span::styled(
                    truncate(
                        &display::release_line2(release),
                        text_width.saturating_sub(4),
                    ),
                    detail_style,
                ),
            ];
            if release.rejected {
                detail.push(Span::styled(
                    format!(" {}", display::GLYPH_REJECTED),
                    theme::error(),
                ));
            }
            if display::previously_grabbed(&self.history, release) {
                detail.push(Span::styled(
                    format!(" {}", display::GLYPH_GRABBED),
                    Style::new().fg(theme::accent_bright()),
                ));
            }
            if y + 1 < area.y + area.height {
                Line::from(vec![gutter(selected), Span::styled(title, title_style)])
                    .render(Rect::new(area.x, y, area.width.saturating_sub(1), 1), buf);
                Line::from(detail).render(
                    Rect::new(area.x, y + 1, area.width.saturating_sub(1), 1),
                    buf,
                );
            }
            y += 3;
        }
        if self.releases.len() > items_per_page {
            list::draw_scrollbar(buf, area, self.release_cursor, self.releases.len());
        }
    }

    fn help_entries(&self) -> Vec<(&'static str, &'static str)> {
        if self.filter_focused {
            return vec![("esc", "cancel"), ("enter", "apply")];
        }
        if self.add_search_focused {
            return vec![("esc", "unfocus"), ("enter", "search")];
        }
        if self.form.is_some() {
            return vec![
                ("↑/↓", "move"),
                ("←/→", "change"),
                ("enter", "select"),
                ("esc", "cancel"),
            ];
        }
        if self.confirm.is_some() {
            return vec![("y", "confirm"), ("n", "cancel")];
        }
        if self.delete_prompt.is_some() {
            return vec![
                ("↑/↓", "move"),
                ("space", "toggle"),
                ("enter", "delete"),
                ("esc", "cancel"),
            ];
        }
        if self.downloads.is_prompting() {
            return vec![
                ("↑/↓", "move"),
                ("space", "toggle"),
                ("enter", "remove"),
                ("esc", "cancel"),
            ];
        }
        if self.sort_menu.is_some() || self.search_menu.is_some() {
            return vec![("esc", "close"), ("enter", "select")];
        }
        if self.rejections_popup.is_some() {
            return vec![("esc", "close")];
        }
        let mut entries: Vec<(&'static str, &'static str)> = Vec::new();
        match &self.level {
            Level::MovieList => {
                entries.push(("enter", "open"));
                entries.push(("a", "add"));
                entries.push(("d", "downloads"));
                entries.push(("/", "filter"));
                if self.filter_active {
                    entries.push(("esc", "clear"));
                }
                entries.push(("v", "sort"));
                entries.push(("m", "monitor"));
                entries.push(("o", "options"));
            }
            Level::Downloads => {
                entries.push(("esc", "back"));
                entries.push(("space", "mark"));
                entries.push(("x", "remove"));
            }
            Level::MovieDetail { movie } => {
                entries.push(("esc", "back"));
                entries.push(("enter", "search"));
                if movie.movie_file.is_some() {
                    entries.push(("x", "delete file"));
                }
                entries.push(("z", "delete"));
                entries.push(("m", "monitor"));
                entries.push(("o", "options"));
                // Only offered when the overview is actually capped.
                if self.overview_truncatable {
                    entries.push((
                        "i",
                        if self.overview_expanded {
                            "collapse"
                        } else {
                            "expand"
                        },
                    ));
                }
                if self.overview_expanded {
                    entries.push(("↑/↓", "scroll"));
                }
            }
            Level::Search { .. } => {
                entries.push(("esc", "back"));
                entries.push(("enter", "download"));
                if self
                    .selected_release()
                    .is_some_and(|release| !release.rejections.is_empty())
                {
                    entries.push(("x", "rejections"));
                }
            }
            Level::Add => {
                entries.push(("enter", "add"));
                entries.push(("/", "search"));
                entries.push(("esc", "back"));
            }
        }
        entries.push(("r", "refresh"));
        entries.push((
            "?",
            if self.show_full_help {
                "close help"
            } else {
                "help"
            },
        ));
        entries.push(("q", "quit"));
        entries
    }

    /// Columns for the expanded (`?`) help: shared navigation, the
    /// level-aware `help_entries`, and the symbol legend as its own labelled
    /// column so the glyphs are never mistaken for keybindings.
    fn help_sections(&self) -> Vec<help::Section> {
        vec![
            help::Section {
                title: "Move",
                entries: help::NAV.to_vec(),
            },
            help::Section {
                title: "Actions",
                entries: self.help_entries(),
            },
            help::Section {
                title: "Symbols",
                entries: display::SYMBOL_LEGEND.to_vec(),
            },
        ]
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        if !self.show_full_help {
            let buf = frame.buffer_mut();
            let mut spans = vec![Span::raw("  ")];
            for (i, (key, desc)) in self.help_entries().into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" • ", theme::dim()));
                }
                spans.push(Span::styled(key, Style::new().fg(theme::dim_color())));
                spans.push(Span::styled(format!(" {desc}"), theme::dim()));
            }
            Line::from(spans).render(area, buf);
            return;
        }
        help::draw(frame, area, &self.help_sections());
    }
}

const SEARCH_MENU_OPTIONS: [&str; 3] = ["Auto search", "Interactive search", "Cancel"];

/// Radarr's minimumAvailability options for the add/edit forms: (menu label,
/// API value).
const MIN_AVAILABILITY: [(&str, &str); 3] = [
    ("Announced", "announced"),
    ("In Cinemas", "inCinemas"),
    ("Released", "released"),
];

/// The minimum-availability labels as owned strings for a form field.
fn availability_choices() -> Vec<String> {
    MIN_AVAILABILITY
        .iter()
        .map(|(label, _)| label.to_string())
        .collect()
}

/// The add wizard pre-selects the first quality profile whose name contains
/// "1080" (case-insensitive) so the common HD choice needs no scrolling,
/// falling back to the first profile when none match.
fn preferred_quality_index(profiles: &[QualityProfile]) -> usize {
    profiles
        .iter()
        .position(|p| p.name.to_lowercase().contains("1080"))
        .unwrap_or(0)
}

/// Index of the "Released" minimum-availability option so the step opens on
/// it; falls back to the first option if the table is ever reordered.
fn released_availability_index() -> usize {
    MIN_AVAILABILITY
        .iter()
        .position(|(_, value)| *value == "released")
        .unwrap_or(0)
}

/// Index of the movie's current quality profile, or `None` if it no longer
/// exists in the profile list.
fn current_quality_index(profiles: &[QualityProfile], id: i64) -> Option<usize> {
    profiles.iter().position(|p| p.id == id)
}

/// Index into `MIN_AVAILABILITY` for the movie's current value, falling back
/// to "Released" (the add-wizard default) when unset or unrecognised.
fn current_availability_index(current: Option<&str>) -> usize {
    current
        .and_then(|c| MIN_AVAILABILITY.iter().position(|(_, value)| *value == c))
        .unwrap_or_else(released_availability_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: i64, name: &str) -> QualityProfile {
        QualityProfile {
            id,
            name: name.to_string(),
        }
    }

    #[test]
    fn preferred_quality_index_picks_first_1080() {
        let profiles = [
            profile(1, "Any"),
            profile(2, "SD"),
            profile(3, "HD-1080p"),
            profile(4, "Ultra-HD"),
        ];
        assert_eq!(preferred_quality_index(&profiles), 2);
    }

    #[test]
    fn preferred_quality_index_is_case_insensitive() {
        let profiles = [profile(1, "Any"), profile(2, "HD - 1080P")];
        assert_eq!(preferred_quality_index(&profiles), 1);
    }

    #[test]
    fn preferred_quality_index_falls_back_to_first() {
        let profiles = [profile(1, "Any"), profile(2, "SD"), profile(3, "HD-720p")];
        assert_eq!(preferred_quality_index(&profiles), 0);
    }

    #[test]
    fn preferred_quality_index_empty_is_zero() {
        assert_eq!(preferred_quality_index(&[]), 0);
    }

    #[test]
    fn released_availability_index_matches_table() {
        assert_eq!(
            MIN_AVAILABILITY[released_availability_index()].1,
            "released"
        );
    }

    #[test]
    fn current_quality_index_matches_id_else_none() {
        let profiles = [profile(3, "SD"), profile(7, "HD-1080p")];
        assert_eq!(current_quality_index(&profiles, 7), Some(1));
        assert_eq!(current_quality_index(&profiles, 99), None);
    }

    #[test]
    fn current_availability_index_maps_value_and_falls_back() {
        assert_eq!(
            MIN_AVAILABILITY[current_availability_index(Some("inCinemas"))].1,
            "inCinemas"
        );
        assert_eq!(
            MIN_AVAILABILITY[current_availability_index(None)].1,
            "released"
        );
        assert_eq!(
            MIN_AVAILABILITY[current_availability_index(Some("bogus"))].1,
            "released"
        );
    }

    #[test]
    fn queue_poll_interval_backs_off_and_caps() {
        assert_eq!(queue_poll_interval(0), QUEUE_POLL_TICKS);
        assert_eq!(queue_poll_interval(1), QUEUE_POLL_TICKS * 2);
        assert_eq!(
            queue_poll_interval(QUEUE_BACKOFF_MAX_SHIFT),
            QUEUE_POLL_MAX_TICKS
        );
        // Beyond the cap (and an absurd shift) stays at the max, never overflows.
        assert_eq!(
            queue_poll_interval(QUEUE_BACKOFF_MAX_SHIFT + 4),
            QUEUE_POLL_MAX_TICKS
        );
        assert_eq!(queue_poll_interval(64), QUEUE_POLL_MAX_TICKS);
    }
}
