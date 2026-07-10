use std::any::Any;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::app::{AppId, MediaApp, ShellRequest};
use crate::config::Config;
use crate::ui::theme::{self, Theme};

/// Which setting a row edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setting {
    Theme,
    Accent,
}

/// Open editor state: which setting, and the cursor within its choice list.
struct Editing {
    setting: Setting,
    cursor: usize,
}

/// A settings screen. Purely local: it mutates the global palette and persists
/// the choice to the config file; it spawns no async work and needs no sender.
pub struct SettingsApp {
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    /// Cursor over the visible setting rows.
    cursor: usize,
    /// `Some` while a choice list is open (modal within the tab).
    editing: Option<Editing>,
}

impl SettingsApp {
    pub fn new(config: Arc<Mutex<Config>>, config_path: PathBuf) -> Self {
        Self {
            config,
            config_path,
            cursor: 0,
            editing: None,
        }
    }

    /// The settings rows currently shown. Accent only appears when the active
    /// theme offers accents.
    fn rows(&self) -> Vec<Setting> {
        let mut rows = vec![Setting::Theme];
        if !theme::active_theme().accents().is_empty() {
            rows.push(Setting::Accent);
        }
        rows
    }

    fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
        }
    }

    /// Apply the chosen value for a setting: update the live palette and persist.
    fn apply(&mut self, setting: Setting, choice: usize) {
        match setting {
            Setting::Theme => {
                let theme = Theme::ALL[choice];
                theme::set(theme);
                let mut config = self.config.lock().unwrap();
                config.theme = theme;
                if let Err(err) = config.save(&self.config_path) {
                    tracing::warn!(%err, "failed to persist theme");
                }
            }
            Setting::Accent => {
                let accent = theme::active_theme().accents()[choice];
                theme::set_accent(accent);
                let mut config = self.config.lock().unwrap();
                config.accent = accent;
                if let Err(err) = config.save(&self.config_path) {
                    tracing::warn!(%err, "failed to persist accent");
                }
            }
        }
    }

    fn draw_list(&self, area: Rect, buf: &mut Buffer) {
        let rows = self.rows();
        for (i, &setting) in rows.iter().enumerate() {
            if (i as u16) < area.height {
                setting_row(setting, i == self.cursor)
                    .render(Rect::new(area.x, area.y + i as u16, area.width, 1), buf);
            }
        }
        if area.height as usize > rows.len() + 1 {
            Line::styled("  enter: change   j/k: move   q: quit", theme::dim()).render(
                Rect::new(area.x, area.y + area.height - 1, area.width, 1),
                buf,
            );
        }
    }
}

/// A settings row: `> Theme    Catppuccin Latte` (Accent adds a colour swatch).
fn setting_row(setting: Setting, selected: bool) -> Line<'static> {
    let label_style = if selected {
        theme::selected()
    } else {
        Style::new().fg(theme::fg())
    };
    let marker = if selected { "> " } else { "  " };
    let mut spans = vec![Span::styled(format!("  {marker}"), label_style)];
    match setting {
        Setting::Theme => {
            spans.push(Span::styled("Theme    ", label_style));
            spans.push(Span::styled(theme::active_theme().title(), theme::dim()));
        }
        Setting::Accent => {
            let accent = theme::active_accent();
            spans.push(Span::styled("Accent   ", label_style));
            spans.push(Span::styled(
                "\u{2588} ",
                Style::new().fg(theme::accent_colors(accent).accent),
            ));
            spans.push(Span::styled(accent.title(), theme::dim()));
        }
    }
    Line::from(spans)
}

/// A choice row inside an open editor, optionally prefixed with a colour swatch.
fn choice_line(selected: bool, swatch: Option<Color>, title: &'static str) -> Line<'static> {
    let style = if selected {
        theme::selected()
    } else {
        Style::new().fg(theme::fg())
    };
    let marker = if selected { "> " } else { "  " };
    let mut spans = vec![Span::styled(format!("  {marker}"), style)];
    if let Some(color) = swatch {
        spans.push(Span::styled("\u{2588} ", Style::new().fg(color)));
    }
    spans.push(Span::styled(title, style));
    Line::from(spans)
}

fn draw_editor(editing: &Editing, area: Rect, buf: &mut Buffer) {
    let (header, lines): (&str, Vec<Line<'static>>) = match editing.setting {
        Setting::Theme => (
            "Theme",
            Theme::ALL
                .iter()
                .enumerate()
                .map(|(i, theme)| choice_line(i == editing.cursor, None, theme.title()))
                .collect(),
        ),
        Setting::Accent => (
            "Accent",
            theme::active_theme()
                .accents()
                .iter()
                .enumerate()
                .map(|(i, accent)| {
                    choice_line(
                        i == editing.cursor,
                        Some(theme::accent_colors(*accent).accent),
                        accent.title(),
                    )
                })
                .collect(),
        ),
    };
    Line::styled(format!("  {header}"), theme::selected())
        .render(Rect::new(area.x, area.y, area.width, 1), buf);
    for (i, line) in lines.into_iter().enumerate() {
        let y = area.y + 2 + i as u16;
        if y < area.y + area.height {
            line.render(Rect::new(area.x, y, area.width, 1), buf);
        }
    }
    if area.height > 2 {
        Line::styled("  enter: select   esc: back", theme::dim()).render(
            Rect::new(area.x, area.y + area.height - 1, area.width, 1),
            buf,
        );
    }
}

/// Number of choices in a setting's list.
fn choice_count(setting: Setting) -> usize {
    match setting {
        Setting::Theme => Theme::ALL.len(),
        Setting::Accent => theme::active_theme().accents().len(),
    }
}

/// Index of the currently active value, so an editor opens on the live choice.
fn current_choice_index(setting: Setting) -> usize {
    match setting {
        Setting::Theme => Theme::ALL
            .iter()
            .position(|&t| t == theme::active_theme())
            .unwrap_or(0),
        Setting::Accent => theme::active_theme()
            .accents()
            .iter()
            .position(|&a| a == theme::active_accent())
            .unwrap_or(0),
    }
}

impl MediaApp for SettingsApp {
    fn id(&self) -> AppId {
        "settings"
    }

    fn title(&self) -> &'static str {
        "Settings"
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<ShellRequest> {
        // Editing a setting is modal within the tab: only navigate/apply/cancel.
        if let Some(editing) = self.editing.as_ref() {
            let setting = editing.setting;
            let cursor = editing.cursor;
            let count = choice_count(setting);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                    self.editing = Some(Editing {
                        setting,
                        cursor: (cursor + count - 1) % count,
                    });
                }
                KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                    self.editing = Some(Editing {
                        setting,
                        cursor: (cursor + 1) % count,
                    });
                }
                KeyCode::Enter => {
                    self.apply(setting, cursor);
                    self.editing = None;
                    self.clamp_cursor();
                }
                KeyCode::Esc => self.editing = None,
                _ => {}
            }
            return None;
        }

        let rows = self.rows();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if !rows.is_empty() => {
                self.cursor = (self.cursor + rows.len() - 1) % rows.len();
            }
            KeyCode::Down | KeyCode::Char('j') if !rows.is_empty() => {
                self.cursor = (self.cursor + 1) % rows.len();
            }
            KeyCode::Enter => {
                if let Some(&setting) = rows.get(self.cursor) {
                    self.editing = Some(Editing {
                        setting,
                        cursor: current_choice_index(setting),
                    });
                }
            }
            KeyCode::Char('q') if key.modifiers.is_empty() => return Some(ShellRequest::Quit),
            _ => {}
        }
        None
    }

    fn on_event(&mut self, _payload: Box<dyn Any + Send>) {}

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let buf = frame.buffer_mut();
        let [_, title_row, _, body] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(area);
        Line::styled("  Settings", theme::selected()).render(title_row, buf);
        match self.editing.as_ref() {
            Some(editing) => draw_editor(editing, body, buf),
            None => self.draw_list(body, buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn app() -> SettingsApp {
        SettingsApp::new(
            Arc::new(Mutex::new(Config::default())),
            PathBuf::from("settings-test-config.toml"),
        )
    }

    #[test]
    fn draw_does_not_panic_on_narrow_terminals() {
        let mut settings = app();
        for (width, height) in [(10, 5), (1, 1), (0, 0), (24, 3), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            // The settings list.
            terminal.draw(|f| settings.draw(f, f.area())).unwrap();
            // Each editor view.
            for setting in [Setting::Theme, Setting::Accent] {
                settings.editing = Some(Editing { setting, cursor: 0 });
                terminal.draw(|f| settings.draw(f, f.area())).unwrap();
            }
            settings.editing = None;
        }
    }
}
