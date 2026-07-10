use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::event::AppSender;
use crate::jellyfin::{Client, LibraryItemsQuery, MediaItem, display, models::ItemKind};
use crate::ui::input::TextInput;
use crate::ui::{prompt, theme};

use super::msg::Msg;

const SPINNER_FRAMES: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

/// Items per request in the Libraries tab's lazily-loaded lists.
const PAGE_SIZE: usize = 100;
/// Fetch the next page once the cursor is within this many rows of the end
/// of the loaded window.
const FETCH_AHEAD: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Resume,
    NextUp,
    RecentlyAdded,
    Libraries,
    Search,
}

impl Tab {
    const ALL: [Tab; 5] = [
        Tab::Resume,
        Tab::NextUp,
        Tab::RecentlyAdded,
        Tab::Libraries,
        Tab::Search,
    ];

    fn name(self) -> &'static str {
        match self {
            Tab::Resume => "Resume",
            Tab::NextUp => "Next Up",
            Tab::RecentlyAdded => "Recently Added",
            Tab::Libraries => "Libraries",
            Tab::Search => "Search",
        }
    }

    fn next(self) -> Tab {
        let idx = Tab::ALL.iter().position(|&t| t == self).unwrap();
        Tab::ALL[(idx + 1) % Tab::ALL.len()]
    }

    fn prev(self) -> Tab {
        let idx = Tab::ALL.iter().position(|&t| t == self).unwrap();
        Tab::ALL[(idx + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

/// How a library behaves when opened; derived from the view's
/// `CollectionType`, since every view shares `Type: "CollectionFolder"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryKind {
    Movies,
    Shows,
    Other,
}

fn library_kind(item: &MediaItem) -> LibraryKind {
    match item.collection_type.as_deref() {
        Some("movies") => LibraryKind::Movies,
        Some("tvshows") => LibraryKind::Shows,
        _ => LibraryKind::Other,
    }
}

/// Where the user is inside the Libraries tab. A flat enum rather than a
/// stack: depth is bounded at two, and collections only exist inside Other
/// libraries, so popping `Collection` always lands on `Library { Other }`.
#[allow(clippy::large_enum_variant)] // one instance per Browse, size is irrelevant
#[derive(Debug, Clone)]
enum LibraryLevel {
    Root,
    Library {
        library: MediaItem,
        kind: LibraryKind,
    },
    Collection {
        library: MediaItem,
        collection: MediaItem,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortField {
    Name,
    DateAdded,
    ReleaseDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortDir {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sort {
    field: SortField,
    dir: SortDir,
}

impl Sort {
    const DEFAULT: Sort = Sort {
        field: SortField::DateAdded,
        dir: SortDir::Descending,
    };

    fn api_params(self) -> (&'static str, &'static str) {
        let sort_by = match self.field {
            SortField::Name => "SortName",
            SortField::DateAdded => "DateCreated",
            SortField::ReleaseDate => "PremiereDate",
        };
        let sort_order = match self.dir {
            SortDir::Ascending => "Ascending",
            SortDir::Descending => "Descending",
        };
        (sort_by, sort_order)
    }

    fn label(self) -> &'static str {
        match (self.field, self.dir) {
            (SortField::DateAdded, SortDir::Descending) => "Date added, newest first",
            (SortField::DateAdded, SortDir::Ascending) => "Date added, oldest first",
            (SortField::Name, SortDir::Ascending) => "Name, A to Z",
            (SortField::Name, SortDir::Descending) => "Name, Z to A",
            (SortField::ReleaseDate, SortDir::Descending) => "Release date, newest first",
            (SortField::ReleaseDate, SortDir::Ascending) => "Release date, oldest first",
        }
    }
}

/// Sort menu entries, default first.
const SORT_OPTIONS: [Sort; 6] = [
    Sort {
        field: SortField::DateAdded,
        dir: SortDir::Descending,
    },
    Sort {
        field: SortField::DateAdded,
        dir: SortDir::Ascending,
    },
    Sort {
        field: SortField::Name,
        dir: SortDir::Ascending,
    },
    Sort {
        field: SortField::Name,
        dir: SortDir::Descending,
    },
    Sort {
        field: SortField::ReleaseDate,
        dir: SortDir::Descending,
    },
    Sort {
        field: SortField::ReleaseDate,
        dir: SortDir::Ascending,
    },
];

/// Whether the cursor is close enough to the end of the loaded window to
/// warrant fetching the next page. `loading` blocks a second in-flight page.
fn should_fetch_more(cursor: usize, loaded: usize, total: usize, loading: bool) -> bool {
    !loading && loaded < total && cursor + FETCH_AHEAD >= loaded
}

/// The request for one page of the current library level; `None` at the
/// root, which is served by `get_libraries` instead. Free function (not a
/// method) so it can be unit-tested without a `Client`.
fn build_library_query(
    level: &LibraryLevel,
    sort: Sort,
    filter: Option<&str>,
    start_index: usize,
) -> Option<LibraryItemsQuery> {
    let (parent_id, include_item_types, recursive) = match level {
        LibraryLevel::Root => return None,
        LibraryLevel::Library { library, kind } => (
            library.id.clone(),
            match kind {
                LibraryKind::Movies => Some("Movie"),
                LibraryKind::Shows => Some("Series"),
                LibraryKind::Other => None,
            },
            // Movie/show libraries recurse into folders; an Other library
            // lists its direct children (the collections themselves).
            !matches!(kind, LibraryKind::Other),
        ),
        LibraryLevel::Collection { collection, .. } => (collection.id.clone(), None, false),
    };
    let (sort_by, sort_order) = sort.api_params();
    Some(LibraryItemsQuery {
        parent_id,
        include_item_types,
        recursive,
        start_index,
        limit: PAGE_SIZE,
        sort_by,
        sort_order,
        search_term: filter.filter(|term| !term.is_empty()).map(str::to_string),
    })
}

/// What the browse screen wants its parent (the Jellyfin app) to do.
#[allow(clippy::large_enum_variant)] // one short-lived value, size is irrelevant
pub enum BrowseAction {
    Quit,
    /// Play this item; if it is an episode, play it within its series.
    Play(MediaItem),
}

pub struct Browse {
    pub client: Client,
    sender: AppSender,

    tab: Tab,
    all_items: Vec<MediaItem>,
    /// Currently visible items (filter applied).
    items: Vec<MediaItem>,
    cursor: usize,

    search: TextInput,
    search_focused: bool,
    filter: TextInput,
    filter_active: bool,
    filter_focused: bool,

    current_series: Option<MediaItem>,

    /// Drill-down position inside the Libraries tab; `Root` elsewhere.
    lib_level: LibraryLevel,
    /// Server-side total for the current Libraries list, driving pagination.
    lib_total: usize,
    /// Sort of the collection-videos level; reset on entering a collection.
    collection_sort: Sort,
    /// Sort popup cursor into `SORT_OPTIONS`; `None` when closed.
    sort_menu: Option<usize>,

    show_full_help: bool,
    loading: bool,
    pub error: Option<String>,
    /// One-line advisory (e.g. plain-http nudge after auto-login), shown in
    /// the error row when there is no error; cleared on the next fetch.
    pub notice: Option<String>,
    /// Allocator for fetch generations. Owned by the app and shared across
    /// Browse instances, so a fetch still in flight from a pre-relogin
    /// instance can never collide with this instance's generations.
    gen_counter: Arc<AtomicU64>,
    /// Generation of this instance's latest fetch; older results are stale.
    fetch_gen: u64,
    has_fetched: bool,
    spinner_frame: usize,
    /// Terminal height from the last draw, for jfsh-style page jumps.
    last_height: u16,
}

impl Browse {
    pub fn new(client: Client, sender: AppSender, gen_counter: Arc<AtomicU64>) -> Self {
        let mut browse = Self {
            client,
            sender,
            tab: Tab::Resume,
            all_items: Vec::new(),
            items: Vec::new(),
            cursor: 0,
            search: TextInput::default(),
            search_focused: false,
            filter: TextInput::default(),
            filter_active: false,
            filter_focused: false,
            current_series: None,
            lib_level: LibraryLevel::Root,
            lib_total: 0,
            collection_sort: Sort::DEFAULT,
            sort_menu: None,
            show_full_help: false,
            loading: false,
            error: None,
            notice: None,
            gen_counter,
            fetch_gen: 0,
            has_fetched: false,
            spinner_frame: 0,
            last_height: 24,
        };
        browse.fetch();
        browse
    }

    pub fn on_tick(&mut self) {
        if self.loading {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
        }
    }

    fn current_item(&self) -> Option<&MediaItem> {
        self.items.get(self.cursor)
    }

    /// Whether a text input currently swallows plain character keys (so
    /// app-level shortcuts like `s` must stay out of the way).
    pub fn input_focused(&self) -> bool {
        self.search_focused || self.filter_focused
    }

    /// Whether the user is inside any drill-down level; tab switching is
    /// blocked while drilled in.
    fn drilled_in(&self) -> bool {
        self.current_series.is_some()
            || (self.tab == Tab::Libraries && !matches!(self.lib_level, LibraryLevel::Root))
    }

    /// Whether `/` filters server-side (`searchTerm` on the paginated
    /// request) instead of narrowing the already-loaded items. True inside
    /// any Libraries drill level except the episodes view, which is a plain
    /// unpaginated list like everywhere else.
    fn uses_server_filter(&self) -> bool {
        self.tab == Tab::Libraries
            && self.current_series.is_none()
            && !matches!(self.lib_level, LibraryLevel::Root)
    }

    /// Only videos inside a collection are user-sortable; every other
    /// Libraries list uses the default sort.
    fn current_sort(&self) -> Sort {
        match self.lib_level {
            LibraryLevel::Collection { .. } => self.collection_sort,
            _ => Sort::DEFAULT,
        }
    }

    fn sort_key_available(&self) -> bool {
        self.tab == Tab::Libraries
            && self.current_series.is_none()
            && matches!(self.lib_level, LibraryLevel::Collection { .. })
    }

    /// Mark a fetch as started and allocate its generation; any result
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

    pub fn fetch(&mut self) {
        // Libraries lists are paginated and go through their own path; the
        // episodes drill inside a shows library is the exception and loads
        // like any other series.
        if self.tab == Tab::Libraries && self.current_series.is_none() {
            self.fetch_library_page(0);
            return;
        }
        let fetch_gen = self.begin_fetch();
        let client = self.client.clone();
        let sender = self.sender.clone();
        let series = self.current_series.clone();
        let tab = self.tab;
        let query = self.search.value().to_string();
        tokio::spawn(async move {
            let result = match (&series, tab) {
                (Some(series), _) => client.get_episodes(series).await,
                (None, Tab::Resume) => client.get_resume().await,
                (None, Tab::NextUp) => client.get_next_up().await,
                (None, Tab::RecentlyAdded) => client.get_recently_added().await,
                // Handled by fetch_library_page above; kept for exhaustiveness.
                (None, Tab::Libraries) => Ok(Vec::new()),
                (None, Tab::Search) => {
                    if query.is_empty() {
                        Ok(Vec::new())
                    } else {
                        client.search(&query).await
                    }
                }
            };
            sender.send(Msg::ItemsLoaded { fetch_gen, result });
        });
    }

    /// Request one page of the current library level. `start_index == 0`
    /// replaces the list (navigation, sort/filter change, refresh); a later
    /// index appends as the user scrolls.
    fn fetch_library_page(&mut self, start_index: usize) {
        let fetch_gen = self.begin_fetch();
        let client = self.client.clone();
        let sender = self.sender.clone();
        let filter = self.filter_active.then(|| self.filter.value().to_string());
        match build_library_query(
            &self.lib_level,
            self.current_sort(),
            filter.as_deref(),
            start_index,
        ) {
            // Root: /Views is a handful of entries, one unpaginated request.
            None => {
                tokio::spawn(async move {
                    let result = client.get_libraries().await;
                    sender.send(Msg::LibraryItemsLoaded {
                        fetch_gen,
                        start_index: 0,
                        result,
                    });
                });
            }
            Some(query) => {
                tokio::spawn(async move {
                    let result = client.get_library_items(&query).await;
                    sender.send(Msg::LibraryItemsLoaded {
                        fetch_gen,
                        start_index,
                        result,
                    });
                });
            }
        }
    }

    fn maybe_fetch_next_page(&mut self) {
        if self.tab != Tab::Libraries
            || self.current_series.is_some()
            // The root list is never paginated, whatever total the server
            // reports for it.
            || matches!(self.lib_level, LibraryLevel::Root)
        {
            return;
        }
        if should_fetch_more(
            self.cursor,
            self.all_items.len(),
            self.lib_total,
            self.loading,
        ) {
            self.fetch_library_page(self.all_items.len());
        }
    }

    /// Reset per-level list state after moving between library levels, then
    /// load the first page of the level just entered. The previous level's
    /// items stay visible until it arrives, like every other fetch.
    fn enter_new_level(&mut self) {
        self.cursor = 0;
        self.lib_total = 0;
        self.filter.clear();
        self.filter_active = false;
        self.filter_focused = false;
        self.fetch_library_page(0);
    }

    /// Returns true when the server rejected the session token (the caller
    /// must drop to the login screen). Staleness is checked FIRST: a 401
    /// from a superseded fetch belongs to a token that may since have been
    /// replaced by a re-login, and must not kick the fresh session.
    #[must_use]
    pub fn on_items_loaded(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<MediaItem>, crate::jellyfin::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // stale response from a superseded fetch
        }
        self.loading = false;
        match result {
            Ok(items) => {
                self.all_items = items;
                self.filter.clear();
                self.filter_active = false;
                self.filter_focused = false;
                self.apply_filter();
            }
            Err(crate::jellyfin::Error::Unauthorized) => return true,
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Same contract as `on_items_loaded`, for Libraries-tab pages. Unlike
    /// `on_items_loaded` this must NOT clear the filter: the request was
    /// issued with it (server-side filtering), so the result matches it.
    #[must_use]
    pub fn on_library_items_loaded(
        &mut self,
        fetch_gen: u64,
        start_index: usize,
        result: Result<crate::jellyfin::ItemsResponse, crate::jellyfin::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // stale page from a superseded fetch
        }
        self.loading = false;
        match result {
            Ok(page) => {
                self.lib_total = page.total_record_count.max(0) as usize;
                if start_index == 0 {
                    self.all_items = page.items;
                } else {
                    self.all_items.extend(page.items);
                }
                // The fetched list is already filtered and sorted; it IS the
                // visible list.
                self.items = self.all_items.clone();
                if self.cursor >= self.items.len() {
                    self.cursor = self.items.len().saturating_sub(1);
                }
            }
            Err(crate::jellyfin::Error::Unauthorized) => return true,
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Same contract as `on_items_loaded`: true means the session expired.
    #[must_use]
    pub fn on_watched_toggled(
        &mut self,
        fetch_gen: u64,
        result: Result<(), crate::jellyfin::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // superseded: the user moved to another view meanwhile
        }
        if matches!(result, Err(crate::jellyfin::Error::Unauthorized)) {
            return true;
        }
        self.loading = false;
        self.fetch();
        // After fetch(), which resets the error line; a toggle failure must
        // outlive the refetch, not flash for a tick.
        if let Err(err) = result {
            self.error = Some(err.to_string());
        }
        false
    }

    /// Refetch after playback so progress/watched state is fresh. A playback
    /// failure (e.g. mpv missing) survives the refetch's error reset;
    /// otherwise it would flash for a single tick and vanish.
    pub fn on_playback_finished(&mut self) {
        let playback_error = self.error.take();
        self.fetch();
        self.error = playback_error;
    }

    fn apply_filter(&mut self) {
        // Server-side filtering: the loaded items already match the filter,
        // so a residual call just mirrors them.
        if self.uses_server_filter() {
            self.items = self.all_items.clone();
            if self.cursor >= self.items.len() {
                self.cursor = 0;
            }
            return;
        }
        if !self.filter_active || self.filter.value().is_empty() {
            self.items = self.all_items.clone();
        } else {
            let needle = self.filter.value().to_lowercase();
            self.items = self
                .all_items
                .iter()
                .filter(|item| {
                    display::item_title(item).to_lowercase().contains(&needle)
                        || display::item_description(item)
                            .to_lowercase()
                            .contains(&needle)
                })
                .cloned()
                .collect();
        }
        if self.cursor >= self.items.len() {
            self.cursor = 0;
        }
    }

    fn toggle_watched(&mut self) {
        let Some(item) = self.current_item() else {
            return;
        };
        // Only individually playable items toggle; series, libraries and
        // collections are containers.
        if !matches!(
            item.kind,
            ItemKind::Movie | ItemKind::Episode | ItemKind::Video
        ) {
            return;
        }
        let watched = display::watched(item);
        let item_id = item.id.clone();
        self.loading = true;
        let fetch_gen = self.fetch_gen;
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = if watched {
                client.mark_unwatched(&item_id).await
            } else {
                client.mark_watched(&item_id).await
            };
            sender.send(Msg::WatchedToggled { fetch_gen, result });
        });
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<BrowseAction> {
        if self.search_focused {
            match key.code {
                KeyCode::Esc => self.search_focused = false,
                KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    self.search_focused = false;
                    self.fetch();
                }
                _ => {
                    self.search.on_key(key);
                }
            }
            self.search.focused = self.search_focused;
            return None;
        }

        if self.filter_focused {
            if self.uses_server_filter() {
                // Server-side filter: typing only edits the text; leaving the
                // input applies it as a fresh page-0 fetch, mirroring the
                // Search tab's apply-on-unfocus. Applying on EVERY unfocus
                // path (not just Enter) keeps a later page append from mixing
                // an edited-but-unapplied term with the visible list.
                match key.code {
                    KeyCode::Esc => {
                        self.filter_focused = false;
                        self.filter.clear();
                        self.filter_active = false;
                        self.cursor = 0;
                        self.fetch_library_page(0);
                    }
                    KeyCode::Enter
                    | KeyCode::Tab
                    | KeyCode::BackTab
                    | KeyCode::Up
                    | KeyCode::Down => {
                        self.filter_focused = false;
                        if self.filter.value().is_empty() {
                            self.filter_active = false;
                        }
                        self.cursor = 0;
                        self.fetch_library_page(0);
                    }
                    _ => {
                        self.filter.on_key(key);
                    }
                }
            } else {
                match key.code {
                    KeyCode::Esc => {
                        self.filter_focused = false;
                        self.filter.clear();
                        self.filter_active = false;
                        self.apply_filter();
                    }
                    KeyCode::Enter
                    | KeyCode::Tab
                    | KeyCode::BackTab
                    | KeyCode::Up
                    | KeyCode::Down => {
                        self.filter_focused = false;
                    }
                    _ => {
                        if self.filter.on_key(key) {
                            self.apply_filter();
                        }
                    }
                }
            }
            self.filter.focused = self.filter_focused;
            return None;
        }

        // The shell only claims ctrl+c/arrows/digits; don't let other
        // ctrl/alt-modified chars fall through to single-letter shortcuts
        // (ctrl+d is not "page down", ctrl+q is not "quit").
        if crate::app::modified_char(&key) {
            return None;
        }

        // The sort popup is modal: while open it owns every key.
        if let Some(selected) = self.sort_menu {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.sort_menu = Some(selected.saturating_sub(1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.sort_menu = Some((selected + 1).min(SORT_OPTIONS.len() - 1));
                }
                KeyCode::Enter => {
                    self.sort_menu = None;
                    let sort = SORT_OPTIONS.get(selected).copied().unwrap_or(Sort::DEFAULT);
                    if sort != self.collection_sort {
                        self.collection_sort = sort;
                        self.cursor = 0;
                        self.fetch_library_page(0);
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => self.sort_menu = None,
                _ => {}
            }
            return None;
        }

        let page_jump = (self.last_height / 5).max(1) as usize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if self.cursor + 1 < self.items.len() {
                    self.cursor += 1;
                }
            }
            KeyCode::PageUp | KeyCode::Char('b') | KeyCode::Char('u') => {
                self.cursor = self.cursor.saturating_sub(page_jump);
            }
            KeyCode::PageDown | KeyCode::Char('f') | KeyCode::Char('d') => {
                self.cursor = (self.cursor + page_jump).min(self.items.len().saturating_sub(1));
            }
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = self.items.len().saturating_sub(1),

            KeyCode::Right | KeyCode::Char('l') if !self.drilled_in() => {
                self.tab = self.tab.next();
                self.cursor = 0;
                self.lib_level = LibraryLevel::Root;
                self.lib_total = 0;
                self.fetch();
            }
            KeyCode::Left | KeyCode::Char('h') if !self.drilled_in() => {
                self.tab = self.tab.prev();
                self.cursor = 0;
                self.lib_level = LibraryLevel::Root;
                self.lib_total = 0;
                self.fetch();
            }

            KeyCode::Char('/') => {
                if self.tab == Tab::Search && self.current_series.is_none() {
                    self.search_focused = true;
                    self.search.focused = true;
                } else if self.tab == Tab::Libraries
                    && self.current_series.is_none()
                    && matches!(self.lib_level, LibraryLevel::Root)
                {
                    // No filter at the libraries root: /Views takes no
                    // searchTerm and the list is a handful of entries.
                } else {
                    self.filter_active = true;
                    self.filter_focused = true;
                    self.filter.focused = true;
                }
            }
            KeyCode::Esc | KeyCode::Backspace => {
                if self.current_series.is_some() && !self.filter_active {
                    self.current_series = None;
                    self.cursor = 0;
                    self.fetch();
                } else if self.filter_active && key.code == KeyCode::Esc {
                    self.filter.clear();
                    self.filter_active = false;
                    if self.uses_server_filter() {
                        self.cursor = 0;
                        self.fetch_library_page(0);
                    } else {
                        self.apply_filter();
                    }
                } else if self.tab == Tab::Libraries
                    && self.current_series.is_none()
                    && !self.filter_active
                {
                    // Pop one library level; collections only exist inside
                    // Other libraries, so that is always where they return to.
                    match std::mem::replace(&mut self.lib_level, LibraryLevel::Root) {
                        LibraryLevel::Collection { library, .. } => {
                            self.lib_level = LibraryLevel::Library {
                                library,
                                kind: LibraryKind::Other,
                            };
                            self.enter_new_level();
                        }
                        LibraryLevel::Library { .. } => self.enter_new_level(),
                        LibraryLevel::Root => {}
                    }
                } else if self.tab == Tab::Search
                    && key.code == KeyCode::Esc
                    && !self.search.value().is_empty()
                {
                    self.search.clear();
                    self.fetch();
                }
            }

            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(item) = self.current_item().cloned() {
                    if self.tab == Tab::Libraries && self.current_series.is_none() {
                        match (&self.lib_level, item.kind) {
                            (LibraryLevel::Root, _) => {
                                let kind = library_kind(&item);
                                self.lib_level = LibraryLevel::Library {
                                    library: item,
                                    kind,
                                };
                                self.enter_new_level();
                                return None;
                            }
                            (
                                LibraryLevel::Library {
                                    library,
                                    kind: LibraryKind::Other,
                                },
                                ItemKind::BoxSet,
                            ) => {
                                self.lib_level = LibraryLevel::Collection {
                                    library: library.clone(),
                                    collection: item,
                                };
                                // Sort belongs to this level; entering
                                // always starts from the default.
                                self.collection_sort = Sort::DEFAULT;
                                self.enter_new_level();
                                return None;
                            }
                            _ => {}
                        }
                    }
                    if item.kind == ItemKind::Series {
                        self.current_series = Some(item);
                        self.cursor = 0;
                        self.fetch();
                    } else if !matches!(item.kind, ItemKind::BoxSet | ItemKind::CollectionFolder) {
                        // Containers never play; anything else does. A video
                        // inside a collection plays alone (only episodes are
                        // expanded to a playlist, by the player).
                        return Some(BrowseAction::Play(item));
                    }
                }
            }

            KeyCode::Char('s') if self.sort_key_available() => {
                self.sort_menu = Some(
                    SORT_OPTIONS
                        .iter()
                        .position(|&sort| sort == self.collection_sort)
                        .unwrap_or(0),
                );
            }
            KeyCode::Char('w') => self.toggle_watched(),
            KeyCode::Char('r') => self.fetch(),
            KeyCode::Char('?') => self.show_full_help = !self.show_full_help,
            KeyCode::Char('q') => return Some(BrowseAction::Quit),
            _ => {}
        }
        self.maybe_fetch_next_page();
        None
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.last_height = area.height;
        let has_message = self.error.is_some() || self.notice.is_some();
        let show_search = self.tab == Tab::Search && self.current_series.is_none();
        let help_height = if self.show_full_help { 8 } else { 1 };

        let rows = Layout::vertical([
            Constraint::Length(if has_message { 1 } else { 0 }),
            Constraint::Length(2), // tab row + spacer
            Constraint::Length(if show_search { 2 } else { 0 }),
            Constraint::Length(if self.filter_active { 2 } else { 0 }),
            Constraint::Fill(1), // item list
            Constraint::Length(help_height),
        ])
        .split(area);

        if let Some(error) = &self.error {
            Line::styled(format!("Error: {error}"), theme::error())
                .render(rows[0], frame.buffer_mut());
        } else if let Some(notice) = &self.notice {
            Line::styled(notice.clone(), theme::accent()).render(rows[0], frame.buffer_mut());
        }
        self.draw_tabs(frame, rows[1]);
        if show_search {
            self.draw_input_row(frame, rows[2], "Search: ", &self.search);
        }
        if self.filter_active {
            self.draw_input_row(frame, rows[3], "Filter: ", &self.filter);
        }
        self.draw_list(frame, rows[4]);
        self.draw_help(frame, rows[5]);
        if let Some(selected) = self.sort_menu {
            let labels: Vec<&str> = SORT_OPTIONS.iter().map(|sort| sort.label()).collect();
            prompt::draw_menu(frame, area, "Sort by", &labels, selected);
        }
    }

    /// "Library" or "Library / Collection" pill while drilled into the
    /// Libraries tab; `None` at the root (the tab strip shows instead).
    fn library_breadcrumb(&self) -> Option<String> {
        if self.tab != Tab::Libraries {
            return None;
        }
        let name = |item: &MediaItem| item.name.clone().unwrap_or_default();
        match &self.lib_level {
            LibraryLevel::Root => None,
            LibraryLevel::Library { library, .. } => Some(name(library)),
            LibraryLevel::Collection {
                library,
                collection,
            } => Some(format!("{} / {}", name(library), name(collection))),
        }
    }

    fn draw_tabs(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![Span::raw(" ")];
        if let Some(series) = &self.current_series {
            spans.push(Span::styled(
                format!(" {} ", display::item_title(series)),
                Style::new()
                    .fg(theme::on_accent())
                    .bg(theme::accent_color()),
            ));
        } else if let Some(breadcrumb) = self.library_breadcrumb() {
            spans.push(Span::styled(
                format!(" {breadcrumb} "),
                Style::new()
                    .fg(theme::on_accent())
                    .bg(theme::accent_color()),
            ));
        } else {
            for tab in Tab::ALL {
                let style = if tab == self.tab {
                    Style::new()
                        .fg(theme::on_accent())
                        .bg(theme::accent_color())
                } else {
                    Style::new().fg(theme::on_tab()).bg(theme::tab_bg())
                };
                spans.push(Span::styled(format!(" {} ", tab.name()), style));
                spans.push(Span::raw(" "));
            }
        }
        if self.loading {
            spans.push(Span::styled(
                format!(" {}", SPINNER_FRAMES[self.spinner_frame]),
                Style::new().fg(theme::accent_bright()),
            ));
        }
        Line::from(spans).render(area, frame.buffer_mut());
    }

    fn draw_input_row(&self, frame: &mut Frame, area: Rect, prompt: &str, input: &TextInput) {
        let [row, _] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
        let [prompt_area, input_area] = Layout::horizontal([
            Constraint::Length(prompt.len() as u16 + 2),
            Constraint::Fill(1),
        ])
        .areas(row);
        Line::styled(format!("  {prompt}"), Style::new().fg(theme::fg()))
            .render(prompt_area, frame.buffer_mut());
        input.render(input_area, frame.buffer_mut());
    }

    fn draw_list(&self, frame: &mut Frame, area: Rect) {
        let buf = frame.buffer_mut();
        if self.items.is_empty() {
            Line::styled("  No items.", theme::dim()).render(area, buf);
            return;
        }

        let avail_height = area.height as usize;
        let items_per_page = (avail_height / 3).max(1);
        let mut first = self.cursor.saturating_sub(items_per_page / 2);
        if first > self.items.len().saturating_sub(items_per_page) {
            first = self.items.len().saturating_sub(items_per_page);
        }
        let last = (first + items_per_page).min(self.items.len());

        let text_width = area.width.saturating_sub(6) as usize;
        let mut y = area.y;
        for (i, item) in self.items.iter().enumerate().take(last).skip(first) {
            let title = truncate(&display::item_title(item), text_width);
            let desc = truncate(&display::item_description(item), text_width);
            let (title_line, desc_line) = if i == self.cursor {
                (
                    Line::from(vec![
                        Span::styled(" │ ", Style::new().fg(theme::accent_bright())),
                        Span::styled(
                            title,
                            Style::new()
                                .fg(theme::accent_bright())
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(vec![
                        Span::styled(" │ ", Style::new().fg(theme::accent_bright())),
                        Span::styled(desc, Style::new().fg(theme::accent_color())),
                    ]),
                )
            } else {
                let title_style = if display::watched(item) {
                    theme::dim()
                } else {
                    Style::new().fg(theme::fg())
                };
                (
                    Line::from(Span::styled(format!("   {title}"), title_style)),
                    Line::from(Span::styled(format!("   {desc}"), theme::dim())),
                )
            };
            let row = |offset: u16| Rect::new(area.x, y + offset, area.width.saturating_sub(1), 1);
            if y + 1 < area.y + area.height {
                title_line.render(row(0), buf);
                desc_line.render(row(1), buf);
            }
            y += 3;
        }

        // Hand-drawn scrollbar in the rightmost column, like jfsh.
        if self.items.len() > items_per_page {
            let x = area.x + area.width - 1;
            let thumb = ((self.cursor as f64 / (self.items.len() - 1) as f64)
                * (avail_height - 1) as f64)
                .round() as usize;
            for i in 0..avail_height {
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
    }

    fn help_entries(&self) -> Vec<(&'static str, &'static str)> {
        if self.search_focused || self.filter_focused {
            return vec![("esc", "cancel"), ("enter", "apply")];
        }
        if self.sort_menu.is_some() {
            return vec![("esc", "close"), ("enter", "select")];
        }
        let mut entries = Vec::new();
        if self.drilled_in() {
            entries.push(("esc", "back"));
        }
        if self.current_item().is_some_and(|item| {
            matches!(
                item.kind,
                ItemKind::Movie | ItemKind::Episode | ItemKind::Video
            )
        }) {
            entries.push(("w", "toggle watched"));
        }
        if self.tab == Tab::Search && self.current_series.is_none() {
            entries.push(("/", "search"));
            if !self.search.value().is_empty() {
                entries.push(("esc", "clear"));
            }
        } else if self.tab == Tab::Libraries
            && self.current_series.is_none()
            && matches!(self.lib_level, LibraryLevel::Root)
        {
            // No filter at the libraries root.
        } else {
            entries.push(("/", "filter"));
            if self.filter_active {
                entries.push(("esc", "clear"));
            }
        }
        if self.sort_key_available() {
            entries.push(("s", "sort"));
        }
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

        let mut third: Vec<(&str, &str)> = vec![("w", "toggle watched")];
        if self.sort_key_available() {
            third.push(("s", "sort (in collection)"));
        }
        third.push(("q", "quit"));
        third.push(("?", "close help"));
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
                ("→/l", "next tab"),
                ("←/h", "prev tab"),
                ("r", "refresh"),
                ("enter", "select"),
                ("/", "search/filter"),
                ("esc", "clear/back"),
            ],
            &third,
        ];
        let areas = Layout::horizontal([
            Constraint::Length(26),
            Constraint::Length(26),
            Constraint::Length(26),
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

/// Truncate to a terminal column budget with a trailing ellipsis. Measures
/// display width, not chars: CJK characters occupy two columns each.
fn truncate(text: &str, max_width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    if text.width() <= max_width {
        return text.to_string();
    }
    let budget = max_width.saturating_sub(1); // leave a column for the ellipsis
    let mut used = 0;
    let mut truncated = String::new();
    for c in text.chars() {
        let char_width = c.width().unwrap_or(0);
        if used + char_width > budget {
            break;
        }
        used += char_width;
        truncated.push(c);
    }
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_cycle_wraps() {
        assert_eq!(Tab::RecentlyAdded.next(), Tab::Libraries);
        assert_eq!(Tab::Libraries.next(), Tab::Search);
        assert_eq!(Tab::Search.next(), Tab::Resume);
        assert_eq!(Tab::Resume.prev(), Tab::Search);
        assert_eq!(Tab::Resume.next(), Tab::NextUp);
    }

    fn library(id: &str, collection_type: Option<&str>) -> MediaItem {
        MediaItem {
            id: id.into(),
            name: Some(id.to_string()),
            kind: ItemKind::CollectionFolder,
            collection_type: collection_type.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn library_kind_classifies_by_collection_type() {
        assert_eq!(
            library_kind(&library("a", Some("movies"))),
            LibraryKind::Movies
        );
        assert_eq!(
            library_kind(&library("b", Some("tvshows"))),
            LibraryKind::Shows
        );
        assert_eq!(
            library_kind(&library("c", Some("boxsets"))),
            LibraryKind::Other
        );
        assert_eq!(
            library_kind(&library("d", Some("music"))),
            LibraryKind::Other
        );
        assert_eq!(library_kind(&library("e", None)), LibraryKind::Other);
    }

    #[test]
    fn sort_api_params_cover_all_options() {
        let expected = [
            ("DateCreated", "Descending"),
            ("DateCreated", "Ascending"),
            ("SortName", "Ascending"),
            ("SortName", "Descending"),
            ("PremiereDate", "Descending"),
            ("PremiereDate", "Ascending"),
        ];
        for (sort, expected) in SORT_OPTIONS.iter().zip(expected) {
            assert_eq!(sort.api_params(), expected);
        }
        assert_eq!(Sort::DEFAULT.api_params(), ("DateCreated", "Descending"));
        assert_eq!(
            SORT_OPTIONS[0],
            Sort::DEFAULT,
            "default sorts first in the menu"
        );
    }

    #[test]
    fn should_fetch_more_predicate() {
        // Never while a fetch is already in flight.
        assert!(!should_fetch_more(99, 100, 500, true));
        // Never once everything is loaded.
        assert!(!should_fetch_more(99, 100, 100, false));
        // Not while the cursor is far from the end of the window.
        assert!(!should_fetch_more(10, 100, 500, false));
        // At the threshold, with more available.
        assert!(should_fetch_more(80, 100, 500, false));
        // An empty list has nothing to page.
        assert!(!should_fetch_more(0, 0, 0, false));
    }

    #[test]
    fn library_query_per_level() {
        let movies = LibraryLevel::Library {
            library: library("lib-movies", Some("movies")),
            kind: LibraryKind::Movies,
        };
        let query = build_library_query(&movies, Sort::DEFAULT, None, 0).unwrap();
        assert_eq!(query.parent_id, "lib-movies");
        assert_eq!(query.include_item_types, Some("Movie"));
        assert!(query.recursive);
        assert_eq!(query.limit, PAGE_SIZE);
        assert_eq!(query.search_term, None);

        let shows = LibraryLevel::Library {
            library: library("lib-shows", Some("tvshows")),
            kind: LibraryKind::Shows,
        };
        let query = build_library_query(&shows, Sort::DEFAULT, Some("expanse"), 100).unwrap();
        assert_eq!(query.include_item_types, Some("Series"));
        assert!(query.recursive);
        assert_eq!(query.start_index, 100);
        assert_eq!(query.search_term.as_deref(), Some("expanse"));

        let other = LibraryLevel::Library {
            library: library("lib-other", Some("boxsets")),
            kind: LibraryKind::Other,
        };
        let query = build_library_query(&other, Sort::DEFAULT, Some(""), 0).unwrap();
        assert_eq!(query.include_item_types, None);
        assert!(!query.recursive, "collections list direct children");
        assert_eq!(query.search_term, None, "empty filter sends no searchTerm");

        let collection = LibraryLevel::Collection {
            library: library("lib-other", Some("boxsets")),
            collection: MediaItem {
                id: "box1".into(),
                kind: ItemKind::BoxSet,
                ..Default::default()
            },
        };
        let sort = Sort {
            field: SortField::Name,
            dir: SortDir::Ascending,
        };
        let query = build_library_query(&collection, sort, None, 0).unwrap();
        assert_eq!(query.parent_id, "box1");
        assert_eq!((query.sort_by, query.sort_order), ("SortName", "Ascending"));

        assert!(build_library_query(&LibraryLevel::Root, Sort::DEFAULT, None, 0).is_none());
    }

    #[test]
    fn truncate_handles_multibyte() {
        assert_eq!(truncate("héllo wörld", 20), "héllo wörld");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn truncate_counts_display_width() {
        use unicode_width::UnicodeWidthStr;

        // 5 chars but 10 columns: fits a 10-column budget untouched...
        assert_eq!(truncate("こんにちは", 10), "こんにちは");
        // ...but a 6-column budget fits only 2 double-width chars + ellipsis.
        assert_eq!(truncate("こんにちは", 6), "こん…");
        // A double-width char never straddles the boundary.
        assert_eq!(truncate("こんにちは", 5), "こん…");
        assert!(truncate("攻殻機動隊 S01E01 (1995)", 12).width() <= 12);
    }
}
