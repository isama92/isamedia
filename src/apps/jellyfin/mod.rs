use std::any::Any;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::app::{AppId, MediaApp, ShellRequest};
use crate::config::Config;
use crate::event::AppSender;
use crate::ui::theme;

pub struct JellyfinApp {
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    sender: AppSender,
}

impl JellyfinApp {
    pub fn new(config: Arc<Mutex<Config>>, config_path: PathBuf, sender: AppSender) -> Self {
        Self {
            config,
            config_path,
            sender,
        }
    }
}

impl MediaApp for JellyfinApp {
    fn id(&self) -> AppId {
        "jellyfin"
    }

    fn title(&self) -> &'static str {
        "Jellyfin"
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<ShellRequest> {
        match key.code {
            KeyCode::Char('q') => Some(ShellRequest::Quit),
            _ => None,
        }
    }

    fn on_event(&mut self, _payload: Box<dyn Any + Send>) {}

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        Line::styled("Jellyfin (not implemented yet)", theme::dim())
            .centered()
            .render(area, frame.buffer_mut());
    }
}
