use std::any::Any;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::app::{AppId, AppStatus, MediaApp, ShellRequest};
use crate::ui::theme;

/// Placeholder for apps that are planned but not implemented yet.
pub struct ComingSoonApp {
    id: AppId,
    title: &'static str,
}

impl ComingSoonApp {
    pub fn new(id: AppId, title: &'static str) -> Self {
        Self { id, title }
    }
}

impl MediaApp for ComingSoonApp {
    fn id(&self) -> AppId {
        self.id
    }

    fn title(&self) -> &'static str {
        self.title
    }

    fn status(&self) -> AppStatus {
        AppStatus::ComingSoon
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<ShellRequest> {
        match key.code {
            KeyCode::Char('q') => Some(ShellRequest::Quit),
            _ => None,
        }
    }

    fn on_event(&mut self, _payload: Box<dyn Any + Send>) {}

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let [_, middle, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(2),
            Constraint::Fill(1),
        ])
        .areas(area);
        Line::styled(
            format!("{} support is coming soon.", self.title),
            theme::accent(),
        )
        .centered()
        .render(middle, frame.buffer_mut());
        let [_, hint_row] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(middle);
        Line::styled("ctrl+←/→ to switch back", theme::dim())
            .centered()
            .render(hint_row, frame.buffer_mut());
    }
}
