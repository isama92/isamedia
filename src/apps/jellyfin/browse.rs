use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::app::ShellRequest;
use crate::event::AppSender;
use crate::jellyfin::{Client, MediaItem, display, models::ItemKind};
use crate::ui::input::TextInput;
use crate::ui::theme;

use super::msg::Msg;

const SPINNER_FRAMES: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Resume,
    NextUp,
    RecentlyAdded,
    Search,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Resume, Tab::NextUp, Tab::RecentlyAdded, Tab::Search];

    fn name(self) -> &'static str {
        match self {
            Tab::Resume => "Resume",
            Tab::NextUp => "Next Up",
            Tab::RecentlyAdded => "Recently Added",
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

/// What the browse screen wants its parent (the Jellyfin app) to do.
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

    show_full_help: bool,
    loading: bool,
    pub error: Option<String>,
    fetch_gen: u64,
    spinner_frame: usize,
    /// Terminal height from the last draw, for jfsh-style page jumps.
    last_height: u16,
}

impl Browse {
    pub fn new(client: Client, sender: AppSender) -> Self {
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
            show_full_help: false,
            loading: false,
            error: None,
            fetch_gen: 0,
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

    pub fn fetch(&mut self) {
        self.loading = true;
        self.error = None;
        self.fetch_gen += 1;
        let fetch_gen = self.fetch_gen;
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

    pub fn on_items_loaded(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<MediaItem>, crate::jellyfin::Error>,
    ) {
        if fetch_gen != self.fetch_gen {
            return; // stale response from a superseded fetch
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
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    pub fn on_watched_toggled(&mut self, result: Result<(), crate::jellyfin::Error>) {
        self.loading = false;
        if let Err(err) = result {
            self.error = Some(err.to_string());
        }
        self.fetch();
    }

    /// Refetch after playback so progress/watched state is fresh.
    pub fn on_playback_finished(&mut self) {
        self.fetch();
    }

    fn apply_filter(&mut self) {
        if !self.filter_active || self.filter.value().is_empty() {
            self.items = self.all_items.clone();
        } else {
            let needle = self.filter.value().to_lowercase();
            self.items = self
                .all_items
                .iter()
                .filter(|item| {
                    display::item_title(item).to_lowercase().contains(&needle)
                        || display::item_description(item).to_lowercase().contains(&needle)
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
        if item.kind == ItemKind::Series {
            return;
        }
        let watched = display::watched(item);
        let item_id = item.id.clone();
        self.loading = true;
        let client = self.client.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = if watched {
                client.mark_unwatched(&item_id).await
            } else {
                client.mark_watched(&item_id).await
            };
            sender.send(Msg::WatchedToggled(result));
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
            match key.code {
                KeyCode::Esc => {
                    self.filter_focused = false;
                    self.filter.clear();
                    self.filter_active = false;
                    self.apply_filter();
                }
                KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    self.filter_focused = false;
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

            KeyCode::Right | KeyCode::Char('l') if self.current_series.is_none() => {
                self.tab = self.tab.next();
                self.cursor = 0;
                self.fetch();
            }
            KeyCode::Left | KeyCode::Char('h') if self.current_series.is_none() => {
                self.tab = self.tab.prev();
                self.cursor = 0;
                self.fetch();
            }

            KeyCode::Char('/') => {
                if self.tab == Tab::Search && self.current_series.is_none() {
                    self.search_focused = true;
                    self.search.focused = true;
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
                    self.apply_filter();
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
                    if item.kind == ItemKind::Series {
                        self.current_series = Some(item);
                        self.cursor = 0;
                        self.fetch();
                    } else {
                        return Some(BrowseAction::Play(item));
                    }
                }
            }

            KeyCode::Char('w') => self.toggle_watched(),
            KeyCode::Char('r') => self.fetch(),
            KeyCode::Char('?') => self.show_full_help = !self.show_full_help,
            KeyCode::Char('q') => return Some(BrowseAction::Quit),
            _ => {}
        }
        None
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.last_height = area.height;
        let has_error = self.error.is_some();
        let show_search = self.tab == Tab::Search && self.current_series.is_none();
        let help_height = if self.show_full_help { 8 } else { 1 };

        let rows = Layout::vertical([
            Constraint::Length(if has_error { 1 } else { 0 }),
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
    }

    fn draw_tabs(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![Span::raw(" ")];
        if let Some(series) = &self.current_series {
            spans.push(Span::styled(
                format!(" {} ", display::item_title(series)),
                Style::new().fg(theme::FG).bg(theme::ACCENT),
            ));
        } else {
            for tab in Tab::ALL {
                let style = if tab == self.tab {
                    Style::new().fg(theme::FG).bg(theme::ACCENT)
                } else {
                    Style::new().fg(theme::FG).bg(theme::TAB_BG)
                };
                spans.push(Span::styled(format!(" {} ", tab.name()), style));
                spans.push(Span::raw(" "));
            }
        }
        if self.loading {
            spans.push(Span::styled(
                format!(" {}", SPINNER_FRAMES[self.spinner_frame]),
                Style::new().fg(theme::ACCENT_BRIGHT),
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
        Line::styled(format!("  {prompt}"), Style::new().fg(theme::FG))
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
                        Span::styled(" │ ", Style::new().fg(theme::ACCENT_BRIGHT)),
                        Span::styled(
                            title,
                            Style::new()
                                .fg(theme::ACCENT_BRIGHT)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(vec![
                        Span::styled(" │ ", Style::new().fg(theme::ACCENT_BRIGHT)),
                        Span::styled(desc, Style::new().fg(theme::ACCENT)),
                    ]),
                )
            } else {
                let title_style = if display::watched(item) {
                    theme::dim()
                } else {
                    Style::new().fg(theme::FG)
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
                    ("█", Style::new().fg(theme::ACCENT))
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
        let mut entries = Vec::new();
        if self.current_series.is_some() {
            entries.push(("esc", "back"));
        }
        if self
            .current_item()
            .is_some_and(|item| item.kind != ItemKind::Series)
        {
            entries.push(("w", "toggle watched"));
        }
        if self.tab == Tab::Search && self.current_series.is_none() {
            entries.push(("/", "search"));
            if !self.search.value().is_empty() {
                entries.push(("esc", "clear"));
            }
        } else {
            entries.push(("/", "filter"));
            if self.filter_active {
                entries.push(("esc", "clear"));
            }
        }
        entries.push(("?", if self.show_full_help { "close help" } else { "help" }));
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
                spans.push(Span::styled(key, Style::new().fg(theme::DIM)));
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
                ("→/l", "next tab"),
                ("←/h", "prev tab"),
                ("r", "refresh"),
                ("enter", "select"),
                ("/", "search/filter"),
                ("esc", "clear/back"),
            ],
            &[
                ("w", "toggle watched"),
                ("q", "quit"),
                ("?", "close help"),
            ],
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
                        Span::styled(format!("  {key:<10}"), Style::new().fg(theme::DIM)),
                        Span::styled(desc.to_string(), theme::dim()),
                    ])
                    .render(line_area, buf);
                }
            }
        }
    }
}

fn truncate(text: &str, max_width: usize) -> String {
    if text.chars().count() <= max_width {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_width.saturating_sub(1)).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_cycle_wraps() {
        assert_eq!(Tab::Search.next(), Tab::Resume);
        assert_eq!(Tab::Resume.prev(), Tab::Search);
        assert_eq!(Tab::Resume.next(), Tab::NextUp);
    }

    #[test]
    fn truncate_handles_multibyte() {
        assert_eq!(truncate("héllo wörld", 20), "héllo wörld");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }
}
