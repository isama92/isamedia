use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::event::AppSender;
use crate::jellyfin::{
    Client, LibraryItemsQuery, MediaItem, display, library_scope, models::ItemKind,
};
use crate::ui::input::TextInput;
use crate::ui::text::{truncate, wrap_text};
use crate::ui::{help, list, prompt, theme};

use super::msg::Msg;

const SPINNER_FRAMES: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

/// Items per request in the Libraries tab's lazily-loaded lists.
const PAGE_SIZE: usize = 100;
/// Fetch the next page once the cursor is within this many rows of the end
/// of the loaded window.
const FETCH_AHEAD: usize = 20;
/// How many times a page in the client-filter load-everything chain is retried
/// after a transient error before the chain gives up. Bounded with exponential
/// backoff so one failed page cannot leave the filter matching a partial list,
/// nor hammer a struggling self-hosted server.
const FILTER_LOAD_MAX_RETRIES: u32 = 3;

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

/// Whether the info panel is meaningful for this item. Containers (library
/// views, box sets) carry no overview, so `i` is inert on them.
fn info_available(item: &MediaItem) -> bool {
    matches!(
        item.kind,
        ItemKind::Movie | ItemKind::Series | ItemKind::Episode | ItemKind::Video
    )
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

/// Group a flat, season-ordered episode list into seasons by season number
/// (`parent_index_number`), preserving first-seen (server) order. Season
/// `None`/`0` collects as its own bucket (rendered as "Specials"). Free
/// function so the grouping is unit-testable without a `Client`.
fn group_seasons(items: &[MediaItem]) -> Vec<SeasonRow> {
    let mut seasons: Vec<SeasonRow> = Vec::new();
    for episode in items {
        let number = episode.parent_index_number;
        let watched = display::watched(episode) as usize;
        match seasons.iter_mut().find(|season| season.number == number) {
            Some(season) => {
                season.episode_count += 1;
                season.watched_count += watched;
            }
            None => seasons.push(SeasonRow {
                number,
                episode_count: 1,
                watched_count: watched,
            }),
        }
    }
    seasons
}

/// Whether this level's `/` filter runs server-side (`searchTerm` on the
/// paginated request). Only levels that query with `recursive=true` qualify:
/// Jellyfin silently ignores `searchTerm` on non-recursive queries, so the
/// Other-library levels (collections list, collection contents) filter
/// client-side over a fully loaded list instead.
fn server_filter_level(level: &LibraryLevel) -> bool {
    matches!(
        level,
        LibraryLevel::Library {
            kind: LibraryKind::Movies | LibraryKind::Shows,
            ..
        }
    )
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
        LibraryLevel::Library { library, .. } => {
            // Same scope the client uses for the root's item counts, so the
            // number shown and the list opened can never disagree.
            let (include_item_types, recursive) = library_scope(library.collection_type.as_deref());
            (library.id.clone(), include_item_types, recursive)
        }
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

/// Sub-position while drilled into a series (`current_series` is `Some`). The
/// show-info header stays pinned; only the bottom region cycles through these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeriesView {
    Seasons,
    Episodes,
    EpisodeInfo,
}

/// A season derived from the flat episode list by grouping on the episodes'
/// season number (`parent_index_number`); `number == None`/`0` is Specials.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeasonRow {
    number: Option<i32>,
    episode_count: usize,
    watched_count: usize,
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
    /// Sub-view within a drilled-in series; ignored when `current_series` is None.
    series_view: SeriesView,
    /// Seasons derived from the loaded episode list, in first-seen order.
    seasons: Vec<SeasonRow>,
    /// Cursor within the seasons list (the episode list reuses `cursor`).
    season_cursor: usize,

    /// Drill-down position inside the Libraries tab; `Root` elsewhere.
    lib_level: LibraryLevel,
    /// Server-side total for the current Libraries list, driving pagination.
    lib_total: usize,
    /// Sort of the collection-videos level; reset on entering a collection.
    collection_sort: Sort,
    /// Sort popup cursor into `SORT_OPTIONS`; `None` when closed.
    sort_menu: Option<usize>,
    /// Full-screen info/overview panel for the selected item; modal while open.
    info_open: bool,
    /// Scroll offset into the info panel's wrapped text; clamped when drawing.
    info_scroll: u16,

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
    /// Consecutive failures of the current client-filter chain page, capped by
    /// `FILTER_LOAD_MAX_RETRIES`; reset on any successful page.
    filter_load_attempts: u32,
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
            series_view: SeriesView::Seasons,
            seasons: Vec::new(),
            season_cursor: 0,
            lib_level: LibraryLevel::Root,
            lib_total: 0,
            collection_sort: Sort::DEFAULT,
            sort_menu: None,
            info_open: false,
            info_scroll: 0,
            show_full_help: false,
            loading: false,
            error: None,
            notice: None,
            gen_counter,
            fetch_gen: 0,
            filter_load_attempts: 0,
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
    /// request) instead of narrowing the already-loaded items. True only in
    /// movie/show library listings; see `server_filter_level`. The episodes
    /// drill and the client-filtered levels behave like every other tab.
    fn uses_server_filter(&self) -> bool {
        self.tab == Tab::Libraries
            && self.current_series.is_none()
            && server_filter_level(&self.lib_level)
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

    /// Drop the displayed list ahead of a fresh navigation so the body shows
    /// `Loading...` instead of the previous view's items. Only navigation
    /// call sites use this: position-preserving refreshes and page appends
    /// keep the visible list until the result lands.
    fn clear_list(&mut self) {
        self.all_items.clear();
        self.items.clear();
    }

    pub fn fetch(&mut self) {
        // Libraries lists are paginated and go through their own path; the
        // episodes drill inside a shows library is the exception and loads
        // like any other series.
        if self.tab == Tab::Libraries && self.current_series.is_none() {
            self.fetch_library_page(0, PAGE_SIZE);
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

    /// Request `limit` items of the current library level starting at
    /// `start_index`. `start_index == 0` replaces the list (navigation,
    /// sort/filter change, refresh); a later index appends as the user scrolls.
    /// `limit` is normally `PAGE_SIZE`; a position-preserving refresh
    /// (`refetch_in_place`) passes the loaded count to reload every page
    /// scrolled through in one request.
    fn fetch_library_page(&mut self, start_index: usize, limit: usize) {
        self.fetch_library_page_after(start_index, limit, Duration::ZERO);
    }

    /// Like `fetch_library_page` but delays the request by `delay`, used by the
    /// client-filter chain's bounded retry so a transient page error does not
    /// halt the load permanently. Carries a fresh `fetch_gen` like every fetch,
    /// so a delayed page landing after the user navigates away is dropped as
    /// stale by the guard in `on_library_items_loaded`.
    fn fetch_library_page_after(&mut self, start_index: usize, limit: usize, delay: Duration) {
        let fetch_gen = self.begin_fetch();
        let client = self.client.clone();
        let sender = self.sender.clone();
        // Client-filtered levels always fetch unfiltered pages: the server
        // would ignore the searchTerm anyway, and the local filter needs the
        // complete list.
        let filter = (self.filter_active && self.uses_server_filter())
            .then(|| self.filter.value().to_string());
        match build_library_query(
            &self.lib_level,
            self.current_sort(),
            filter.as_deref(),
            start_index,
        ) {
            // Root: /Views is a handful of entries, one unpaginated request.
            None => {
                tokio::spawn(async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let result = client.get_libraries().await;
                    sender.send(Msg::LibraryItemsLoaded {
                        fetch_gen,
                        start_index: 0,
                        limit,
                        result,
                    });
                });
            }
            Some(mut query) => {
                query.limit = limit;
                tokio::spawn(async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let result = client.get_library_items(&query).await;
                    sender.send(Msg::LibraryItemsLoaded {
                        fetch_gen,
                        start_index,
                        limit,
                        result,
                    });
                });
            }
        }
    }

    /// Re-fetch the current view after a local state change (watched toggle,
    /// playback end) WITHOUT discarding the scroll position of a paginated
    /// library. It reloads every page already scrolled through in one request,
    /// so the reloaded list is the same length and the cursor still points at
    /// the same item (`apply_filter` only clamps the cursor when it is out of
    /// range). Every other view delegates to `fetch` with its existing refresh
    /// behaviour unchanged: a drilled-in series rebuilds its seasons in place,
    /// while the flat tabs reload the whole list (and may re-clamp the cursor
    /// if the item set shifted).
    fn refetch_in_place(&mut self) {
        if self.tab == Tab::Libraries
            && self.current_series.is_none()
            && !matches!(self.lib_level, LibraryLevel::Root)
        {
            let loaded = self.all_items.len().max(PAGE_SIZE);
            self.fetch_library_page(0, loaded);
        } else {
            self.fetch();
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
            self.fetch_library_page(self.all_items.len(), PAGE_SIZE);
        }
    }

    /// Kick off loading the rest of a client-filtered list the moment its
    /// filter activates, so the filter sees every item, not just the pages
    /// scrolled past so far. No-op in server-filter mode and while a fetch
    /// is in flight (`on_library_items_loaded` continues the chain).
    fn maybe_start_filter_load(&mut self) {
        if self.tab == Tab::Libraries
            && self.current_series.is_none()
            && !matches!(self.lib_level, LibraryLevel::Root)
            && !self.uses_server_filter()
            && !self.loading
            && self.all_items.len() < self.lib_total
        {
            // Fresh chain: give it a full retry budget.
            self.filter_load_attempts = 0;
            self.fetch_library_page(self.all_items.len(), PAGE_SIZE);
        }
    }

    /// Reset per-level list state after moving between library levels, then
    /// load the first page of the level just entered. The previous level's
    /// items are dropped so the body shows `Loading...` until it arrives.
    fn enter_new_level(&mut self) {
        self.clear_list();
        self.cursor = 0;
        self.lib_total = 0;
        self.filter.clear();
        self.filter_active = false;
        self.filter_focused = false;
        self.filter_load_attempts = 0;
        self.fetch_library_page(0, PAGE_SIZE);
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
                if self.current_series.is_some() {
                    // These are the series' episodes: group them into seasons
                    // (the bottom region), keeping the current sub-view so a
                    // refresh does not kick the user back to the seasons list.
                    // Keep the library filter intact so leaving the series
                    // returns to the filtered library; the filter bar stays
                    // hidden while drilled in (see `draw`).
                    self.build_seasons();
                    if self.season_cursor >= self.seasons.len() {
                        self.season_cursor = self.seasons.len().saturating_sub(1);
                    }
                    if matches!(
                        self.series_view,
                        SeriesView::Episodes | SeriesView::EpisodeInfo
                    ) {
                        self.rebuild_season_episodes();
                    }
                } else {
                    // A fresh top-level list (tab switch, search, refresh): the
                    // previous filter no longer applies.
                    self.filter.clear();
                    self.filter_active = false;
                    self.filter_focused = false;
                    self.apply_filter();
                }
            }
            Err(crate::jellyfin::Error::Unauthorized) => return true,
            Err(err) => self.error = Some(err.to_string()),
        }
        false
    }

    /// Same contract as `on_items_loaded`, for Libraries-tab pages. Unlike
    /// `on_items_loaded` this must NOT clear the filter: either the request
    /// was issued with it (server-side filtering) or the client-side filter
    /// is applied to the accumulated pages right here.
    #[must_use]
    pub fn on_library_items_loaded(
        &mut self,
        fetch_gen: u64,
        start_index: usize,
        limit: usize,
        result: Result<crate::jellyfin::ItemsResponse, crate::jellyfin::Error>,
    ) -> bool {
        if fetch_gen != self.fetch_gen {
            return false; // stale page from a superseded fetch
        }
        self.loading = false;
        match result {
            Ok(page) => {
                // A page landed: the chain made progress, so restore the full
                // retry budget for the next page.
                self.filter_load_attempts = 0;
                self.lib_total = page.total_record_count.max(0) as usize;
                let page_len = page.items.len();
                if start_index == 0 {
                    self.all_items = page.items;
                } else {
                    self.all_items.extend(page.items);
                }
                // Mirrors the page in server-filter mode, narrows it in
                // client-filter mode, so appends respect an active filter.
                self.apply_filter();
                // A client-side filter must see the whole list: keep
                // chain-loading pages while it is active. The non-empty-page
                // guard stops the chain if the server misreports the total.
                if self.filter_active
                    && !self.uses_server_filter()
                    && page_len > 0
                    && self.all_items.len() < self.lib_total
                {
                    self.fetch_library_page(self.all_items.len(), PAGE_SIZE);
                }
            }
            Err(crate::jellyfin::Error::Unauthorized) => return true,
            Err(err) => {
                // A transient error mid client-filter chain would otherwise end
                // the chain silently, leaving the filter matching a partial
                // list. Retry the same page with exponential backoff before
                // giving up, so one bad page does not strand the load.
                let in_filter_chain = self.filter_active
                    && !self.uses_server_filter()
                    && self.all_items.len() < self.lib_total;
                if in_filter_chain && self.filter_load_attempts < FILTER_LOAD_MAX_RETRIES {
                    self.filter_load_attempts += 1;
                    let backoff = Duration::from_millis(500u64 << (self.filter_load_attempts - 1));
                    // Retry the exact failed request: `limit` may exceed
                    // PAGE_SIZE (a position-preserving refresh), so reusing it
                    // avoids shrinking the list to one page then re-growing it.
                    self.fetch_library_page_after(start_index, limit, backoff);
                } else {
                    self.filter_load_attempts = 0;
                    self.error = Some(err.to_string());
                }
            }
        }
        false
    }

    /// Same contract as `on_items_loaded`: true means the session expired.
    /// A toggle *failure* is surfaced even when superseded, so the user always
    /// learns the watched state didn't change; a stale success just repaints on
    /// the newer fetch.
    #[must_use]
    pub fn on_watched_toggled(
        &mut self,
        fetch_gen: u64,
        result: Result<(), crate::jellyfin::Error>,
    ) -> bool {
        if matches!(result, Err(crate::jellyfin::Error::Unauthorized)) {
            return true; // a revoked session must re-auth regardless of staleness
        }
        if fetch_gen != self.fetch_gen {
            // Superseded: a newer fetch owns loading and will repaint the list,
            // but a toggle failure is still worth surfacing.
            if let Err(err) = result {
                self.error = Some(err.to_string());
            }
            return false;
        }
        self.loading = false;
        self.refetch_in_place();
        // After the refetch, which resets the error line; a toggle failure must
        // outlive the refetch, not flash for a tick.
        if let Err(err) = result {
            self.error = Some(err.to_string());
        }
        false
    }

    /// Group the loaded flat episode list into seasons.
    fn build_seasons(&mut self) {
        self.seasons = group_seasons(&self.all_items);
    }

    /// The season number currently selected in the seasons list (`None` is a
    /// valid value: the Specials bucket).
    fn selected_season(&self) -> Option<i32> {
        self.seasons.get(self.season_cursor).and_then(|s| s.number)
    }

    /// Rebuild `items` to just the selected season's episodes, clamping the
    /// episode cursor. Used on entering a season and on refresh.
    fn rebuild_season_episodes(&mut self) {
        let number = self.selected_season();
        self.items = self
            .all_items
            .iter()
            .filter(|episode| episode.parent_index_number == number)
            .cloned()
            .collect();
        if self.cursor >= self.items.len() {
            self.cursor = self.items.len().saturating_sub(1);
        }
    }

    /// Enter the highlighted season: switch the bottom region to its episodes.
    fn enter_season(&mut self) {
        if self.seasons.get(self.season_cursor).is_none() {
            return;
        }
        self.cursor = 0;
        self.series_view = SeriesView::Episodes;
        self.rebuild_season_episodes();
    }

    /// Refetch after playback so progress/watched state is fresh. A playback
    /// failure (e.g. mpv missing) survives the refetch's error reset;
    /// otherwise it would flash for a single tick and vanish.
    pub fn on_playback_finished(&mut self) {
        let playback_error = self.error.take();
        self.refetch_in_place();
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
                    // An empty query short-circuits in `fetch` without touching
                    // the network; keep the (empty) list as is rather than
                    // flashing `Loading...` for it.
                    if !self.search.value().is_empty() {
                        self.clear_list();
                    }
                    self.cursor = 0;
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
                        self.clear_list();
                        self.cursor = 0;
                        self.fetch_library_page(0, PAGE_SIZE);
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
                        self.clear_list();
                        self.cursor = 0;
                        self.fetch_library_page(0, PAGE_SIZE);
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
                KeyCode::Up => {
                    self.sort_menu = Some(selected.saturating_sub(1));
                }
                KeyCode::Down => {
                    self.sort_menu = Some((selected + 1).min(SORT_OPTIONS.len() - 1));
                }
                KeyCode::Enter => {
                    self.sort_menu = None;
                    let sort = SORT_OPTIONS.get(selected).copied().unwrap_or(Sort::DEFAULT);
                    if sort != self.collection_sort {
                        self.collection_sort = sort;
                        self.clear_list();
                        self.cursor = 0;
                        self.fetch_library_page(0, PAGE_SIZE);
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('v') => self.sort_menu = None,
                _ => {}
            }
            return None;
        }

        // The info panel is modal: while open it owns scroll and close keys, so
        // the list cursor stays pinned to the item being read.
        if self.info_open {
            let page = self.last_height.saturating_sub(4).max(1);
            match key.code {
                KeyCode::Up => self.info_scroll = self.info_scroll.saturating_sub(1),
                KeyCode::Down => self.info_scroll = self.info_scroll.saturating_add(1),
                KeyCode::PageUp | KeyCode::Left => {
                    self.info_scroll = self.info_scroll.saturating_sub(page);
                }
                KeyCode::PageDown | KeyCode::Right => {
                    self.info_scroll = self.info_scroll.saturating_add(page);
                }
                KeyCode::Home | KeyCode::Char('g') => self.info_scroll = 0,
                KeyCode::End | KeyCode::Char('G') => self.info_scroll = u16::MAX,
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('i') => {
                    self.info_open = false;
                    self.info_scroll = 0;
                }
                KeyCode::Char('q') => return Some(BrowseAction::Quit),
                _ => {}
            }
            return None;
        }

        // Inside a drilled-in series the bottom region owns its own seasons /
        // episodes / episode-info sub-levels with their own key handling.
        if self.current_series.is_some() {
            return self.on_series_key(key);
        }

        let page_jump = (self.last_height / 5).max(1) as usize;
        match key.code {
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => {
                if self.cursor + 1 < self.items.len() {
                    self.cursor += 1;
                }
            }
            KeyCode::PageUp | KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(page_jump);
            }
            KeyCode::PageDown | KeyCode::Right => {
                self.cursor = (self.cursor + page_jump).min(self.items.len().saturating_sub(1));
            }
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = self.items.len().saturating_sub(1),

            KeyCode::Tab if !self.drilled_in() => {
                self.tab = self.tab.next();
                self.clear_list();
                self.cursor = 0;
                self.lib_level = LibraryLevel::Root;
                self.lib_total = 0;
                self.fetch();
            }
            KeyCode::BackTab if !self.drilled_in() => {
                self.tab = self.tab.prev();
                self.clear_list();
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
                    self.maybe_start_filter_load();
                }
            }
            KeyCode::Esc | KeyCode::Backspace => {
                // A drilled-in series is handled earlier by `on_series_key`, so
                // `current_series` is always None here.
                if self.filter_active && key.code == KeyCode::Esc {
                    self.filter.clear();
                    self.filter_active = false;
                    if self.uses_server_filter() {
                        self.clear_list();
                        self.cursor = 0;
                        self.fetch_library_page(0, PAGE_SIZE);
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
                    // The now-empty query short-circuits in `fetch`, replacing
                    // the list with an empty one immediately; no clear needed.
                    self.search.clear();
                    self.cursor = 0;
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
                        self.series_view = SeriesView::Seasons;
                        self.season_cursor = 0;
                        self.seasons.clear();
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

            KeyCode::Char('v') if self.sort_key_available() => {
                self.sort_menu = Some(
                    SORT_OPTIONS
                        .iter()
                        .position(|&sort| sort == self.collection_sort)
                        .unwrap_or(0),
                );
            }
            KeyCode::Char('w') => self.toggle_watched(),
            KeyCode::Char('i') if self.current_item().is_some_and(info_available) => {
                self.info_open = true;
                self.info_scroll = 0;
            }
            KeyCode::Char('r') => self.fetch(),
            KeyCode::Char('?') => self.show_full_help = !self.show_full_help,
            KeyCode::Char('q') => return Some(BrowseAction::Quit),
            _ => {}
        }
        self.maybe_fetch_next_page();
        None
    }

    /// Key handling inside a drilled-in series, dispatched on `series_view`.
    /// The show-info header stays fixed; these keys drive the bottom region:
    /// seasons list -> episode list -> episode info.
    fn on_series_key(&mut self, key: KeyEvent) -> Option<BrowseAction> {
        let page_jump = (self.last_height / 5).max(1) as usize;
        match self.series_view {
            SeriesView::Seasons => match key.code {
                KeyCode::Up => self.season_cursor = self.season_cursor.saturating_sub(1),
                KeyCode::Down => {
                    if self.season_cursor + 1 < self.seasons.len() {
                        self.season_cursor += 1;
                    }
                }
                KeyCode::PageUp | KeyCode::Left => {
                    self.season_cursor = self.season_cursor.saturating_sub(page_jump);
                }
                KeyCode::PageDown | KeyCode::Right => {
                    self.season_cursor =
                        (self.season_cursor + page_jump).min(self.seasons.len().saturating_sub(1));
                }
                KeyCode::Home | KeyCode::Char('g') => self.season_cursor = 0,
                KeyCode::End | KeyCode::Char('G') => {
                    self.season_cursor = self.seasons.len().saturating_sub(1);
                }
                KeyCode::Enter | KeyCode::Char(' ') => self.enter_season(),
                KeyCode::Esc | KeyCode::Backspace => {
                    // Leave the series, back to the tab's list. Clear the
                    // series' episodes so nothing from it is playable during
                    // the refetch: once current_series is None, Enter routes to
                    // the top-level play handler, and current_item() must not
                    // still return a stale episode until the tab list lands.
                    self.current_series = None;
                    self.series_view = SeriesView::Seasons;
                    self.items.clear();
                    self.seasons.clear();
                    self.cursor = 0;
                    self.fetch();
                }
                KeyCode::Char('r') => self.fetch(),
                KeyCode::Char('?') => self.show_full_help = !self.show_full_help,
                KeyCode::Char('q') => return Some(BrowseAction::Quit),
                _ => {}
            },
            SeriesView::Episodes => match key.code {
                KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
                KeyCode::Down => {
                    if self.cursor + 1 < self.items.len() {
                        self.cursor += 1;
                    }
                }
                KeyCode::PageUp | KeyCode::Left => {
                    self.cursor = self.cursor.saturating_sub(page_jump);
                }
                KeyCode::PageDown | KeyCode::Right => {
                    self.cursor = (self.cursor + page_jump).min(self.items.len().saturating_sub(1));
                }
                KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
                KeyCode::End | KeyCode::Char('G') => {
                    self.cursor = self.items.len().saturating_sub(1);
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if let Some(episode) = self.current_item().cloned() {
                        return Some(BrowseAction::Play(episode));
                    }
                }
                KeyCode::Char('i') if !self.items.is_empty() => {
                    self.series_view = SeriesView::EpisodeInfo;
                    self.info_scroll = 0;
                }
                KeyCode::Char('w') => self.toggle_watched(),
                KeyCode::Esc | KeyCode::Backspace => {
                    self.series_view = SeriesView::Seasons;
                    self.cursor = 0;
                }
                KeyCode::Char('r') => self.fetch(),
                KeyCode::Char('?') => self.show_full_help = !self.show_full_help,
                KeyCode::Char('q') => return Some(BrowseAction::Quit),
                _ => {}
            },
            SeriesView::EpisodeInfo => {
                let page = self.last_height.saturating_sub(4).max(1);
                match key.code {
                    KeyCode::Up => self.info_scroll = self.info_scroll.saturating_sub(1),
                    KeyCode::Down => self.info_scroll = self.info_scroll.saturating_add(1),
                    KeyCode::PageUp | KeyCode::Left => {
                        self.info_scroll = self.info_scroll.saturating_sub(page);
                    }
                    KeyCode::PageDown | KeyCode::Right => {
                        self.info_scroll = self.info_scroll.saturating_add(page);
                    }
                    KeyCode::Home | KeyCode::Char('g') => self.info_scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => self.info_scroll = u16::MAX,
                    KeyCode::Enter => {
                        if let Some(episode) = self.current_item().cloned() {
                            return Some(BrowseAction::Play(episode));
                        }
                    }
                    KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('i') => {
                        self.series_view = SeriesView::Episodes;
                        self.info_scroll = 0;
                    }
                    KeyCode::Char('?') => self.show_full_help = !self.show_full_help,
                    KeyCode::Char('q') => return Some(BrowseAction::Quit),
                    _ => {}
                }
            }
        }
        None
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.last_height = area.height;
        let has_message = self.error.is_some() || self.notice.is_some();
        let show_search = self.tab == Tab::Search && self.current_series.is_none();
        let help_height = if self.show_full_help {
            help::rows(&self.help_sections())
        } else {
            1
        };

        let rows = Layout::vertical([
            Constraint::Length(if has_message { 1 } else { 0 }),
            Constraint::Length(2), // tab row + spacer
            Constraint::Length(if show_search { 2 } else { 0 }),
            Constraint::Length(if self.filter_active && self.current_series.is_none() {
                2
            } else {
                0
            }),
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
        if self.filter_active && self.current_series.is_none() {
            self.draw_input_row(frame, rows[3], "Filter: ", &self.filter);
        }
        if let Some(series) = self.current_series.clone() {
            // Show-info header stays pinned; the bottom region is the current
            // sub-view (cloning `series` frees the borrow for the &mut draws).
            let body = self.draw_series_header(frame, rows[4], &series);
            match self.series_view {
                SeriesView::Seasons => self.draw_seasons(frame, body),
                SeriesView::Episodes => self.draw_episode_list(frame, body),
                SeriesView::EpisodeInfo => self.draw_episode_info(frame, body),
            }
        } else if self.info_open {
            self.draw_info(frame, rows[4]);
        } else {
            self.draw_list(frame, rows[4]);
        }
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
            let message = if self.loading {
                "  Loading..."
            } else {
                "  No items."
            };
            Line::styled(message, theme::dim()).render(area, buf);
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

        // Scrollbar in the rightmost column, like jfsh. Shares the guarded
        // helper so a degenerate area can't underflow the edge arithmetic.
        if self.items.len() > items_per_page {
            list::draw_scrollbar(buf, area, self.cursor, self.items.len());
        }
    }

    /// The selected item's info: a title, the shared metadata line, genres, and
    /// the wrapped overview. Scrolls with the info-panel modal keys.
    fn draw_info(&mut self, frame: &mut Frame, area: Rect) {
        let text_width = area.width.saturating_sub(4) as usize;
        // Build the owned lines first so the borrow of `self.items` is released
        // before we mutate `self.info_scroll` below.
        let lines: Vec<Line> = {
            let Some(item) = self.items.get(self.cursor) else {
                self.info_open = false;
                return;
            };
            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(Span::styled(
                format!("  {}", display::item_title(item)),
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!("  {}", display::item_description(item)),
                theme::dim(),
            )));
            if !item.genres.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  Genres: {}", item.genres.join(", ")),
                    theme::dim(),
                )));
            }
            lines.push(Line::from(""));
            let overview = item.overview.as_deref().unwrap_or("No overview available.");
            for line in wrap_text(overview, text_width) {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::new().fg(theme::fg()),
                )));
            }
            lines
        };

        let buf = frame.buffer_mut();
        let total = lines.len();
        let height = area.height as usize;
        let max_scroll = total.saturating_sub(height).min(u16::MAX as usize) as u16;
        self.info_scroll = self.info_scroll.min(max_scroll);
        for (row, line) in lines
            .into_iter()
            .skip(self.info_scroll as usize)
            .take(height)
            .enumerate()
        {
            line.render(
                Rect::new(area.x, area.y + row as u16, area.width.saturating_sub(1), 1),
                buf,
            );
        }
        if total > height {
            list::draw_scrollbar(
                buf,
                area,
                self.info_scroll as usize,
                max_scroll as usize + 1,
            );
        }
    }

    /// The pinned show-info header shown across all three series sub-views:
    /// title, a "year · rating" meta line, and the show overview (clamped so a
    /// very long synopsis can't push the bottom list off screen). Returns the
    /// leftover `Rect` below it for the sub-view's list.
    fn draw_series_header(&self, frame: &mut Frame, area: Rect, series: &MediaItem) -> Rect {
        // Clamp the overview by characters (bounds the header on any terminal),
        // and always keep at least a few rows for the bottom list.
        const OVERVIEW_MAX_CHARS: usize = 600;
        const MIN_LIST_ROWS: u16 = 4;

        let buf = frame.buffer_mut();
        let text_width = area.width.saturating_sub(4) as usize;

        let overview_raw = series
            .overview
            .as_deref()
            .unwrap_or("No overview available.");
        let overview = if overview_raw.chars().count() > OVERVIEW_MAX_CHARS {
            let clipped: String = overview_raw.chars().take(OVERVIEW_MAX_CHARS).collect();
            format!("{}…", clipped.trim_end())
        } else {
            overview_raw.to_string()
        };

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!("  {}", display::item_title(series)),
            Style::new()
                .fg(theme::accent_bright())
                .add_modifier(Modifier::BOLD),
        )));
        let mut meta: Vec<String> = Vec::new();
        if let Some(year) = series.production_year {
            meta.push(year.to_string());
        }
        if let Some(rating) = series.community_rating
            && rating > 0.0
        {
            meta.push(format!("{rating:.1}"));
        }
        if !meta.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {}", meta.join(" · ")),
                theme::dim(),
            )));
        }
        lines.push(Line::from(""));
        for line in wrap_text(&overview, text_width) {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::new().fg(theme::fg()),
            )));
        }
        lines.push(Line::from(""));

        let max_header = area.height.saturating_sub(MIN_LIST_ROWS).max(1) as usize;
        let header_h = lines.len().min(max_header);
        for (row, line) in lines.into_iter().take(header_h).enumerate() {
            line.render(
                Rect::new(area.x, area.y + row as u16, area.width.saturating_sub(1), 1),
                buf,
            );
        }

        let bottom = area.y + area.height;
        let top = (area.y + header_h as u16).min(bottom);
        Rect::new(area.x, top, area.width, bottom.saturating_sub(top))
    }

    /// The seasons list (bottom region of the Seasons sub-view).
    fn draw_seasons(&self, frame: &mut Frame, area: Rect) {
        let buf = frame.buffer_mut();
        if area.height == 0 {
            return;
        }
        if self.seasons.is_empty() {
            let message = if self.loading {
                "  Loading..."
            } else {
                "  No seasons."
            };
            Line::styled(message, theme::dim()).render(area, buf);
            return;
        }
        let per_page = area.height as usize;
        let first = list::window_start(self.season_cursor, self.seasons.len(), per_page);
        let last = (first + per_page).min(self.seasons.len());
        for (row, (i, season)) in self
            .seasons
            .iter()
            .enumerate()
            .take(last)
            .skip(first)
            .enumerate()
        {
            let selected = i == self.season_cursor;
            let fully_watched =
                season.episode_count > 0 && season.watched_count == season.episode_count;
            let gutter = if selected {
                Span::styled(" │ ", Style::new().fg(theme::accent_bright()))
            } else {
                Span::raw("   ")
            };
            let name_style = if selected {
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD)
            } else if fully_watched {
                theme::dim()
            } else {
                Style::new().fg(theme::fg())
            };
            // Show the watched/total ratio inline next to the name (with a
            // check once the whole season is watched) rather than in a
            // far-right column, so the count reads as part of the season.
            let count = if fully_watched {
                format!("  {}/{} ✓", season.watched_count, season.episode_count)
            } else {
                format!("  {}/{}", season.watched_count, season.episode_count)
            };
            let row_area = Rect::new(area.x, area.y + row as u16, area.width.saturating_sub(1), 1);
            Line::from(vec![
                gutter,
                Span::styled(display::season_name(season.number), name_style),
                Span::styled(count, theme::dim()),
            ])
            .render(row_area, buf);
        }
        if self.seasons.len() > per_page {
            list::draw_scrollbar(buf, area, self.season_cursor, self.seasons.len());
        }
    }

    /// The selected season's episode list (bottom region of the Episodes
    /// sub-view). One line per episode: code + title, watched/progress marker.
    fn draw_episode_list(&self, frame: &mut Frame, area: Rect) {
        let buf = frame.buffer_mut();
        if area.height == 0 {
            return;
        }
        if self.items.is_empty() {
            let message = if self.loading {
                "  Loading..."
            } else {
                "  No episodes."
            };
            Line::styled(message, theme::dim()).render(area, buf);
            return;
        }
        let per_page = area.height as usize;
        let first = list::window_start(self.cursor, self.items.len(), per_page);
        let last = (first + per_page).min(self.items.len());
        for (row, (i, item)) in self
            .items
            .iter()
            .enumerate()
            .take(last)
            .skip(first)
            .enumerate()
        {
            let selected = i == self.cursor;
            let watched = display::watched(item);
            let base_style = if selected {
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD)
            } else if watched {
                theme::dim()
            } else {
                Style::new().fg(theme::fg())
            };
            let gutter = if selected {
                Span::styled(" │ ", Style::new().fg(theme::accent_bright()))
            } else {
                Span::raw("   ")
            };
            let code = display::episode_code(item);
            // Watched check / progress marker sits inline after the title
            // (like movies/videos) instead of in a far-right column.
            let marker = if watched {
                "  ✓".to_string()
            } else {
                let pct = item
                    .user_data
                    .as_ref()
                    .and_then(|data| data.played_percentage)
                    .unwrap_or(0.0);
                if pct > 0.0 {
                    format!("  {pct:.0}%")
                } else {
                    String::new()
                }
            };
            let row_area = Rect::new(area.x, area.y + row as u16, area.width.saturating_sub(1), 1);
            // Reserve the gutter (3), the code plus its two spaces, and a fixed
            // slot (6) for the marker so a long title never pushes it off-screen.
            let title_width = (row_area.width as usize)
                .saturating_sub(3 + code.len() + 2 + 6)
                .max(4);
            let title = truncate(item.name.as_deref().unwrap_or(""), title_width);
            Line::from(vec![
                gutter,
                Span::styled(format!("{code}  "), base_style),
                Span::styled(title, base_style),
                Span::styled(marker, theme::dim()),
            ])
            .render(row_area, buf);
        }
        if self.items.len() > per_page {
            list::draw_scrollbar(buf, area, self.cursor, self.items.len());
        }
    }

    /// The selected episode's info (bottom region of the EpisodeInfo sub-view):
    /// code + title, meta, technical streams, then the wrapped overview.
    /// Scrolls with `info_scroll`; reuses the `draw_info` window/scrollbar math.
    fn draw_episode_info(&mut self, frame: &mut Frame, area: Rect) {
        let text_width = area.width.saturating_sub(4) as usize;
        let lines: Vec<Line> = {
            let Some(item) = self.items.get(self.cursor) else {
                self.series_view = SeriesView::Episodes;
                return;
            };
            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}  {}",
                    display::episode_code(item),
                    item.name.as_deref().unwrap_or("")
                ),
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD),
            )));
            let meta = display::episode_meta(item);
            if !meta.is_empty() {
                lines.push(Line::from(Span::styled(format!("  {meta}"), theme::dim())));
            }
            for (label, value) in display::media_summary(item) {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {label:<7}"), theme::dim()),
                    Span::styled(value, Style::new().fg(theme::fg())),
                ]));
            }
            lines.push(Line::from(""));
            let overview = item.overview.as_deref().unwrap_or("No overview available.");
            for line in wrap_text(overview, text_width) {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::new().fg(theme::fg()),
                )));
            }
            lines
        };

        let buf = frame.buffer_mut();
        let total = lines.len();
        let height = area.height as usize;
        let max_scroll = total.saturating_sub(height).min(u16::MAX as usize) as u16;
        self.info_scroll = self.info_scroll.min(max_scroll);
        for (row, line) in lines
            .into_iter()
            .skip(self.info_scroll as usize)
            .take(height)
            .enumerate()
        {
            line.render(
                Rect::new(area.x, area.y + row as u16, area.width.saturating_sub(1), 1),
                buf,
            );
        }
        if total > height {
            list::draw_scrollbar(
                buf,
                area,
                self.info_scroll as usize,
                max_scroll as usize + 1,
            );
        }
    }

    fn help_entries(&self) -> Vec<(&'static str, &'static str)> {
        if self.search_focused || self.filter_focused {
            return vec![("esc", "cancel"), ("enter", "apply")];
        }
        if self.sort_menu.is_some() {
            return vec![("esc", "close"), ("enter", "select")];
        }
        if self.info_open {
            return vec![
                ("↑/↓", "scroll"),
                ("←/→", "page"),
                ("g/G", "top/bottom"),
                ("i/esc", "close"),
            ];
        }
        if self.current_series.is_some() {
            let help = if self.show_full_help {
                "close help"
            } else {
                "help"
            };
            return match self.series_view {
                SeriesView::Seasons => vec![
                    ("↑/↓", "move"),
                    ("enter", "open season"),
                    ("esc", "back"),
                    ("r", "refresh"),
                    ("?", help),
                    ("q", "quit"),
                ],
                SeriesView::Episodes => vec![
                    ("↑/↓", "move"),
                    ("enter", "play"),
                    ("i", "info"),
                    ("w", "watched"),
                    ("esc", "back"),
                    ("r", "refresh"),
                    ("?", help),
                    ("q", "quit"),
                ],
                SeriesView::EpisodeInfo => vec![
                    ("↑/↓", "scroll"),
                    ("←/→", "page"),
                    ("g/G", "top/bottom"),
                    ("i/esc", "close"),
                    ("q", "quit"),
                ],
            };
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
        if self.current_item().is_some_and(info_available) {
            entries.push(("i", "info"));
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
            entries.push(("v", "sort"));
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

    /// Columns for the expanded (`?`) help: navigation on the left, the
    /// context-aware `help_entries` on the right, so the full help shows only
    /// what applies to the current screen (never a stale cheat sheet).
    fn help_sections(&self) -> Vec<help::Section> {
        let mut nav = help::NAV.to_vec();
        if !self.drilled_in() && !self.info_open {
            nav.push(("tab", "next tab"));
            nav.push(("shift+tab", "prev tab"));
        }
        vec![
            help::Section {
                title: "Move",
                entries: nav,
            },
            help::Section {
                title: "Actions",
                entries: self.help_entries(),
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

    fn episode(season: Option<i32>, number: i32, played: bool) -> MediaItem {
        MediaItem {
            id: "x".into(),
            kind: ItemKind::Episode,
            parent_index_number: season,
            index_number: Some(number),
            user_data: Some(crate::jellyfin::models::UserData {
                played: Some(played),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn group_seasons_buckets_by_number_in_first_seen_order() {
        let items = vec![
            episode(Some(1), 1, true),
            episode(Some(1), 2, false),
            episode(Some(2), 1, false),
            episode(None, 1, true), // a special with no season number
            episode(Some(2), 2, true),
        ];
        let seasons = group_seasons(&items);
        assert_eq!(seasons.len(), 3);
        // First-seen order preserved: season 1, season 2, then the None bucket.
        assert_eq!(seasons[0].number, Some(1));
        assert_eq!((seasons[0].episode_count, seasons[0].watched_count), (2, 1));
        assert_eq!(seasons[1].number, Some(2));
        assert_eq!((seasons[1].episode_count, seasons[1].watched_count), (2, 1));
        assert_eq!(seasons[2].number, None);
        assert_eq!((seasons[2].episode_count, seasons[2].watched_count), (1, 1));
        assert!(group_seasons(&[]).is_empty());
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
    fn server_filter_only_in_recursive_library_listings() {
        // Jellyfin ignores searchTerm on non-recursive queries, so only the
        // recursive movie/show listings may filter server-side.
        let movies = LibraryLevel::Library {
            library: library("m", Some("movies")),
            kind: LibraryKind::Movies,
        };
        let shows = LibraryLevel::Library {
            library: library("t", Some("tvshows")),
            kind: LibraryKind::Shows,
        };
        let other = LibraryLevel::Library {
            library: library("o", Some("boxsets")),
            kind: LibraryKind::Other,
        };
        let collection = LibraryLevel::Collection {
            library: library("o", Some("boxsets")),
            collection: MediaItem {
                id: "box1".into(),
                kind: ItemKind::BoxSet,
                ..Default::default()
            },
        };
        assert!(server_filter_level(&movies));
        assert!(server_filter_level(&shows));
        assert!(!server_filter_level(&other));
        assert!(!server_filter_level(&collection));
        assert!(!server_filter_level(&LibraryLevel::Root));
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
}
