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

use crate::event::AppSender;
use crate::radarr::display::{self, MOVIE_SORTS, MovieSort, MovieStatus};
use crate::radarr::{Client, HistoryRecord, Movie, QueueItem, Release};
use crate::ui::input::TextInput;
use crate::ui::text::{truncate, wrap_text};
use crate::ui::{prompt, theme};

use super::msg::{CommandKind, Msg};

const SPINNER_FRAMES: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

/// Queue polls fire every this many 250ms ticks (~10s). Unlike the Sonarr
/// tab, the movie list itself shows download state, so polling runs on
/// every level.
const QUEUE_POLL_TICKS: u32 = 40;

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
}

/// A pending yes/no decision; modal while set.
enum Confirm {
    DeleteFile { file_id: i64 },
    Grab { index: usize },
}

/// What the browse screen wants its parent (the Radarr app) to do.
pub enum BrowseAction {
    Quit,
}

pub struct Browse {
    pub client: Client,
    sender: AppSender,

    level: Level,

    all_movies: Vec<Movie>,
    /// Currently visible movies (filter applied).
    movies: Vec<Movie>,
    movie_cursor: usize,
    releases: Vec<Release>,
    release_cursor: usize,
    /// Download queue, refreshed by the poll; drives every ↓ marker.
    queue: Vec<QueueItem>,
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
    /// Cursor into the selected release's rejection list; `None` when closed.
    rejections_popup: Option<usize>,

    /// Full-screen scrollable overview on the detail level.
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
            level: Level::MovieList,
            all_movies: Vec::new(),
            movies: Vec::new(),
            movie_cursor: 0,
            releases: Vec::new(),
            release_cursor: 0,
            queue: Vec::new(),
            history: Vec::new(),
            filter: TextInput::default(),
            filter_active: false,
            filter_focused: false,
            sort: MOVIE_SORTS[0],
            sort_menu: None,
            search_menu: None,
            confirm: None,
            rejections_popup: None,
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
        // Every level shows download state, so the poll never pauses.
        if self.tick_count.is_multiple_of(QUEUE_POLL_TICKS) {
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
        self.loading = false;
        match result {
            Ok(mut list) => {
                display::sort_movies(&mut list, self.sort);
                // A reload with a detail open (refresh, post-delete, post-
                // grab) must also refresh the embedded copy the detail
                // renders from.
                let embedded = match &mut self.level {
                    Level::MovieList => None,
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
            Ok(queue) => self.queue = queue,
            Err(crate::radarr::Error::Unauthorized) => return true,
            Err(err) => tracing::debug!(%err, "queue poll failed"),
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

    /// Same contract as `on_movies_loaded`.
    #[must_use]
    pub fn on_command_done(
        &mut self,
        fetch_gen: u64,
        kind: CommandKind,
        result: Result<(), crate::radarr::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // superseded: the user moved on meanwhile
        }
        if matches!(result, Err(crate::radarr::Error::Unauthorized)) {
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
        }
        false
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
        self.level = Level::MovieDetail { movie };
    }

    fn apply_filter(&mut self) {
        if !self.filter_active || self.filter.value().is_empty() {
            self.movies = self.all_movies.clone();
        } else {
            let needle = self.filter.value().to_lowercase();
            self.movies = self
                .all_movies
                .iter()
                .filter(|movie| {
                    movie
                        .title
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&needle)
                })
                .cloned()
                .collect();
        }
        if self.movie_cursor >= self.movies.len() {
            self.movie_cursor = 0;
        }
    }

    fn selected_movie(&self) -> Option<&Movie> {
        self.movies.get(self.movie_cursor)
    }

    fn selected_release(&self) -> Option<&Release> {
        self.releases.get(self.release_cursor)
    }

    /// The cursor of the current level's list, with the list length; `None`
    /// on the (list-less) movie detail.
    fn cursor_and_len(&mut self) -> Option<(&mut usize, usize)> {
        match &self.level {
            Level::MovieList => Some((&mut self.movie_cursor, self.movies.len())),
            Level::MovieDetail { .. } => None,
            Level::Search { .. } => Some((&mut self.release_cursor, self.releases.len())),
        }
    }

    fn open_selected(&mut self) {
        match &self.level {
            Level::MovieList => {
                if let Some(movie) = self.selected_movie().cloned() {
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
        }
    }

    /// Pop one level; returns false at the movie list so the caller can
    /// give Esc its list-level meaning (clear filter) instead.
    fn pop_level(&mut self) -> bool {
        match std::mem::replace(&mut self.level, Level::MovieList) {
            Level::MovieList => false,
            Level::MovieDetail { .. } => {
                self.overview_fullscreen = false;
                true
            }
            Level::Search { movie } => {
                self.releases.clear();
                self.history.clear();
                self.level = Level::MovieDetail { movie };
                true
            }
        }
    }

    fn refresh(&mut self) {
        match &self.level {
            Level::MovieList | Level::MovieDetail { .. } => {
                self.fetch_movies();
                self.fetch_queue();
            }
            Level::Search { movie } => {
                let id = movie.id;
                self.fetch_releases(id);
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
                    client.movie_search(id).await
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

        // The shell only claims ctrl+c/arrows/digits; don't let other
        // ctrl/alt-modified chars fall through to single-letter shortcuts.
        if crate::app::modified_char(&key) {
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

            KeyCode::Char('/') if matches!(self.level, Level::MovieList) => {
                self.filter_active = true;
                self.filter_focused = true;
                self.filter.focused = true;
            }
            KeyCode::Esc | KeyCode::Backspace => {
                if !self.pop_level() && self.filter_active && key.code == KeyCode::Esc {
                    self.filter.clear();
                    self.filter_active = false;
                    self.apply_filter();
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.open_selected(),

            KeyCode::Char('s') if matches!(self.level, Level::MovieList) => {
                self.sort_menu = Some(
                    MOVIE_SORTS
                        .iter()
                        .position(|&sort| sort == self.sort)
                        .unwrap_or(0),
                );
            }
            KeyCode::Char('v') if matches!(self.level, Level::MovieDetail { .. }) => {
                self.overview_fullscreen = true;
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
        let show_filter = self.filter_active && matches!(self.level, Level::MovieList);
        let help_height = if self.show_full_help { 8 } else { 1 };

        let rows = Layout::vertical([
            Constraint::Length(if has_message { 1 } else { 0 }),
            Constraint::Length(2), // breadcrumb row + spacer
            Constraint::Length(if show_filter { 2 } else { 0 }),
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
        }
        if self.overview_fullscreen {
            self.draw_overview_fullscreen(frame, rows[3]);
        } else {
            match &self.level {
                Level::MovieList => self.draw_movie_list(frame, rows[3]),
                Level::MovieDetail { .. } => self.draw_movie_detail(frame, rows[3]),
                Level::Search { .. } => self.draw_search(frame, rows[3]),
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
    }

    fn breadcrumb(&self) -> String {
        let title = |movie: &Movie| movie.title.clone().unwrap_or_default();
        match &self.level {
            Level::MovieList => "Movies".to_string(),
            Level::MovieDetail { movie } => title(movie),
            Level::Search { movie } => format!("{} / search", title(movie)),
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

    fn status_span(status: &MovieStatus) -> Span<'static> {
        match status {
            MovieStatus::Downloaded(quality) => {
                Span::styled(quality.clone(), Style::new().fg(theme::fg()))
            }
            MovieStatus::Downloading(percent) => Span::styled(
                format!("↓ {percent}%"),
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
        if self.movies.is_empty() {
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
        let first = Self::window_start(self.movie_cursor, self.movies.len(), items_per_page);
        let last = (first + items_per_page).min(self.movies.len());
        let now = display::now_utc_iso();
        let text_width = area.width.saturating_sub(6) as usize;
        let title_width = area.width.saturating_sub(MOVIE_META_WIDTH + 6).max(4) as usize;

        let mut y = area.y;
        for (i, movie) in self.movies.iter().enumerate().take(last).skip(first) {
            let queue_entry = display::movie_queue_entry(&self.queue, movie.id);
            let status = display::movie_status(movie, queue_entry, &now);
            let unreleased = matches!(status, MovieStatus::Unreleased(_));
            let selected = i == self.movie_cursor;

            let title = truncate(movie.title.as_deref().unwrap_or("(untitled)"), title_width);
            let meta = truncate(&display::movie_meta(movie), text_width);

            let title_style = if selected {
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD)
            } else if unreleased {
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
                Line::from(vec![gutter(selected), Span::styled(meta, meta_style)]).render(
                    Rect::new(area.x, y + 1, area.width.saturating_sub(1), 1),
                    buf,
                );
            }
            y += 3;
        }
        if self.movies.len() > items_per_page {
            Self::draw_scrollbar(buf, area, self.movie_cursor, self.movies.len());
        }
    }

    /// Movie header — title, "year • rating • runtime", wrapped overview
    /// capped at ~40% of the space. Returns the area left below it for the
    /// file block / status line.
    fn draw_movie_header(&self, frame: &mut Frame, area: Rect, movie: &Movie) -> Rect {
        let buf = frame.buffer_mut();
        let text_width = area.width.saturating_sub(6) as usize;

        let overview_lines = wrap_text(movie.overview.as_deref().unwrap_or_default(), text_width);
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
                format!("  {}", movie.title.as_deref().unwrap_or("(untitled)")),
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD),
            )),
            &mut y,
        );
        let meta = display::movie_meta(movie);
        if !meta.is_empty() {
            line(
                Line::from(Span::styled(format!("  {meta}"), theme::dim())),
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

    fn draw_movie_detail(&self, frame: &mut Frame, area: Rect) {
        let Level::MovieDetail { movie } = &self.level else {
            return;
        };
        let area = self.draw_movie_header(frame, area, movie);
        let buf = frame.buffer_mut();
        if area.height == 0 {
            return;
        }

        // No file: one status line instead of the file block.
        let Some(file) = &movie.movie_file else {
            let queue_entry = display::movie_queue_entry(&self.queue, movie.id);
            let status = display::movie_status(movie, queue_entry, &display::now_utc_iso());
            let hint = match status {
                MovieStatus::Missing => Span::styled("  (enter: search)", theme::dim()),
                _ => Span::raw(""),
            };
            Line::from(vec![Span::raw("  "), Self::status_span(&status), hint]).render(
                Rect::new(area.x, area.y, area.width.saturating_sub(1), 1),
                buf,
            );
            return;
        };

        let text_width = area.width.saturating_sub(16) as usize;
        let none = || "-".to_string();
        let mut entries: Vec<(&str, String)> = vec![
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

    fn draw_overview_fullscreen(&mut self, frame: &mut Frame, area: Rect) {
        let Level::MovieDetail { movie } = &self.level else {
            return;
        };
        let buf = frame.buffer_mut();
        let text_width = area.width.saturating_sub(4) as usize;
        let lines = wrap_text(movie.overview.as_deref().unwrap_or_default(), text_width);
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
        let first = Self::window_start(self.release_cursor, self.releases.len(), items_per_page);
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
                detail.push(Span::styled(" !", theme::error()));
            }
            if display::previously_grabbed(&self.history, release) {
                detail.push(Span::styled(" ✓", Style::new().fg(theme::accent_bright())));
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
            Self::draw_scrollbar(buf, area, self.release_cursor, self.releases.len());
        }
    }

    fn help_entries(&self) -> Vec<(&'static str, &'static str)> {
        if self.filter_focused {
            return vec![("esc", "cancel"), ("enter", "apply")];
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
            Level::MovieList => {
                entries.push(("enter", "open"));
                entries.push(("/", "filter"));
                if self.filter_active {
                    entries.push(("esc", "clear"));
                }
                entries.push(("s", "sort"));
            }
            Level::MovieDetail { movie } => {
                entries.push(("esc", "back"));
                entries.push(("enter", "search"));
                if movie.movie_file.is_some() {
                    entries.push(("x", "delete file"));
                }
                entries.push(("v", "overview"));
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
                ("enter", "open/search"),
                ("esc", "back/clear"),
                ("/", "filter movies"),
                ("s", "sort movies"),
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
