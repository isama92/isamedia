use std::any::Any;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Line;

/// True for a character key carrying ctrl/alt/super-style modifiers. Such
/// keys must not trigger single-letter shortcuts (ctrl+s is not `s`). Shift
/// is exempt: shifted chars ('G', '?') already arrive as their own character
/// with SHIFT set.
pub fn modified_char(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char(_)) && !(key.modifiers - KeyModifiers::SHIFT).is_empty()
}

pub type AppId = &'static str;

/// Things an app can ask the shell to do in response to a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellRequest {
    Quit,
}

/// One selectable app in the top tab bar (Jellyfin, Sonarr, Radarr).
///
/// Apps own their entire keymap and state. The shell only handles quit and
/// app switching, routes `AppEvent`s to the app named in the event, and
/// renders the frame chrome (app tabs + status bar).
pub trait MediaApp {
    fn id(&self) -> AppId;

    /// Tab label.
    fn title(&self) -> &'static str;

    /// Called when the app becomes the active tab (including at boot).
    fn activate(&mut self) {}

    /// Called when the app stops being the active tab (the user switched
    /// away). Apps should pause work that only matters while visible — e.g. a
    /// periodic marker poll — here; background jobs the shell still surfaces
    /// (a now-playing bar, an auto-search status line) must keep running.
    fn deactivate(&mut self) {}

    fn on_key(&mut self, key: KeyEvent) -> Option<ShellRequest>;

    /// A message sent by one of this app's background tasks. The payload is
    /// whatever the app's own tasks sent; downcast and ignore foreign types.
    fn on_event(&mut self, payload: Box<dyn Any + Send>);

    fn on_tick(&mut self) {}

    /// Called once before the program exits; stop background work here.
    /// Return true to ask the shell for a grace period so shutdown work
    /// (e.g. telling mpv to quit, final playback report) can flush.
    fn on_quit(&mut self) -> bool {
        false
    }

    /// Polled during the shutdown grace period; return false while shutdown
    /// work started by `on_quit` is still in flight. The shell keeps routing
    /// events (so completion messages still arrive) until every app reports
    /// true or the grace deadline passes.
    fn ready_to_quit(&self) -> bool {
        true
    }

    /// One-line status shown in the shell status bar even when another app's
    /// tab is active (e.g. now playing).
    fn status_line(&self) -> Option<Line<'static>> {
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect);
}
