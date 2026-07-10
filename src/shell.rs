use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;

use crate::app::{AppStatus, MediaApp, ShellRequest};
use crate::config::Config;
use crate::event::Event;
use crate::ui::theme::{self, Theme};
use crate::ui::theme_picker;

pub struct Shell {
    apps: Vec<Box<dyn MediaApp>>,
    active: usize,
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    rx: mpsc::UnboundedReceiver<Event>,
    should_quit: bool,
    /// `None` when closed; `Some(cursor)` when the theme picker is open, where
    /// `cursor` indexes `Theme::ALL`.
    theme_picker: Option<usize>,
}

impl Shell {
    pub fn new(
        apps: Vec<Box<dyn MediaApp>>,
        config: Arc<Mutex<Config>>,
        config_path: PathBuf,
        rx: mpsc::UnboundedReceiver<Event>,
    ) -> Self {
        let last_app = config.lock().unwrap().last_app.clone();
        let active = last_app
            .and_then(|id| apps.iter().position(|app| app.id() == id))
            .unwrap_or(0);
        Self {
            apps,
            active,
            config,
            config_path,
            rx,
            should_quit: false,
            theme_picker: None,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.apps[self.active].activate();
        loop {
            terminal.draw(|frame| render(frame, &mut self.apps, self.active, self.theme_picker))?;
            let Some(event) = self.rx.recv().await else {
                break;
            };
            match event {
                Event::Term(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                    self.on_key(key);
                }
                Event::Term(_) => {}
                Event::Tick => {
                    for app in &mut self.apps {
                        app.on_tick();
                    }
                }
                Event::App(app_event) => {
                    if let Some(app) = self.apps.iter_mut().find(|app| app.id() == app_event.app) {
                        app.on_event(app_event.payload);
                    }
                }
            }
            if self.should_quit {
                let mut needs_grace = false;
                for app in &mut self.apps {
                    needs_grace |= app.on_quit();
                }
                if needs_grace {
                    self.drain_shutdown().await;
                }
                break;
            }
        }
        Ok(())
    }

    /// Keep routing app events until every app has finished its shutdown
    /// work (e.g. the player's Exited after mpv quit and the final playback
    /// report) or the deadline passes. A fixed sleep would either waste time
    /// or cut the final report short; this leaves as soon as the work is
    /// actually done.
    async fn drain_shutdown(&mut self) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while !self.apps.iter().all(|app| app.ready_to_quit()) {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(Event::App(app_event))) => {
                    if let Some(app) = self.apps.iter_mut().find(|app| app.id() == app_event.app) {
                        app.on_event(app_event.payload);
                    }
                }
                // Ignore keys and ticks; the UI is already done.
                Ok(Some(_)) => {}
                // Channel closed or deadline reached: stop waiting.
                Ok(None) | Err(_) => break,
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // The picker is modal: while open it swallows every key so none reach
        // the active app (mirrors the jellyfin `pending_play` guard).
        if let Some(cursor) = self.theme_picker {
            self.handle_picker_key(key, cursor);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Left => {
                    let target = (self.active + self.apps.len() - 1) % self.apps.len();
                    self.switch_to(target);
                    return;
                }
                KeyCode::Right => {
                    let target = (self.active + 1) % self.apps.len();
                    self.switch_to(target);
                    return;
                }
                KeyCode::Char('t') => {
                    // Open on the currently active theme so it starts highlighted.
                    self.theme_picker = Some(self.config.lock().unwrap().theme as usize);
                    return;
                }
                KeyCode::Char(c @ '1'..='9') => {
                    let target = c as usize - '1' as usize;
                    if target < self.apps.len() {
                        self.switch_to(target);
                    }
                    return;
                }
                _ => {}
            }
        }
        if let Some(request) = self.apps[self.active].on_key(key) {
            match request {
                ShellRequest::Quit => self.should_quit = true,
            }
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent, cursor: usize) {
        let count = Theme::ALL.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.theme_picker = Some((cursor + count - 1) % count);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.theme_picker = Some((cursor + 1) % count);
            }
            KeyCode::Enter => {
                self.apply_theme(Theme::ALL[cursor]);
                self.theme_picker = None;
            }
            KeyCode::Esc => self.theme_picker = None,
            // ctrl+c remains the global quit even with the picker open.
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            _ => {}
        }
    }

    fn apply_theme(&mut self, theme: Theme) {
        theme::set(theme);
        let mut config = self.config.lock().unwrap();
        if config.theme == theme {
            return;
        }
        config.theme = theme;
        if let Err(err) = config.save(&self.config_path) {
            tracing::warn!(%err, "failed to persist theme");
        }
    }

    fn switch_to(&mut self, target: usize) {
        if target == self.active {
            return;
        }
        self.active = target;
        let app = &mut self.apps[target];
        app.activate();
        let mut config = self.config.lock().unwrap();
        config.last_app = Some(app.id().to_string());
        if let Err(err) = config.save(&self.config_path) {
            tracing::warn!(%err, "failed to persist last_app");
        }
    }
}

fn render(
    frame: &mut Frame,
    apps: &mut [Box<dyn MediaApp>],
    active: usize,
    theme_picker: Option<usize>,
) {
    let [tabs_area, body_area, status_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_app_tabs(frame, tabs_area, apps, active);
    apps[active].draw(frame, body_area);
    render_status_bar(frame, status_area, apps, active);
    // Drawn last (plus its own Clear) so it floats over the body.
    if let Some(cursor) = theme_picker {
        theme_picker::draw(frame, frame.area(), cursor);
    }
}

fn render_app_tabs(frame: &mut Frame, area: Rect, apps: &[Box<dyn MediaApp>], active: usize) {
    let mut spans = vec![Span::raw(" ")];
    for (i, app) in apps.iter().enumerate() {
        let style = if i == active {
            theme::selected()
        } else if app.status() == AppStatus::ComingSoon {
            theme::dim()
        } else {
            Style::new().fg(theme::fg())
        };
        spans.push(Span::styled(format!(" {} ", app.title()), style));
        if app.status() == AppStatus::ComingSoon {
            spans.push(Span::styled("(soon)", theme::dim()));
        }
        spans.push(Span::raw("  "));
    }
    let [tabs_row, rule_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    // Reserve a right-hand column for the active-theme label (titles are ASCII,
    // so char count is the display width); clamp so it never overflows the row.
    let label = format!(" theme: {} ", theme::active_theme().title());
    let label_width = (label.chars().count() as u16).min(tabs_row.width);
    let [tabs_col, label_col] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(label_width)]).areas(tabs_row);
    Line::from(spans).render(tabs_col, frame.buffer_mut());
    Line::styled(label, theme::dim())
        .right_aligned()
        .render(label_col, frame.buffer_mut());
    Line::styled("─".repeat(rule_row.width as usize), theme::dim())
        .render(rule_row, frame.buffer_mut());
}

fn render_status_bar(frame: &mut Frame, area: Rect, apps: &[Box<dyn MediaApp>], active: usize) {
    // Prefer the active app's status line, then any other app's (so e.g. a
    // now-playing bar stays visible from another tab).
    let line = std::iter::once(&apps[active])
        .chain(apps.iter().filter(|app| app.id() != apps[active].id()))
        .find_map(|app| app.status_line())
        .unwrap_or_else(|| Line::styled(" ctrl+←/→ or ctrl+1..3: switch app", theme::dim()));
    line.render(area, frame.buffer_mut());
}
