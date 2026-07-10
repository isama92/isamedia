//! The Sonarr browse screen: series list, show detail (seasons), episode
//! list, episode file detail, and interactive search — a flat `Level` enum
//! rather than a stack, since every pop target is deterministic.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::event::AppSender;
use crate::sonarr::display::{self, EpStatus, SERIES_SORTS, SeriesSort};
use crate::sonarr::models::Season;
use crate::sonarr::{
    Client, Episode, HistoryRecord, QualityProfile, QueueItem, Release, RootFolder, Series,
};
use crate::ui::input::TextInput;
use crate::ui::text::{truncate, wrap_text};
use crate::ui::{prompt, theme};

use super::msg::{CommandKind, Msg};

const SPINNER_FRAMES: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

/// Queue polls fire every this many 250ms ticks (~10s) while inside a show.
const QUEUE_POLL_TICKS: u32 = 40;

/// Right-hand column budget of an episode row: air date, status, a gap.
const EPISODE_META_WIDTH: u16 = 34;

/// Where the user is inside the Sonarr app. Pops are deterministic:
/// EpisodeDetail/Search → Episodes → Show → SeriesList.
#[allow(clippy::large_enum_variant)] // one instance per Browse, size is irrelevant
enum Level {
    SeriesList,
    Show {
        series: Series,
    },
    Episodes {
        series: Series,
        season: Season,
    },
    /// Only entered for episodes that have a file.
    EpisodeDetail {
        series: Series,
        season: Season,
        episode: Episode,
    },
    /// Interactive search results for one missing (or upgradeable) episode.
    Search {
        series: Series,
        season: Season,
        episode: Episode,
    },
    /// Add a new series: a persistent search box over series/lookup and its
    /// results, from which the add wizard starts.
    Add,
}

/// A pending yes/no decision; modal while set.
enum Confirm {
    DeleteFile { file_id: i64 },
    Grab { index: usize },
}

/// One step of the add wizard, in order. Sonarr has no minimum-availability
/// step (that is a Radarr-only concept).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AddStep {
    RootFolder,
    QualityProfile,
    Monitored,
    Download,
}

impl AddStep {
    /// The step after this one, or `None` to commit the add.
    fn next(self) -> Option<AddStep> {
        match self {
            AddStep::RootFolder => Some(AddStep::QualityProfile),
            AddStep::QualityProfile => Some(AddStep::Monitored),
            AddStep::Monitored => Some(AddStep::Download),
            AddStep::Download => None,
        }
    }
}

/// The in-progress add wizard: which result is being added, the current step
/// with its selection cursor, and the choices gathered so far. Modal while
/// `Some`.
struct AddFlow {
    result: usize,
    step: AddStep,
    cursor: usize,
    root_folder: Option<String>,
    quality_profile_id: Option<i64>,
    monitored: bool,
    search: bool,
}

/// What the browse screen wants its parent (the Sonarr app) to do.
pub enum BrowseAction {
    Quit,
}

pub struct Browse {
    pub client: Client,
    sender: AppSender,

    level: Level,

    all_series: Vec<Series>,
    /// Currently visible series (filter applied).
    series: Vec<Series>,
    series_cursor: usize,
    season_cursor: usize,
    episodes: Vec<Episode>,
    episode_cursor: usize,
    releases: Vec<Release>,
    release_cursor: usize,
    /// Download queue, refreshed by the poll; drives every ↓ marker.
    queue: Vec<QueueItem>,
    /// History of the episode currently being searched interactively.
    history: Vec<HistoryRecord>,

    filter: TextInput,
    filter_active: bool,
    filter_focused: bool,
    sort: SeriesSort,

    /// Sort popup cursor into `SERIES_SORTS`; `None` when closed.
    sort_menu: Option<usize>,
    /// "Auto search / Interactive search / Cancel" popup cursor.
    search_menu: Option<usize>,
    confirm: Option<Confirm>,
    /// Cursor into the selected release's rejection list; `None` when closed.
    rejections_popup: Option<usize>,

    // The add-new flow (Level::Add): a persistent search box, its lookup
    // results, the wizard prerequisites, and the wizard itself when open.
    add_search: TextInput,
    add_search_focused: bool,
    add_results: Vec<Series>,
    add_cursor: usize,
    root_folders: Vec<RootFolder>,
    quality_profiles: Vec<QualityProfile>,
    /// Generation of the latest add-prerequisites fetch, independent of list
    /// fetches so a lookup can never invalidate it.
    add_prereq_gen: u64,
    add_flow: Option<AddFlow>,

    /// Full-screen scrollable overview on the show level.
    overview_fullscreen: bool,
    overview_scroll: usize,

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
            level: Level::SeriesList,
            all_series: Vec::new(),
            series: Vec::new(),
            series_cursor: 0,
            season_cursor: 0,
            episodes: Vec::new(),
            episode_cursor: 0,
            releases: Vec::new(),
            release_cursor: 0,
            queue: Vec::new(),
            history: Vec::new(),
            filter: TextInput::default(),
            filter_active: false,
            filter_focused: false,
            sort: SERIES_SORTS[0],
            sort_menu: None,
            search_menu: None,
            confirm: None,
            rejections_popup: None,
            add_search: TextInput::default(),
            add_search_focused: false,
            add_results: Vec::new(),
            add_cursor: 0,
            root_folders: Vec::new(),
            quality_profiles: Vec::new(),
            add_prereq_gen: 0,
            add_flow: None,
            overview_fullscreen: false,
            overview_scroll: 0,
            show_full_help: false,
            loading: false,
            queue_loading: false,
            error: None,
            notice: None,
            gen_counter,
            fetch_gen: 0,
            queue_gen: 0,
            has_fetched: false,
            tick_count: 0,
            spinner_frame: 0,
            last_height: 24,
        };
        browse.fetch_series();
        browse
    }

    pub fn on_tick(&mut self) {
        if self.loading {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
        }
        self.tick_count = self.tick_count.wrapping_add(1);
        // Keep the downloading markers live while inside a show; the series
        // list has none, so it doesn't poll.
        if !matches!(self.level, Level::SeriesList)
            && self.tick_count.is_multiple_of(QUEUE_POLL_TICKS)
        {
            self.fetch_queue();
        }
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

    fn fetch_series(&mut self) {
        let fetch_gen = self.begin_fetch();
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.get_series().await;
            sender.send(Msg::SeriesLoaded { fetch_gen, result });
        });
    }

    fn fetch_episodes(&mut self, series_id: i64, season_number: i32) {
        let fetch_gen = self.begin_fetch();
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.get_episodes(series_id, season_number).await;
            sender.send(Msg::EpisodesLoaded { fetch_gen, result });
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

    /// Interactive search: releases and the episode's history fetched
    /// concurrently under one generation. The history only feeds the
    /// "grabbed before" marker, so it may trickle in after the releases.
    fn fetch_releases(&mut self, episode_id: i64) {
        let fetch_gen = self.begin_fetch();
        self.releases.clear();
        self.history.clear();
        self.release_cursor = 0;
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.search_releases(episode_id).await;
            sender.send(Msg::ReleasesLoaded { fetch_gen, result });
        });
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.get_history(episode_id).await;
            sender.send(Msg::HistoryLoaded { fetch_gen, result });
        });
    }

    /// Run a mutation under the CURRENT view generation: if the user
    /// refetches or navigates somewhere that refetches before it lands, the
    /// result is stale and dropped (same contract as Jellyfin's
    /// watched-toggle).
    fn spawn_command(
        &mut self,
        kind: CommandKind,
        task: impl Future<Output = Result<(), crate::sonarr::Error>> + Send + 'static,
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
    pub fn on_series_loaded(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<Series>, crate::sonarr::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // stale response from a superseded fetch
        }
        self.loading = false;
        match result {
            Ok(mut list) => {
                display::sort_series(&mut list, self.sort);
                // A reload with a show open (refresh from the show level)
                // must also refresh the embedded copy the header and season
                // list render from.
                let embedded = match &mut self.level {
                    Level::SeriesList | Level::Add => None,
                    Level::Show { series }
                    | Level::Episodes { series, .. }
                    | Level::EpisodeDetail { series, .. }
                    | Level::Search { series, .. } => Some(series),
                };
                if let Some(series) = embedded
                    && let Some(updated) = list.iter().find(|s| s.id == series.id)
                {
                    *series = updated.clone();
                }
                self.all_series = list;
                self.apply_filter();
            }
            Err(crate::sonarr::Error::Unauthorized) => return true,
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Same contract as `on_series_loaded`.
    #[must_use]
    pub fn on_episodes_loaded(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<Episode>, crate::sonarr::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // stale response from a superseded fetch
        }
        self.loading = false;
        match result {
            Ok(mut episodes) => {
                episodes.sort_by_key(|episode| episode.episode_number);
                self.episodes = episodes;
                if self.episode_cursor >= self.episodes.len() {
                    self.episode_cursor = 0;
                }
            }
            Err(crate::sonarr::Error::Unauthorized) => return true,
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Same contract as `on_series_loaded`. A failed poll is cosmetic (the
    /// markers just go stale until the next one) and must not stomp the
    /// screen with an error every 10 seconds — except a 401, which means the
    /// key was revoked and is reported like everywhere else.
    #[must_use]
    pub fn on_queue_loaded(
        &mut self,
        queue_gen: u64,
        result: Result<Vec<QueueItem>, crate::sonarr::Error>,
    ) -> bool {
        if queue_gen != self.queue_gen {
            return false; // stale poll from a superseded queue fetch
        }
        self.queue_loading = false;
        match result {
            Ok(queue) => self.queue = queue,
            Err(crate::sonarr::Error::Unauthorized) => return true,
            Err(err) => tracing::debug!(%err, "queue poll failed"),
        }
        false
    }

    /// Same contract as `on_series_loaded`.
    #[must_use]
    pub fn on_releases_loaded(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<Release>, crate::sonarr::Error>,
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
            Err(crate::sonarr::Error::Unauthorized) => return true,
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Same contract as `on_series_loaded`. A missing history only costs
    /// the "grabbed before" marker, so failures are logged, not shown.
    #[must_use]
    pub fn on_history_loaded(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<HistoryRecord>, crate::sonarr::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // stale response from a superseded search
        }
        match result {
            Ok(history) => self.history = history,
            Err(crate::sonarr::Error::Unauthorized) => return true,
            Err(err) => tracing::debug!(%err, "episode history fetch failed"),
        }
        false
    }

    /// Same contract as `on_series_loaded`.
    #[must_use]
    pub fn on_command_done(
        &mut self,
        fetch_gen: u64,
        kind: CommandKind,
        result: Result<(), crate::sonarr::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // superseded: the user moved on meanwhile
        }
        if matches!(result, Err(crate::sonarr::Error::Unauthorized)) {
            return true;
        }
        self.loading = false;
        if let Err(err) = result {
            self.error = Some(err.to_string());
            return false;
        }
        match kind {
            CommandKind::AutoSearch => {
                self.notice = Some("search started; grabbed downloads appear in the queue".into());
                // Surface the ↓ marker as soon as Sonarr grabs something.
                self.fetch_queue();
            }
            CommandKind::Grab => {
                self.pop_to_episodes();
                // Set AFTER the refetch, which clears the notice row.
                self.notice = Some("release sent to the download client".into());
            }
            CommandKind::DeleteFile => {
                self.pop_to_episodes();
                self.notice = Some("episode file deleted".into());
            }
        }
        false
    }

    /// Add-flow lookup results. Same contract as `on_series_loaded`; shares
    /// `fetch_gen`, so a superseded lookup is dropped.
    #[must_use]
    pub fn on_lookup_loaded(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<Series>, crate::sonarr::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // stale response from a superseded lookup
        }
        self.loading = false;
        match result {
            Ok(list) => {
                self.add_results = list;
                self.add_cursor = 0;
            }
            Err(crate::sonarr::Error::Unauthorized) => return true,
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
        result: Result<(Vec<RootFolder>, Vec<QualityProfile>), crate::sonarr::Error>,
    ) -> bool {
        if prereq_gen != self.add_prereq_gen {
            return false;
        }
        match result {
            Ok((folders, profiles)) => {
                self.root_folders = folders;
                self.quality_profiles = profiles;
            }
            Err(crate::sonarr::Error::Unauthorized) => return true,
            Err(err) => tracing::debug!(%err, "add options fetch failed"),
        }
        false
    }

    /// The add finished. Same contract as `on_series_loaded`; on success we
    /// jump straight to the created series' show page and refetch the library
    /// so it is present when the user pops back.
    #[must_use]
    pub fn on_series_added(
        &mut self,
        fetch_gen: u64,
        result: Result<Box<Series>, crate::sonarr::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // superseded: the user moved on meanwhile
        }
        if matches!(result, Err(crate::sonarr::Error::Unauthorized)) {
            return true;
        }
        self.loading = false;
        match result {
            Ok(series) => {
                self.season_cursor = 0;
                self.overview_scroll = 0;
                self.level = Level::Show { series: *series };
                self.add_results.clear();
                self.add_search.clear();
                self.add_search_focused = false;
                self.fetch_series();
                self.fetch_queue();
                // Set AFTER the refetch, which clears the notice row.
                self.notice = Some("series added".into());
            }
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Return from a detail/search level to the episode list and refetch it
    /// (and the queue): both callers just changed server state.
    fn pop_to_episodes(&mut self) {
        let (series, season) = match std::mem::replace(&mut self.level, Level::SeriesList) {
            Level::EpisodeDetail { series, season, .. } | Level::Search { series, season, .. } => {
                (series, season)
            }
            other => {
                self.level = other;
                return;
            }
        };
        self.fetch_episodes(series.id, season.season_number);
        self.fetch_queue();
        self.level = Level::Episodes { series, season };
    }

    fn apply_filter(&mut self) {
        if !self.filter_active || self.filter.value().is_empty() {
            self.series = self.all_series.clone();
        } else {
            let needle = self.filter.value().to_lowercase();
            self.series = self
                .all_series
                .iter()
                .filter(|series| {
                    series
                        .title
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&needle)
                })
                .cloned()
                .collect();
        }
        if self.series_cursor >= self.series.len() {
            self.series_cursor = 0;
        }
    }

    fn selected_series(&self) -> Option<&Series> {
        self.series.get(self.series_cursor)
    }

    fn selected_episode(&self) -> Option<&Episode> {
        self.episodes.get(self.episode_cursor)
    }

    fn selected_release(&self) -> Option<&Release> {
        self.releases.get(self.release_cursor)
    }

    /// The cursor of the current level's list, with the list length; `None`
    /// on the (list-less) episode detail.
    fn cursor_and_len(&mut self) -> Option<(&mut usize, usize)> {
        match &self.level {
            Level::SeriesList => Some((&mut self.series_cursor, self.series.len())),
            Level::Show { series } => {
                let len = series.seasons.len();
                Some((&mut self.season_cursor, len))
            }
            Level::Episodes { .. } => Some((&mut self.episode_cursor, self.episodes.len())),
            Level::EpisodeDetail { .. } => None,
            Level::Search { .. } => Some((&mut self.release_cursor, self.releases.len())),
            Level::Add => Some((&mut self.add_cursor, self.add_results.len())),
        }
    }

    fn open_selected(&mut self) {
        match &self.level {
            Level::SeriesList => {
                if let Some(series) = self.selected_series().cloned() {
                    self.season_cursor = 0;
                    self.overview_scroll = 0;
                    self.level = Level::Show { series };
                    self.fetch_queue();
                }
            }
            Level::Show { series } => {
                if let Some(season) = series.seasons.get(self.season_cursor).cloned() {
                    let series = series.clone();
                    self.episode_cursor = 0;
                    self.episodes.clear();
                    self.fetch_episodes(series.id, season.season_number);
                    self.fetch_queue();
                    self.level = Level::Episodes { series, season };
                }
            }
            Level::Episodes { series, season } => {
                let (series, season) = (series.clone(), season.clone());
                let Some(episode) = self.selected_episode().cloned() else {
                    return;
                };
                if episode.has_file && episode.episode_file.is_some() {
                    self.level = Level::EpisodeDetail {
                        series,
                        season,
                        episode,
                    };
                } else {
                    self.search_menu = Some(0);
                }
            }
            Level::EpisodeDetail { .. } => {}
            Level::Search { .. } => {
                if self.selected_release().is_some() {
                    self.confirm = Some(Confirm::Grab {
                        index: self.release_cursor,
                    });
                }
            }
            Level::Add => self.start_add_flow(),
        }
    }

    /// Pop one level; returns false at the series list so the caller can
    /// give Esc its list-level meaning (clear filter) instead.
    fn pop_level(&mut self) -> bool {
        match std::mem::replace(&mut self.level, Level::SeriesList) {
            Level::SeriesList => false,
            Level::Show { .. } => {
                self.overview_fullscreen = false;
                true
            }
            Level::Episodes { series, .. } => {
                self.level = Level::Show { series };
                true
            }
            Level::EpisodeDetail { series, season, .. } => {
                self.level = Level::Episodes { series, season };
                true
            }
            Level::Search { series, season, .. } => {
                self.releases.clear();
                self.history.clear();
                self.level = Level::Episodes { series, season };
                true
            }
            Level::Add => {
                self.add_results.clear();
                self.add_search.clear();
                self.add_search_focused = false;
                self.add_flow = None;
                true
            }
        }
    }

    fn refresh(&mut self) {
        match &self.level {
            Level::SeriesList => self.fetch_series(),
            Level::Show { .. } => {
                // Season stats live on the series payload; the embedded copy
                // is refreshed by on_series_loaded.
                self.fetch_series();
                self.fetch_queue();
            }
            Level::Episodes { series, season } | Level::EpisodeDetail { series, season, .. } => {
                let (id, number) = (series.id, season.season_number);
                self.fetch_episodes(id, number);
                self.fetch_queue();
            }
            Level::Search { episode, .. } => {
                let id = episode.id;
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
                    client.delete_episode_file(file_id).await
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
        let Level::Episodes { series, season } = &self.level else {
            return;
        };
        let Some(episode) = self.selected_episode().cloned() else {
            return;
        };
        match choice {
            0 => {
                let id = episode.id;
                let client = self.client.clone();
                self.spawn_command(CommandKind::AutoSearch, async move {
                    client.episode_search(id).await
                });
            }
            1 => {
                let (series, season) = (series.clone(), season.clone());
                self.fetch_releases(episode.id);
                self.level = Level::Search {
                    series,
                    season,
                    episode,
                };
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
        self.add_cursor = 0;
        self.add_flow = None;
        self.error = None;
        self.notice = None;
        self.fetch_add_options();
    }

    /// Fetch the root folders and quality profiles the wizard picks from, on
    /// their own generation so a lookup can't invalidate them.
    fn fetch_add_options(&mut self) {
        self.add_prereq_gen = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let prereq_gen = self.add_prereq_gen;
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result: Result<_, crate::sonarr::Error> = async {
                let folders = client.get_root_folders().await?;
                let profiles = client.get_quality_profiles().await?;
                Ok((folders, profiles))
            }
            .await;
            sender.send(Msg::AddOptionsLoaded { prereq_gen, result });
        });
    }

    /// Run a lookup for the add screen; an empty term just clears the list.
    fn fetch_lookup(&mut self, term: String) {
        if term.trim().is_empty() {
            self.add_results.clear();
            self.add_cursor = 0;
            return;
        }
        let fetch_gen = self.begin_fetch();
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.lookup_series(&term).await;
            sender.send(Msg::LookupLoaded { fetch_gen, result });
        });
    }

    /// Open the add wizard for the highlighted result; refuses if the
    /// prerequisites haven't loaded (or none are configured).
    fn start_add_flow(&mut self) {
        if self.add_cursor >= self.add_results.len() {
            return;
        }
        if self.root_folders.is_empty() || self.quality_profiles.is_empty() {
            self.notice =
                Some("add options unavailable: need a root folder and a quality profile".into());
            return;
        }
        self.add_flow = Some(AddFlow {
            result: self.add_cursor,
            step: AddStep::RootFolder,
            cursor: 0,
            root_folder: None,
            quality_profile_id: None,
            monitored: true,
            search: true,
        });
    }

    /// Options for the wizard's current step; the count clamps the cursor and
    /// the labels are what `draw_menu` renders.
    fn add_step_menu(&self) -> (String, Vec<String>) {
        let Some(flow) = &self.add_flow else {
            return (String::new(), Vec::new());
        };
        match flow.step {
            AddStep::RootFolder => (
                "Root folder".to_string(),
                self.root_folders.iter().map(|f| f.path.clone()).collect(),
            ),
            AddStep::QualityProfile => (
                "Quality profile".to_string(),
                self.quality_profiles
                    .iter()
                    .map(|p| p.name.clone())
                    .collect(),
            ),
            AddStep::Monitored => (
                "Monitor this show?".to_string(),
                vec!["Yes".to_string(), "No".to_string()],
            ),
            AddStep::Download => (
                "Search & download now?".to_string(),
                vec!["Yes".to_string(), "No".to_string()],
            ),
        }
    }

    fn add_step_option_count(&self) -> usize {
        let Some(flow) = &self.add_flow else {
            return 0;
        };
        match flow.step {
            AddStep::RootFolder => self.root_folders.len(),
            AddStep::QualityProfile => self.quality_profiles.len(),
            AddStep::Monitored | AddStep::Download => 2,
        }
    }

    /// Record the current step's selection and move to the next; commit once
    /// the last step is answered.
    fn advance_add_flow(&mut self) {
        let Some(flow) = self.add_flow.as_ref() else {
            return;
        };
        let (step, cursor) = (flow.step, flow.cursor);
        match step {
            AddStep::RootFolder => {
                let path = self.root_folders.get(cursor).map(|f| f.path.clone());
                if let Some(flow) = self.add_flow.as_mut() {
                    flow.root_folder = path;
                }
            }
            AddStep::QualityProfile => {
                let id = self.quality_profiles.get(cursor).map(|p| p.id);
                if let Some(flow) = self.add_flow.as_mut() {
                    flow.quality_profile_id = id;
                }
            }
            AddStep::Monitored => {
                if let Some(flow) = self.add_flow.as_mut() {
                    flow.monitored = cursor == 0;
                }
            }
            AddStep::Download => {
                if let Some(flow) = self.add_flow.as_mut() {
                    flow.search = cursor == 0;
                }
            }
        }
        match step.next() {
            Some(next) => {
                if let Some(flow) = self.add_flow.as_mut() {
                    flow.step = next;
                    flow.cursor = 0;
                }
            }
            None => self.commit_add(),
        }
    }

    /// Build the add payload from the finished wizard and POST it.
    fn commit_add(&mut self) {
        let Some(flow) = self.add_flow.take() else {
            return;
        };
        let Some(series) = self.add_results.get(flow.result).cloned() else {
            return;
        };
        let monitor = if flow.monitored { "all" } else { "none" };
        let body = serde_json::json!({
            "tvdbId": series.tvdb_id,
            "title": series.title,
            "qualityProfileId": flow.quality_profile_id,
            "rootFolderPath": flow.root_folder,
            "monitored": flow.monitored,
            "seasonFolder": true,
            "seriesType": "standard",
            "addOptions": {
                "searchForMissingEpisodes": flow.search,
                "monitor": monitor,
            },
        });
        let fetch_gen = self.begin_fetch();
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.add_series(&body).await.map(Box::new);
            sender.send(Msg::SeriesAdded { fetch_gen, result });
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

        // The add wizard is modal: while open it owns every key.
        if self.add_flow.is_some() {
            let count = self.add_step_option_count();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(flow) = self.add_flow.as_mut() {
                        flow.cursor = flow.cursor.saturating_sub(1);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(flow) = self.add_flow.as_mut() {
                        flow.cursor = (flow.cursor + 1).min(count.saturating_sub(1));
                    }
                }
                KeyCode::Enter => self.advance_add_flow(),
                KeyCode::Esc | KeyCode::Char('q') => self.add_flow = None,
                _ => {}
            }
            return None;
        }

        // Popups are modal: while open each owns every key.
        if self.confirm.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    if let Some(confirm) = self.confirm.take() {
                        self.execute_confirm(confirm);
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.confirm = None;
                }
                _ => {}
            }
            return None;
        }
        if let Some(selected) = self.sort_menu {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.sort_menu = Some(selected.saturating_sub(1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.sort_menu = Some((selected + 1).min(SERIES_SORTS.len() - 1));
                }
                KeyCode::Enter => {
                    self.sort_menu = None;
                    let sort = SERIES_SORTS
                        .get(selected)
                        .copied()
                        .unwrap_or(SERIES_SORTS[0]);
                    if sort != self.sort {
                        self.sort = sort;
                        display::sort_series(&mut self.all_series, sort);
                        self.series_cursor = 0;
                        self.apply_filter();
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => self.sort_menu = None,
                _ => {}
            }
            return None;
        }
        if let Some(selected) = self.search_menu {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.search_menu = Some(selected.saturating_sub(1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
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
                KeyCode::Up | KeyCode::Char('k') => {
                    self.rejections_popup = Some(selected.saturating_sub(1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.rejections_popup = Some((selected + 1).min(count.saturating_sub(1)));
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('x') => {
                    self.rejections_popup = None;
                }
                _ => {}
            }
            return None;
        }

        // Full-screen overview scrolls; everything else is closed off.
        if self.overview_fullscreen {
            let page = self.last_height.saturating_sub(4).max(1) as usize;
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.overview_scroll = self.overview_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => self.overview_scroll += 1,
                KeyCode::PageUp | KeyCode::Char('b') | KeyCode::Char('u') => {
                    self.overview_scroll = self.overview_scroll.saturating_sub(page);
                }
                KeyCode::PageDown | KeyCode::Char('f') | KeyCode::Char('d') => {
                    self.overview_scroll += page;
                }
                KeyCode::Home | KeyCode::Char('g') => self.overview_scroll = 0,
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('v') => {
                    self.overview_fullscreen = false;
                    self.overview_scroll = 0;
                }
                KeyCode::Char('q') => return Some(BrowseAction::Quit),
                _ => {}
            }
            return None;
        }

        let page_jump = (self.last_height / 5).max(1) as usize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some((cursor, _)) = self.cursor_and_len() {
                    *cursor = cursor.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some((cursor, len)) = self.cursor_and_len()
                    && *cursor + 1 < len
                {
                    *cursor += 1;
                }
            }
            KeyCode::PageUp | KeyCode::Char('b') | KeyCode::Char('u') => {
                if let Some((cursor, _)) = self.cursor_and_len() {
                    *cursor = cursor.saturating_sub(page_jump);
                }
            }
            KeyCode::PageDown | KeyCode::Char('f') | KeyCode::Char('d') => {
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

            KeyCode::Char('/') if matches!(self.level, Level::SeriesList) => {
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
            KeyCode::Char('a') if matches!(self.level, Level::SeriesList) => self.enter_add(),
            KeyCode::Esc | KeyCode::Backspace => {
                if !self.pop_level() && self.filter_active && key.code == KeyCode::Esc {
                    self.filter.clear();
                    self.filter_active = false;
                    self.apply_filter();
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.open_selected(),

            KeyCode::Char('s') if matches!(self.level, Level::SeriesList) => {
                self.sort_menu = Some(
                    SERIES_SORTS
                        .iter()
                        .position(|&sort| sort == self.sort)
                        .unwrap_or(0),
                );
            }
            KeyCode::Char('v')
                if matches!(self.level, Level::Show { .. } | Level::Episodes { .. }) =>
            {
                self.overview_fullscreen = true;
                self.overview_scroll = 0;
            }
            // `x` deletes on the episode detail and shows rejections in the
            // search results; the levels are disjoint.
            KeyCode::Char('x') => match &self.level {
                Level::EpisodeDetail { episode, .. } => {
                    if let Some(file) = &episode.episode_file {
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
                _ => {}
            },
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('?') => self.show_full_help = !self.show_full_help,
            KeyCode::Char('q') => return Some(BrowseAction::Quit),
            _ => {}
        }
        None
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.last_height = area.height;
        let has_message = self.error.is_some() || self.notice.is_some();
        let show_filter = self.filter_active && matches!(self.level, Level::SeriesList);
        let show_add_input = matches!(self.level, Level::Add);
        let show_input = show_filter || show_add_input;
        let help_height = if self.show_full_help { 8 } else { 1 };

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
        if self.overview_fullscreen {
            self.draw_overview_fullscreen(frame, rows[3]);
        } else {
            match &self.level {
                Level::SeriesList => self.draw_series_list(frame, rows[3]),
                Level::Show { .. } => self.draw_show(frame, rows[3]),
                Level::Episodes { .. } => self.draw_episodes(frame, rows[3]),
                Level::EpisodeDetail { .. } => self.draw_episode_detail(frame, rows[3]),
                Level::Search { .. } => self.draw_search(frame, rows[3]),
                Level::Add => self.draw_add_results(frame, rows[3]),
            }
        }
        self.draw_help(frame, rows[4]);

        if let Some(selected) = self.sort_menu {
            let labels: Vec<&str> = SERIES_SORTS.iter().map(|sort| sort.label()).collect();
            prompt::draw_menu(frame, area, "Sort by", &labels, selected);
        }
        if let Some(selected) = self.search_menu {
            prompt::draw_menu(
                frame,
                area,
                "Episode search",
                &SEARCH_MENU_OPTIONS,
                selected,
            );
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
                Confirm::DeleteFile { .. } => "Delete this episode file?",
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
        if self.add_flow.is_some() {
            let (title, options) = self.add_step_menu();
            let labels: Vec<&str> = options.iter().map(String::as_str).collect();
            let cursor = self.add_flow.as_ref().map(|flow| flow.cursor).unwrap_or(0);
            prompt::draw_menu(frame, area, &title, &labels, cursor);
        }
    }

    fn breadcrumb(&self) -> String {
        let title = |series: &Series| series.title.clone().unwrap_or_default();
        match &self.level {
            Level::SeriesList => "Series".to_string(),
            Level::Show { series } => title(series),
            Level::Episodes { series, season } => format!(
                "{} / {}",
                title(series),
                display::season_name(season.season_number)
            ),
            Level::EpisodeDetail {
                series, episode, ..
            }
            | Level::Search {
                series, episode, ..
            } => {
                let code = display::episode_code(episode.season_number, episode.episode_number);
                let suffix = if matches!(self.level, Level::Search { .. }) {
                    " / search"
                } else {
                    ""
                };
                format!("{} / {code}{suffix}", title(series))
            }
            Level::Add => {
                let term = self.add_search.value();
                if term.is_empty() {
                    "Add series".to_string()
                } else {
                    format!("Add series / {term}")
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

    /// Add-screen results: title, "year • rating • N seasons", one truncated
    /// overview line, and a blank spacer (4 rows each).
    fn draw_add_results(&self, frame: &mut Frame, area: Rect) {
        let buf = frame.buffer_mut();
        if self.add_results.is_empty() {
            let message = if self.loading {
                "  Searching..."
            } else if self.add_search.value().trim().is_empty() {
                "  Type a title or tvdb:<id>, then enter to search."
            } else {
                "  No results."
            };
            Line::styled(message.to_string(), theme::dim()).render(area, buf);
            return;
        }
        let avail_height = area.height as usize;
        let items_per_page = (avail_height / 4).max(1);
        let first = Self::window_start(self.add_cursor, self.add_results.len(), items_per_page);
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
        for (i, series) in self.add_results.iter().enumerate().take(last).skip(first) {
            let selected = i == self.add_cursor;
            let title = truncate(series.title.as_deref().unwrap_or("(untitled)"), text_width);
            let meta = truncate(&display::series_meta(series), text_width);
            let overview = truncate(series.overview.as_deref().unwrap_or_default(), text_width);

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
            Self::draw_scrollbar(buf, area, self.add_cursor, self.add_results.len());
        }
    }

    /// The centered window of a cursor list: which item to draw first.
    fn window_start(cursor: usize, len: usize, per_page: usize) -> usize {
        let mut first = cursor.saturating_sub(per_page / 2);
        if first > len.saturating_sub(per_page) {
            first = len.saturating_sub(per_page);
        }
        first
    }

    fn draw_scrollbar(buf: &mut ratatui::buffer::Buffer, area: Rect, cursor: usize, len: usize) {
        if len < 2 {
            return;
        }
        let height = area.height as usize;
        let x = area.x + area.width - 1;
        let thumb = ((cursor as f64 / (len - 1) as f64) * (height - 1) as f64).round() as usize;
        for i in 0..height {
            let (symbol, style) = if i == thumb {
                ("█", Style::new().fg(theme::accent_color()))
            } else {
                ("│", theme::dim())
            };
            buf[(x, area.y + i as u16)]
                .set_symbol(symbol)
                .set_style(style);
        }
    }

    /// Two-line rows (title + dim detail line) with the cursor gutter and
    /// scrollbar, shared by the series list and the search results.
    fn draw_two_line_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        cursor: usize,
        rows: &[(String, Vec<Span<'static>>)],
        empty_message: &str,
    ) {
        let buf = frame.buffer_mut();
        if rows.is_empty() {
            let message = if self.loading {
                "  Loading..."
            } else {
                empty_message
            };
            Line::styled(message.to_string(), theme::dim()).render(area, buf);
            return;
        }
        let avail_height = area.height as usize;
        let items_per_page = (avail_height / 3).max(1);
        let first = Self::window_start(cursor, rows.len(), items_per_page);
        let last = (first + items_per_page).min(rows.len());

        let mut y = area.y;
        for (i, (title, detail)) in rows.iter().enumerate().take(last).skip(first) {
            let (title_line, detail_line) = if i == cursor {
                let gutter = || Span::styled(" │ ", Style::new().fg(theme::accent_bright()));
                (
                    Line::from(vec![
                        gutter(),
                        Span::styled(
                            title.clone(),
                            Style::new()
                                .fg(theme::accent_bright())
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from([vec![gutter()], detail.clone()].concat()),
                )
            } else {
                (
                    Line::from(Span::styled(
                        format!("   {title}"),
                        Style::new().fg(theme::fg()),
                    )),
                    Line::from([vec![Span::raw("   ")], detail.clone()].concat()),
                )
            };
            let row = |offset: u16| Rect::new(area.x, y + offset, area.width.saturating_sub(1), 1);
            if y + 1 < area.y + area.height {
                title_line.render(row(0), buf);
                detail_line.render(row(1), buf);
            }
            y += 3;
        }
        if rows.len() > items_per_page {
            Self::draw_scrollbar(buf, area, cursor, rows.len());
        }
    }

    fn draw_series_list(&self, frame: &mut Frame, area: Rect) {
        let text_width = area.width.saturating_sub(6) as usize;
        let rows: Vec<(String, Vec<Span<'static>>)> = self
            .series
            .iter()
            .map(|series| {
                let title = truncate(series.title.as_deref().unwrap_or("(untitled)"), text_width);
                let overview = truncate(series.overview.as_deref().unwrap_or_default(), text_width);
                (title, vec![Span::styled(overview, theme::dim())])
            })
            .collect();
        self.draw_two_line_list(frame, area, self.series_cursor, &rows, "  No series.");
    }

    /// Series header — title, "year • rating", wrapped overview capped at
    /// ~40% of the space — shared by the show and episode levels so drilling
    /// into a season keeps the series info on screen. Returns the area left
    /// below it (possibly zero-height) for the level's list.
    fn draw_series_header(&self, frame: &mut Frame, area: Rect, series: &Series) -> Rect {
        let buf = frame.buffer_mut();
        let text_width = area.width.saturating_sub(6) as usize;

        let overview_lines = wrap_text(series.overview.as_deref().unwrap_or_default(), text_width);
        let overview_cap = ((area.height as usize) * 2 / 5).max(1);
        let shown_overview = overview_lines.len().min(overview_cap);
        let overview_truncated = overview_lines.len() > overview_cap;

        let mut y = area.y;
        let mut line = |text: Line, y: &mut u16| {
            if *y < area.y + area.height {
                text.render(Rect::new(area.x, *y, area.width.saturating_sub(1), 1), buf);
            }
            *y += 1;
        };

        line(
            Line::from(Span::styled(
                format!("  {}", series.title.as_deref().unwrap_or("(untitled)")),
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD),
            )),
            &mut y,
        );
        let mut meta = Vec::new();
        if let Some(year) = series.year {
            meta.push(year.to_string());
        }
        if let Some(ratings) = &series.ratings
            && ratings.value > 0.0
        {
            meta.push(format!("{:.1}", ratings.value));
        }
        if !meta.is_empty() {
            line(
                Line::from(Span::styled(
                    format!("  {}", meta.join(" • ")),
                    theme::dim(),
                )),
                &mut y,
            );
        }
        y += 1;
        for text in overview_lines.iter().take(shown_overview) {
            line(
                Line::from(Span::styled(
                    format!("  {text}"),
                    Style::new().fg(theme::fg()),
                )),
                &mut y,
            );
        }
        if overview_truncated {
            line(
                Line::from(Span::styled("  … (v: full overview)", theme::dim())),
                &mut y,
            );
        }
        y += 1;

        let bottom = area.y + area.height;
        Rect::new(area.x, y.min(bottom), area.width, bottom.saturating_sub(y))
    }

    fn draw_show(&self, frame: &mut Frame, area: Rect) {
        let Level::Show { series } = &self.level else {
            return;
        };
        // Season list below the series header.
        let list_area = self.draw_series_header(frame, area, series);
        let buf = frame.buffer_mut();
        if list_area.height == 0 {
            return;
        }
        if series.seasons.is_empty() {
            Line::styled("  No seasons.", theme::dim()).render(list_area, buf);
            return;
        }
        let per_page = (list_area.height as usize).max(1);
        let first = Self::window_start(self.season_cursor, series.seasons.len(), per_page);
        let last = (first + per_page).min(series.seasons.len());
        for (row, (i, season)) in series
            .seasons
            .iter()
            .enumerate()
            .take(last)
            .skip(first)
            .enumerate()
        {
            let downloading =
                display::season_downloading(&self.queue, series.id, season.season_number);
            let label = display::season_label(season);
            let mut spans = if i == self.season_cursor {
                vec![
                    Span::styled(" │ ", Style::new().fg(theme::accent_bright())),
                    Span::styled(
                        label,
                        Style::new()
                            .fg(theme::accent_bright())
                            .add_modifier(Modifier::BOLD),
                    ),
                ]
            } else {
                let style = if season.monitored {
                    Style::new().fg(theme::fg())
                } else {
                    theme::dim()
                };
                vec![Span::styled(format!("   {label}"), style)]
            };
            if downloading {
                spans.push(Span::styled(" ↓", Style::new().fg(theme::accent_bright())));
            }
            Line::from(spans).render(
                Rect::new(
                    list_area.x,
                    list_area.y + row as u16,
                    list_area.width.saturating_sub(1),
                    1,
                ),
                buf,
            );
        }
        if series.seasons.len() > per_page {
            Self::draw_scrollbar(buf, list_area, self.season_cursor, series.seasons.len());
        }
    }

    fn draw_overview_fullscreen(&mut self, frame: &mut Frame, area: Rect) {
        let series = match &self.level {
            Level::Show { series } | Level::Episodes { series, .. } => series,
            _ => return,
        };
        let buf = frame.buffer_mut();
        let text_width = area.width.saturating_sub(4) as usize;
        let lines = wrap_text(series.overview.as_deref().unwrap_or_default(), text_width);
        let height = area.height as usize;
        let max_scroll = lines.len().saturating_sub(height);
        self.overview_scroll = self.overview_scroll.min(max_scroll);
        for (row, text) in lines
            .iter()
            .skip(self.overview_scroll)
            .take(height)
            .enumerate()
        {
            Line::from(Span::styled(
                format!("  {text}"),
                Style::new().fg(theme::fg()),
            ))
            .render(
                Rect::new(area.x, area.y + row as u16, area.width.saturating_sub(1), 1),
                buf,
            );
        }
        if lines.len() > height {
            Self::draw_scrollbar(buf, area, self.overview_scroll, max_scroll + 1);
        }
    }

    fn draw_episodes(&self, frame: &mut Frame, area: Rect) {
        let Level::Episodes { series, .. } = &self.level else {
            return;
        };
        // Episode list below the series header, in the seasons list's place.
        let area = self.draw_series_header(frame, area, series);
        let buf = frame.buffer_mut();
        if area.height == 0 {
            return;
        }
        if self.episodes.is_empty() {
            let message = if self.loading {
                "  Loading..."
            } else {
                "  No episodes."
            };
            Line::styled(message.to_string(), theme::dim()).render(area, buf);
            return;
        }
        let per_page = (area.height as usize).max(1);
        let first = Self::window_start(self.episode_cursor, self.episodes.len(), per_page);
        let last = (first + per_page).min(self.episodes.len());
        let now = display::now_utc_iso();
        let title_width = area.width.saturating_sub(EPISODE_META_WIDTH + 12) as usize;

        for (row, (i, episode)) in self
            .episodes
            .iter()
            .enumerate()
            .take(last)
            .skip(first)
            .enumerate()
        {
            let queue_entry = display::episode_queue_entry(&self.queue, episode.id);
            let status = display::episode_status(episode, queue_entry, &now);
            let unaired = status == EpStatus::Unaired;

            let code = display::episode_code(episode.season_number, episode.episode_number);
            let title = truncate(episode.title.as_deref().unwrap_or(""), title_width.max(4));
            let air_date = episode
                .air_date
                .clone()
                .or_else(|| {
                    episode
                        .air_date_utc
                        .as_deref()
                        .map(|utc| utc.chars().take(10).collect())
                })
                .unwrap_or_else(|| "TBA".to_string());
            let status_span = match &status {
                EpStatus::Downloaded(quality) => {
                    Span::styled(quality.clone(), Style::new().fg(theme::fg()))
                }
                EpStatus::Downloading(percent) => Span::styled(
                    format!("↓ {percent}%"),
                    Style::new().fg(theme::accent_bright()),
                ),
                EpStatus::Missing => Span::styled("missing".to_string(), theme::error()),
                EpStatus::Unaired => Span::styled("unaired".to_string(), theme::dim()),
            };

            let selected = i == self.episode_cursor;
            let base_style = if selected {
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD)
            } else if unaired {
                theme::dim()
            } else {
                Style::new().fg(theme::fg())
            };
            let gutter = if selected {
                Span::styled(" │ ", Style::new().fg(theme::accent_bright()))
            } else {
                Span::raw("   ")
            };

            let row_area = Rect::new(area.x, area.y + row as u16, area.width.saturating_sub(1), 1);
            let [left, right] =
                Layout::horizontal([Constraint::Fill(1), Constraint::Length(EPISODE_META_WIDTH)])
                    .areas(row_area);
            Line::from(vec![
                gutter,
                Span::styled(format!("{code}  "), base_style),
                Span::styled(title, base_style),
            ])
            .render(left, buf);
            Line::from(vec![
                Span::styled(format!("{air_date}  "), theme::dim()),
                status_span,
                Span::raw(" "),
            ])
            .right_aligned()
            .render(right, buf);
        }
        if self.episodes.len() > per_page {
            Self::draw_scrollbar(buf, area, self.episode_cursor, self.episodes.len());
        }
    }

    fn draw_episode_detail(&self, frame: &mut Frame, area: Rect) {
        let Level::EpisodeDetail { episode, .. } = &self.level else {
            return;
        };
        let buf = frame.buffer_mut();
        let Some(file) = &episode.episode_file else {
            return;
        };
        let text_width = area.width.saturating_sub(16) as usize;

        let none = || "-".to_string();
        let mut entries: Vec<(&str, String)> = vec![
            (
                "Episode",
                format!(
                    "{}  {}",
                    display::episode_code(episode.season_number, episode.episode_number),
                    episode.title.as_deref().unwrap_or("")
                ),
            ),
            ("Aired", episode.air_date.clone().unwrap_or_else(none)),
            (
                "Path",
                file.path
                    .as_deref()
                    .map(|path| truncate(path, text_width))
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
            entries.push(("Runtime", info.run_time.clone().unwrap_or_else(none)));
        }

        for (row, (label, value)) in entries.iter().enumerate() {
            if row as u16 >= area.height {
                break;
            }
            Line::from(vec![
                Span::styled(format!("  {label:<11}"), theme::selected()),
                Span::styled(format!(" {value}"), Style::new().fg(theme::fg())),
            ])
            .render(
                Rect::new(area.x, area.y + row as u16, area.width.saturating_sub(1), 1),
                buf,
            );
        }
    }

    fn draw_search(&self, frame: &mut Frame, area: Rect) {
        let text_width = area.width.saturating_sub(6) as usize;
        let rows: Vec<(String, Vec<Span<'static>>)> = self
            .releases
            .iter()
            .map(|release| {
                let title = truncate(release.title.as_deref().unwrap_or("(untitled)"), text_width);
                let mut detail = vec![Span::styled(
                    truncate(
                        &display::release_line2(release),
                        text_width.saturating_sub(4),
                    ),
                    theme::dim(),
                )];
                if release.rejected {
                    detail.push(Span::styled(" !", theme::error()));
                }
                if display::previously_grabbed(&self.history, release) {
                    detail.push(Span::styled(" ✓", Style::new().fg(theme::accent_bright())));
                }
                (title, detail)
            })
            .collect();
        let empty = if self.loading {
            "  Searching indexers... this can take a while."
        } else {
            "  No releases found."
        };
        self.draw_two_line_list(frame, area, self.release_cursor, &rows, empty);
    }

    fn help_entries(&self) -> Vec<(&'static str, &'static str)> {
        if self.filter_focused {
            return vec![("esc", "cancel"), ("enter", "apply")];
        }
        if self.add_search_focused {
            return vec![("esc", "unfocus"), ("enter", "search")];
        }
        if self.add_flow.is_some() {
            return vec![("j/k", "move"), ("enter", "next"), ("esc", "cancel")];
        }
        if self.confirm.is_some() {
            return vec![("y/enter", "confirm"), ("n/esc", "cancel")];
        }
        if self.sort_menu.is_some() || self.search_menu.is_some() {
            return vec![("esc", "close"), ("enter", "select")];
        }
        if self.rejections_popup.is_some() {
            return vec![("esc", "close")];
        }
        if self.overview_fullscreen {
            return vec![("esc/v", "back"), ("j/k", "scroll")];
        }
        let mut entries: Vec<(&'static str, &'static str)> = Vec::new();
        match &self.level {
            Level::SeriesList => {
                entries.push(("enter", "open"));
                entries.push(("a", "add"));
                entries.push(("/", "filter"));
                if self.filter_active {
                    entries.push(("esc", "clear"));
                }
                entries.push(("s", "sort"));
            }
            Level::Show { .. } => {
                entries.push(("esc", "back"));
                entries.push(("enter", "open season"));
                entries.push(("v", "overview"));
            }
            Level::Episodes { .. } => {
                entries.push(("esc", "back"));
                entries.push(("enter", "details/search"));
                entries.push(("v", "overview"));
            }
            Level::EpisodeDetail { .. } => {
                entries.push(("esc", "back"));
                entries.push(("x", "delete file"));
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

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let buf = frame.buffer_mut();
        if !self.show_full_help {
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

        let columns: [&[(&str, &str)]; 3] = [
            &[
                ("↑/k", "up"),
                ("↓/j", "down"),
                ("pgup/b/u", "page up"),
                ("pgdn/f/d", "page down"),
                ("g/home", "go to start"),
                ("G/end", "go to end"),
            ],
            &[
                ("enter", "open/select"),
                ("esc", "back/clear"),
                ("/", "filter series"),
                ("s", "sort series"),
                ("v", "full overview"),
                ("r", "refresh"),
            ],
            &[
                ("x", "delete file / rejections"),
                ("! ✓", "rejected / grabbed before"),
                ("↓", "downloading"),
                ("q", "quit"),
                ("?", "close help"),
            ],
        ];
        let areas = Layout::horizontal([
            Constraint::Length(26),
            Constraint::Length(26),
            Constraint::Length(34),
        ])
        .split(area);
        for (column, column_area) in columns.iter().zip(areas.iter()) {
            for (row, (key, desc)) in column.iter().enumerate() {
                if (row as u16) < column_area.height {
                    let line_area = Rect::new(
                        column_area.x,
                        column_area.y + row as u16,
                        column_area.width,
                        1,
                    );
                    Line::from(vec![
                        Span::styled(format!("  {key:<10}"), Style::new().fg(theme::dim_color())),
                        Span::styled(desc.to_string(), theme::dim()),
                    ])
                    .render(line_area, buf);
                }
            }
        }
    }
}

const SEARCH_MENU_OPTIONS: [&str; 3] = ["Auto search", "Interactive search", "Cancel"];
