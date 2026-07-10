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

use crate::apps::auto_search::{self, Monitor, TickAction};
use crate::apps::delete_prompt::{self, DeletePrompt};
use crate::apps::downloads;
use crate::event::AppSender;
use crate::sonarr::display::{self, EpStatus, SERIES_SORTS, SeriesSort};
use crate::sonarr::models::{EpisodeFile, Season};
use crate::sonarr::{
    Client, Command, Episode, HistoryRecord, QualityProfile, QueueItem, Release, RootFolder, Series,
};
use crate::ui::input::TextInput;
use crate::ui::text::{truncate, wrap_text};
use crate::ui::{help, list, prompt, theme};

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
    /// The download queue as a list the user can mark and remove from.
    Downloads,
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

/// A series' "Title (Year)" label for the auto-search status line.
fn series_label(series: &Series) -> String {
    let title = series.title.as_deref().unwrap_or("series");
    match series.year {
        Some(year) if year > 0 => format!("{title} ({year})"),
        _ => title.to_string(),
    }
}

/// The downloaded-file detail rows (Path/Size/Quality/Language + media-info)
/// shown in the episode-info view. Media rows appear only when `media_info` is
/// present. Free function so it can be reused and tested without a `Browse`.
fn episode_file_rows(file: &EpisodeFile, path_width: usize) -> Vec<(&'static str, String)> {
    let none = || "-".to_string();
    let mut rows: Vec<(&'static str, String)> = vec![
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
    rows.push(("Language", language));
    if let Some(info) = &file.media_info {
        let join = |parts: Vec<Option<String>>| {
            let joined: Vec<String> = parts.into_iter().flatten().collect();
            if joined.is_empty() {
                none()
            } else {
                joined.join(" • ")
            }
        };
        rows.push((
            "Video",
            join(vec![
                info.video_codec.clone(),
                info.resolution.clone(),
                info.video_dynamic_range.clone(),
            ]),
        ));
        rows.push((
            "Audio",
            join(vec![
                info.audio_codec.clone(),
                info.audio_channels.map(|channels| format!("{channels} ch")),
                info.audio_languages.clone(),
            ]),
        ));
        rows.push((
            "Subtitles",
            info.subtitles
                .clone()
                .filter(|subs| !subs.is_empty())
                .unwrap_or_else(none),
        ));
        rows.push(("Runtime", info.run_time.clone().unwrap_or_else(none)));
    }
    rows
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
    /// Download queue, refreshed by the poll; drives every ↓ marker and the
    /// Downloads view.
    queue: Vec<QueueItem>,
    /// Selection + prompt state for the Downloads level.
    downloads: downloads::Downloads,
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
    /// The delete-series prompt (files / import-list-exclusion toggles); modal
    /// while set, opened with `z` on the show page.
    delete_prompt: Option<DeletePrompt>,
    /// Cursor into the selected release's rejection list; `None` when closed.
    rejections_popup: Option<usize>,

    // The add-new flow (Level::Add): a persistent search box, its lookup
    // results, the wizard prerequisites, and the wizard itself when open.
    add_search: TextInput,
    add_search_focused: bool,
    add_results: Vec<Series>,
    /// The raw lookup JSON behind `add_results`, index-aligned; the add
    /// forwards the whole object (POST /series needs titleSlug/images/seasons
    /// the typed model drops).
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
    add_flow: Option<AddFlow>,
    /// The just-committed add asked for an auto-search; read once its
    /// `SeriesAdded` lands, to start a background monitor. Only one add is ever
    /// in flight (`add_submitting` guards a second submit), so one flag suffices.
    pending_search: bool,
    /// Background auto-search monitors, one per add-with-search still resolving.
    /// Each runs on its own generation, independent of the view, so it survives
    /// navigation; see [`crate::apps::auto_search`].
    monitors: Vec<Monitor>,

    /// Scroll offset into the episode-info view (`Level::EpisodeDetail`).
    info_scroll: usize,

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
            downloads: downloads::Downloads::default(),
            history: Vec::new(),
            filter: TextInput::default(),
            filter_active: false,
            filter_focused: false,
            sort: SERIES_SORTS[0],
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
            add_flow: None,
            pending_search: false,
            monitors: Vec::new(),
            info_scroll: 0,
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
        self.tick_monitors();
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
        // A list refresh landing while the user is on the add screen must not
        // touch its loading/error/list state (the lookup owns those); still
        // honour a 401 so a revoked key is caught immediately.
        if matches!(self.level, Level::Add) {
            return matches!(result, Err(crate::sonarr::Error::Unauthorized));
        }
        self.loading = false;
        match result {
            Ok(mut list) => {
                display::sort_series(&mut list, self.sort);
                // A reload with a show open (refresh from the show level)
                // must also refresh the embedded copy the header and season
                // list render from.
                let embedded = match &mut self.level {
                    Level::SeriesList | Level::Add | Level::Downloads => None,
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
            Ok(queue) => {
                self.queue = queue;
                self.downloads.prune(&self.queue);
            }
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
            CommandKind::DeleteSeries(deleted_id) => {
                // The series is gone. Only leave its pages if the user is
                // still on one: a delete landing after they navigated to
                // another series (the generation counter doesn't bump on
                // navigation) must not yank them away. The refetch drops it
                // from the list regardless.
                if matches!(&self.level,
                    Level::Show { series }
                    | Level::Episodes { series, .. }
                    | Level::EpisodeDetail { series, .. }
                    | Level::Search { series, .. }
                        if series.id == deleted_id)
                {
                    self.level = Level::SeriesList;
                }
                self.fetch_series();
                self.fetch_queue();
                self.notice = Some("series deleted".into());
            }
            CommandKind::DeleteQueue => {
                self.downloads.clear_marks();
                self.fetch_queue();
                self.notice = Some("removed from queue".into());
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
        result: Result<Vec<serde_json::Value>, crate::sonarr::Error>,
    ) -> bool {
        if add_lookup_gen != self.add_lookup_gen {
            return false; // stale response from a superseded/abandoned lookup
        }
        self.loading = false;
        match result {
            Ok(values) => {
                self.add_results.clear();
                self.add_results_raw.clear();
                // Series deserializes any object (all fields default), so
                // typed and raw stay aligned.
                for value in values {
                    if let Ok(series) = serde_json::from_value::<Series>(value.clone()) {
                        self.add_results.push(series);
                        self.add_results_raw.push(value);
                    }
                }
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

    /// The add finished. Checked against `add_gen`, which is bumped when the
    /// user leaves the add screen, so a committed add that lands after they
    /// moved on is dropped rather than yanking them to the new show. On
    /// success we jump to the created series' show page and refetch the
    /// library so it is present when the user pops back.
    #[must_use]
    pub fn on_series_added(
        &mut self,
        add_gen: u64,
        result: Result<Box<Series>, crate::sonarr::Error>,
    ) -> bool {
        if add_gen != self.add_gen {
            return false; // superseded: the user left the add screen meanwhile
        }
        self.add_submitting = false;
        if matches!(result, Err(crate::sonarr::Error::Unauthorized)) {
            return true;
        }
        self.loading = false;
        let pending_search = std::mem::take(&mut self.pending_search);
        match result {
            Ok(series) => {
                // Kick off the tracked background search before the series is
                // moved into the show level.
                if pending_search {
                    self.start_auto_search(series.id, series_label(&series));
                }
                self.season_cursor = 0;
                self.info_scroll = 0;
                self.level = Level::Show { series: *series };
                self.add_results.clear();
                self.add_results_raw.clear();
                self.add_search.clear();
                self.add_search_focused = false;
                self.fetch_series();
                self.fetch_queue();
                // Set AFTER the refetch, which clears the notice row.
                self.notice = Some("series added".into());
            }
            // The add failed (e.g. already in the library); stay on the
            // results so the user can pick something else.
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Start tracking a background auto-search for a just-added series: fire the
    /// tracked `SeriesSearch` command and record a monitor on its own
    /// generation. The baseline snapshot lets the outcome check tell a fresh
    /// grab from a download that was already queued.
    fn start_auto_search(&mut self, series_id: i64, title: String) {
        let generation = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let baseline = self
            .queue
            .iter()
            .filter(|item| item.series_id == Some(series_id))
            .map(|item| item.id)
            .collect();
        self.monitors
            .push(Monitor::new(generation, series_id, title, baseline));
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.series_search(series_id).await;
            sender.send(Msg::AutoSearchStarted {
                auto_search_gen: generation,
                result,
            });
        });
    }

    /// Queue items for `series_id` that were not present when its search started.
    fn queue_new_items(&self, monitor: &Monitor) -> usize {
        self.queue
            .iter()
            .filter(|item| {
                item.series_id == Some(monitor.target_id) && !monitor.is_baseline(item.id)
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
        result: Result<Command, crate::sonarr::Error>,
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
            Err(crate::sonarr::Error::Unauthorized) => {
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
        result: Result<Command, crate::sonarr::Error>,
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
            Err(crate::sonarr::Error::Unauthorized) => {
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
        auto_search::status_line("Sonarr", &self.monitors)
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
            Level::Downloads => Some((&mut self.downloads.cursor, self.queue.len())),
        }
    }

    fn open_selected(&mut self) {
        match &self.level {
            Level::SeriesList => {
                if let Some(series) = self.selected_series().cloned() {
                    self.season_cursor = 0;
                    self.info_scroll = 0;
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
            Level::Episodes { .. } => {
                // Enter always searches (Auto/Interactive) for any episode,
                // including a downloaded one, so upgrades are reachable. The
                // episode info/file details live under `i` instead. The popup
                // reads the level + selected episode, so stay on Episodes.
                if self.selected_episode().is_some() {
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
            // Nothing to open; Space is intercepted earlier to toggle a mark.
            Level::Downloads => {}
        }
    }

    /// Pop one level; returns false at the series list so the caller can
    /// give Esc its list-level meaning (clear filter) instead.
    fn pop_level(&mut self) -> bool {
        match std::mem::replace(&mut self.level, Level::SeriesList) {
            Level::SeriesList => false,
            Level::Downloads => {
                self.downloads.reset();
                true
            }
            Level::Show { .. } => true,
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
                self.add_results_raw.clear();
                self.add_search.clear();
                self.add_search_focused = false;
                self.add_flow = None;
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
        self.add_results_raw.clear();
        self.add_cursor = 0;
        self.add_flow = None;
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
            let result = client.lookup_series(&term).await;
            sender.send(Msg::LookupLoaded {
                add_lookup_gen,
                result,
            });
        });
    }

    /// Open the add wizard for the highlighted result; refuses if the
    /// prerequisites haven't loaded (or none are configured).
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

    /// Title and option labels for the wizard's current step; the labels
    /// borrow from the prerequisite lists (rebuilt cheaply per draw, no
    /// allocation) and their count clamps the cursor.
    fn add_step_menu(&self) -> (&'static str, Vec<&str>) {
        let Some(flow) = &self.add_flow else {
            return ("", Vec::new());
        };
        match flow.step {
            AddStep::RootFolder => (
                "Root folder",
                self.root_folders.iter().map(|f| f.path.as_str()).collect(),
            ),
            AddStep::QualityProfile => (
                "Quality profile",
                self.quality_profiles
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect(),
            ),
            AddStep::Monitored => ("Monitor this show?", vec!["Yes", "No"]),
            AddStep::Download => ("Search & download now?", vec!["Yes", "No"]),
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

    /// Forward the finished wizard's lookup object, augmented with the user's
    /// choices, to POST /series. The whole object is forwarded (not a subset)
    /// because Sonarr requires fields the typed model drops (titleSlug,
    /// images, the seasons array).
    fn commit_add(&mut self) {
        let Some(flow) = self.add_flow.take() else {
            return;
        };
        // Both required choices must have resolved; if a prerequisite list
        // was empty under the cursor the payload would carry nulls.
        let (Some(root_folder), Some(quality_profile_id)) =
            (flow.root_folder.as_ref(), flow.quality_profile_id)
        else {
            self.notice = Some("add cancelled: no root folder or quality profile".into());
            return;
        };
        let Some(mut body) = self.add_results_raw.get(flow.result).cloned() else {
            return;
        };
        let monitor = if flow.monitored { "all" } else { "none" };
        if let Some(object) = body.as_object_mut() {
            object.insert(
                "qualityProfileId".into(),
                serde_json::json!(quality_profile_id),
            );
            object.insert("rootFolderPath".into(), serde_json::json!(root_folder));
            object.insert("monitored".into(), serde_json::json!(flow.monitored));
            object.insert("seasonFolder".into(), serde_json::json!(true));
            object.insert("seriesType".into(), serde_json::json!("standard"));
            // The search is run and tracked separately (see `start_auto_search`),
            // so the server must not fire its own untracked one — otherwise the
            // series would be searched twice.
            object.insert(
                "addOptions".into(),
                serde_json::json!({
                    "searchForMissingEpisodes": false,
                    "monitor": monitor,
                }),
            );
        }
        self.pending_search = flow.search;
        self.add_submitting = true;
        self.loading = true;
        self.error = None;
        self.notice = Some("adding...".into());
        self.add_gen = self.gen_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let add_gen = self.add_gen;
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = client.add_series(&body).await.map(Box::new);
            sender.send(Msg::SeriesAdded { add_gen, result });
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
                KeyCode::Up => {
                    if let Some(flow) = self.add_flow.as_mut() {
                        flow.cursor = flow.cursor.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
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

        // The delete-series prompt is modal. Take it so the handler owns it
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
                    self.spawn_command(CommandKind::DeleteSeries(id), async move {
                        client.delete_series(id, delete_files, add_exclusion).await
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

        // The episode-info view keeps the series header pinned and scrolls its
        // body; it owns scroll, delete, and close keys while open.
        if matches!(self.level, Level::EpisodeDetail { .. }) {
            let page = self.last_height.saturating_sub(4).max(1) as usize;
            match key.code {
                KeyCode::Up => self.info_scroll = self.info_scroll.saturating_sub(1),
                KeyCode::Down => self.info_scroll += 1,
                KeyCode::PageUp | KeyCode::Left => {
                    self.info_scroll = self.info_scroll.saturating_sub(page);
                }
                KeyCode::PageDown | KeyCode::Right => self.info_scroll += page,
                KeyCode::Home | KeyCode::Char('g') => self.info_scroll = 0,
                KeyCode::End | KeyCode::Char('G') => self.info_scroll = usize::MAX,
                KeyCode::Char('x') => {
                    if let Level::EpisodeDetail { episode, .. } = &self.level
                        && let Some(file) = &episode.episode_file
                    {
                        self.confirm = Some(Confirm::DeleteFile { file_id: file.id });
                    }
                }
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('i') => {
                    self.pop_level();
                    self.info_scroll = 0;
                }
                KeyCode::Char('r') => self.refresh(),
                KeyCode::Char('?') => self.show_full_help = !self.show_full_help,
                KeyCode::Char('q') => return Some(BrowseAction::Quit),
                _ => {}
            }
            return None;
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
            // `d` opens Downloads on the series list only; it is no longer a
            // page-down alias, so it is unbound on every other level.
            KeyCode::Char('d') if matches!(self.level, Level::SeriesList) => {
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
            // On the Downloads level Space marks the highlighted row instead
            // of opening anything.
            KeyCode::Char(' ') if matches!(self.level, Level::Downloads) => {
                self.downloads.toggle_mark(&self.queue);
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
            // Open the selected episode's info (description + file details),
            // keeping the series header pinned. Clone out before reassigning
            // `self.level` (the same borrow dance as `open_selected`).
            KeyCode::Char('i') if matches!(self.level, Level::Episodes { .. }) => {
                if let Some(episode) = self.selected_episode().cloned()
                    && let Level::Episodes { series, season } = &self.level
                {
                    let series = series.clone();
                    let season = season.clone();
                    self.info_scroll = 0;
                    self.level = Level::EpisodeDetail {
                        series,
                        season,
                        episode,
                    };
                }
            }
            // `x` shows rejections in the search results and removes from the
            // queue; deleting an episode file lives in the episode-info view.
            KeyCode::Char('x') => match &self.level {
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
                if let Level::Show { series } = &self.level {
                    self.delete_prompt = Some(DeletePrompt::new(
                        series.id,
                        series
                            .title
                            .clone()
                            .unwrap_or_else(|| "this series".to_string()),
                    ));
                }
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('?') => self.show_full_help = !self.show_full_help,
            KeyCode::Char('q') => return Some(BrowseAction::Quit),
            _ => {}
        }
        None
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.last_height = area.height;
        // The background poll can shrink the queue under a stale cursor.
        self.downloads.clamp(self.queue.len());
        let has_message = self.error.is_some() || self.notice.is_some();
        let show_filter = self.filter_active && matches!(self.level, Level::SeriesList);
        let show_add_input = matches!(self.level, Level::Add);
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
        match &self.level {
            Level::SeriesList => self.draw_series_list(frame, rows[3]),
            Level::Show { .. } => self.draw_show(frame, rows[3]),
            Level::Episodes { .. } => self.draw_episodes(frame, rows[3]),
            Level::EpisodeDetail { .. } => self.draw_episode_info(frame, rows[3]),
            Level::Search { .. } => self.draw_search(frame, rows[3]),
            Level::Add => self.draw_add_results(frame, rows[3]),
            Level::Downloads => self.draw_downloads(frame, rows[3]),
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
        if let Some(prompt) = &self.delete_prompt {
            prompt.draw(frame, area);
        }
        if self.add_flow.is_some() {
            let cursor = self.add_flow.as_ref().map(|flow| flow.cursor).unwrap_or(0);
            let (title, labels) = self.add_step_menu();
            prompt::draw_menu(frame, area, title, &labels, cursor);
        }
        self.downloads.draw_prompt(frame, area);
    }

    fn breadcrumb(&self) -> String {
        let title = |series: &Series| series.title.clone().unwrap_or_default();
        match &self.level {
            Level::SeriesList => "Series".to_string(),
            Level::Downloads => {
                let marked = self.downloads.marked();
                if marked > 0 {
                    format!("Downloads ({marked} marked)")
                } else {
                    format!("Downloads ({})", self.queue.len())
                }
            }
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
            list::draw_scrollbar(buf, area, self.add_cursor, self.add_results.len());
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
        let first = list::window_start(cursor, rows.len(), items_per_page);
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
            list::draw_scrollbar(buf, area, cursor, rows.len());
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

    /// Series header — title, "year • rating", and the full series overview
    /// (clamped to ~600 chars, like the Jellyfin app) — shared by the show,
    /// episodes, and episode-info levels so the series stays on screen. Always
    /// keeps a few rows for the list below and returns that leftover area.
    fn draw_series_header(&self, frame: &mut Frame, area: Rect, series: &Series) -> Rect {
        const OVERVIEW_MAX_CHARS: usize = 600;
        const MIN_LIST_ROWS: u16 = 4;

        let buf = frame.buffer_mut();
        let text_width = area.width.saturating_sub(6) as usize;

        let overview_raw = series.overview.as_deref().unwrap_or_default();
        let overview = if overview_raw.chars().count() > OVERVIEW_MAX_CHARS {
            let clipped: String = overview_raw.chars().take(OVERVIEW_MAX_CHARS).collect();
            format!("{}…", clipped.trim_end())
        } else {
            overview_raw.to_string()
        };
        let overview_lines = wrap_text(&overview, text_width);

        let mut meta = Vec::new();
        if let Some(year) = series.year {
            meta.push(year.to_string());
        }
        if let Some(ratings) = &series.ratings
            && ratings.value > 0.0
        {
            meta.push(format!("{:.1}", ratings.value));
        }

        // Reserve a few rows for the list: title + optional meta + two spacers.
        let fixed = 1 + (!meta.is_empty()) as u16 + 2;
        let room = area
            .height
            .saturating_sub(MIN_LIST_ROWS)
            .saturating_sub(fixed) as usize;
        let shown_overview = overview_lines.len().min(room.max(1));
        let overview_truncated = overview_lines.len() > shown_overview;

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
            line(Line::from(Span::styled("  …", theme::dim())), &mut y);
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
        let first = list::window_start(self.season_cursor, series.seasons.len(), per_page);
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
                spans.push(Span::styled(
                    format!(" {}", display::GLYPH_DOWNLOADING),
                    Style::new().fg(theme::accent_bright()),
                ));
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
            list::draw_scrollbar(buf, list_area, self.season_cursor, series.seasons.len());
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
        let first = list::window_start(self.episode_cursor, self.episodes.len(), per_page);
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
                    format!("{} {percent}%", display::GLYPH_DOWNLOADING),
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
            list::draw_scrollbar(buf, area, self.episode_cursor, self.episodes.len());
        }
    }

    /// The download queue as a markable list. The shared renderer handles
    /// layout and selection; this resolves the series name from the cache
    /// (falling back to the release title) and the Sonarr-only SxxExx code and
    /// episode title from the embedded queue episode.
    fn draw_downloads(&self, frame: &mut Frame, area: Rect) {
        self.downloads.draw_list(frame, area, &self.queue, |item| {
            let name = item
                .series_id
                .and_then(|id| self.all_series.iter().find(|series| series.id == id))
                .and_then(|series| series.title.clone())
                .or_else(|| item.title.clone())
                .unwrap_or_else(|| "(unknown)".to_string());
            downloads::DownloadRow {
                name,
                code: item
                    .episode
                    .as_ref()
                    .map(|ep| display::episode_code(ep.season_number, ep.episode_number)),
                episode_title: item
                    .episode
                    .as_ref()
                    .and_then(|ep| ep.title.clone())
                    .filter(|title| !title.is_empty()),
            }
        });
    }

    /// The selected episode's info: the pinned series header, then the
    /// episode's code/title, meta, its own description, and (if downloaded) the
    /// file details. Scrolls with `info_scroll`.
    fn draw_episode_info(&mut self, frame: &mut Frame, area: Rect) {
        // Draw the pinned header and build the scrollable body under an
        // immutable borrow, then clamp/scroll under a mutable one.
        let (body, lines) = {
            let Level::EpisodeDetail {
                series, episode, ..
            } = &self.level
            else {
                return;
            };
            let body = self.draw_series_header(frame, area, series);
            let text_width = body.width.saturating_sub(4) as usize;

            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}  {}",
                    display::episode_code(episode.season_number, episode.episode_number),
                    episode.title.as_deref().unwrap_or("")
                ),
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD),
            )));
            let mut meta: Vec<String> = Vec::new();
            if let Some(aired) = &episode.air_date {
                meta.push(format!("Aired {aired}"));
            }
            if let Some(runtime) = episode.runtime {
                meta.push(format!("{runtime}m"));
            }
            if !meta.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", meta.join(" · ")),
                    theme::dim(),
                )));
            }
            // File details first (or "not downloaded"), then the description.
            if let Some(file) = &episode.episode_file {
                lines.push(Line::from(""));
                for (label, value) in
                    episode_file_rows(file, body.width.saturating_sub(16) as usize)
                {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {label:<11}"), theme::selected()),
                        Span::styled(format!(" {value}"), Style::new().fg(theme::fg())),
                    ]));
                }
            } else {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("  Not downloaded.", theme::dim())));
            }
            lines.push(Line::from(""));
            let overview = episode
                .overview
                .as_deref()
                .unwrap_or("No overview available.");
            for text in wrap_text(overview, text_width) {
                lines.push(Line::from(Span::styled(
                    format!("  {text}"),
                    Style::new().fg(theme::fg()),
                )));
            }
            (body, lines)
        };

        let buf = frame.buffer_mut();
        let total = lines.len();
        let height = body.height as usize;
        let max_scroll = total.saturating_sub(height);
        self.info_scroll = self.info_scroll.min(max_scroll);
        for (row, line) in lines
            .into_iter()
            .skip(self.info_scroll)
            .take(height)
            .enumerate()
        {
            line.render(
                Rect::new(body.x, body.y + row as u16, body.width.saturating_sub(1), 1),
                buf,
            );
        }
        if total > height {
            list::draw_scrollbar(buf, body, self.info_scroll, max_scroll + 1);
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
            return vec![("↑/↓", "move"), ("enter", "next"), ("esc", "cancel")];
        }
        if self.confirm.is_some() {
            return vec![("y/enter", "confirm"), ("n/esc", "cancel")];
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
            Level::SeriesList => {
                entries.push(("enter", "open"));
                entries.push(("a", "add"));
                entries.push(("d", "downloads"));
                entries.push(("/", "filter"));
                if self.filter_active {
                    entries.push(("esc", "clear"));
                }
                entries.push(("s", "sort"));
            }
            Level::Downloads => {
                entries.push(("esc", "back"));
                entries.push(("space", "mark"));
                entries.push(("x", "remove"));
            }
            Level::Show { .. } => {
                entries.push(("esc", "back"));
                entries.push(("enter", "open season"));
                entries.push(("z", "delete"));
            }
            Level::Episodes { .. } => {
                entries.push(("esc", "back"));
                entries.push(("enter", "search"));
                entries.push(("i", "info"));
            }
            Level::EpisodeDetail { episode, .. } => {
                entries.push(("↑/↓", "scroll"));
                if episode.episode_file.is_some() {
                    entries.push(("x", "delete file"));
                }
                entries.push(("esc/i", "back"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arr::models::Quality;
    use crate::sonarr::models::{Language, MediaInfo, QualityWrapper};

    #[test]
    fn episode_file_rows_include_media_info() {
        let file = EpisodeFile {
            id: 1,
            path: Some("/tv/Show/S01E02.mkv".into()),
            size: 1_500_000_000,
            quality: Some(QualityWrapper {
                quality: Quality {
                    name: Some("Bluray-1080p".into()),
                },
            }),
            languages: vec![Language {
                name: Some("Japanese".into()),
            }],
            language: None,
            media_info: Some(MediaInfo {
                video_codec: Some("x264".into()),
                resolution: Some("1920x1080".into()),
                audio_codec: Some("FLAC".into()),
                audio_channels: Some(2.0),
                audio_languages: Some("jpn".into()),
                subtitles: Some("eng".into()),
                run_time: Some("23:41".into()),
                ..Default::default()
            }),
        };
        let rows = episode_file_rows(&file, 40);
        let labels: Vec<&str> = rows.iter().map(|(label, _)| *label).collect();
        assert_eq!(
            labels,
            [
                "Path",
                "Size",
                "Quality",
                "Language",
                "Video",
                "Audio",
                "Subtitles",
                "Runtime"
            ]
        );
        let value = |label: &str| {
            rows.iter()
                .find(|(l, _)| *l == label)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert_eq!(value("Quality"), "Bluray-1080p");
        assert_eq!(value("Language"), "Japanese");
        assert_eq!(value("Video"), "x264 • 1920x1080");
        assert_eq!(value("Audio"), "FLAC • 2 ch • jpn");

        // Without media_info only the four base rows appear.
        let mut bare = file;
        bare.media_info = None;
        assert_eq!(episode_file_rows(&bare, 40).len(), 4);
    }
}
