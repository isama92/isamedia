//! First-run / re-auth form: host, username and password. A thin facade over
//! the shared [`Form`] widget (a single Connect button, no Cancel — there is
//! nowhere to cancel back to on the entry screen).

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::config::JellyfinConfig;
use crate::jellyfin::url::normalize_host;
use crate::ui::form::{Field, Form, FormEvent};
use crate::ui::theme;

/// Stable field ids; values are read by id.
mod field {
    use crate::ui::form::FieldId;
    pub const HOST: FieldId = 0;
    pub const USERNAME: FieldId = 1;
    pub const PASSWORD: FieldId = 2;
}

pub enum LoginAction {
    Submit,
    /// Esc pressed while an auth attempt is in flight: abandon it (the app
    /// bumps `auth_gen` so the pending result is dropped) instead of leaving
    /// the form locked until the request timeout.
    Cancel,
}

pub struct LoginForm {
    form: Form,
    pub error: Option<String>,
    /// Set while an auth attempt is in flight; input is ignored except esc.
    pub busy: bool,
}

impl LoginForm {
    pub fn from_config(config: &JellyfinConfig) -> Self {
        let fields = vec![
            Field::text(field::HOST, "Host", config.host.clone()),
            Field::text(field::USERNAME, "Username", config.username.clone()),
            // Never prefilled: the stored password lives in the OS keyring and
            // is only read by the auto-login task.
            Field::text(field::PASSWORD, "Password", "").masked(),
        ];
        Self {
            // A single Connect button: an entry screen has nowhere to cancel to.
            form: Form::new(fields, "Connect").without_cancel(),
            error: None,
            busy: false,
        }
    }

    pub fn host(&self) -> String {
        self.form.text(field::HOST)
    }

    pub fn username(&self) -> String {
        self.form.text(field::USERNAME)
    }

    pub fn password(&self) -> String {
        self.form.text(field::PASSWORD)
    }

    /// True while a field (not the action row) has focus, so the shell knows a
    /// keystroke is text input and not a global shortcut.
    pub fn action_row_focused(&self) -> bool {
        self.form.action_row_focused()
    }

    fn host_error(&self) -> Option<String> {
        let host = self.host();
        if host.is_empty() {
            return None;
        }
        normalize_host(&host).err().map(|e| e.to_string())
    }

    /// Plain http means the password and token cross the network unencrypted.
    /// Legitimate on a trusted LAN, but worth a visible nudge.
    fn plain_http(&self) -> bool {
        crate::jellyfin::url::is_plain_http(&self.host())
    }

    /// Why the form can't be submitted yet, or `None` when it's ready. The
    /// password may be left blank — an empty one means "don't keep a stored
    /// password" (see `mod.rs`), so only host and username are required.
    fn invalid_reason(&self) -> Option<String> {
        if self.host().is_empty() {
            return Some("enter the host".into());
        }
        if let Some(err) = self.host_error() {
            return Some(err);
        }
        if self.username().is_empty() {
            return Some("enter the username".into());
        }
        None
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<LoginAction> {
        if self.busy {
            // A submit against a black-holed host would otherwise lock the form
            // until the request timeout; Esc abandons the attempt and returns
            // control. Every other key stays ignored while busy.
            return (key.code == KeyCode::Esc).then_some(LoginAction::Cancel);
        }
        match self.form.on_key(key) {
            FormEvent::Save => {
                if let Some(reason) = self.invalid_reason() {
                    self.error = Some(reason);
                    None
                } else {
                    Some(LoginAction::Submit)
                }
            }
            // Editing a field clears a stale validation error.
            FormEvent::Changed(_) => {
                self.error = None;
                None
            }
            // Esc has nowhere to go on the entry screen; ignore it (matching
            // the pre-widget behaviour).
            FormEvent::Cancel | FormEvent::Consumed => None,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let width = (area.width / 2 + 10).min(area.width);
        let [content] = Layout::horizontal([Constraint::Length(width)])
            .flex(Flex::Center)
            .areas(area);
        // The form (title + 3 fields + Connect) plus a status/nudge row below,
        // centred vertically like the pre-widget card.
        let [form_area, status_area] =
            Layout::vertical([Constraint::Length(7), Constraint::Length(2)])
                .flex(Flex::Center)
                .areas(content);

        self.form.draw(frame, form_area, "isamedia");

        if status_area.height == 0 {
            return;
        }
        let row = Rect::new(
            status_area.x,
            status_area.y + status_area.height - 1,
            status_area.width,
            1,
        );
        let line = if self.busy {
            Line::styled(" authenticating...", theme::dim())
        } else if let Some(err) = &self.error {
            Line::styled(format!(" {err}"), theme::error())
        } else if let Some(err) = self.host_error() {
            Line::styled(format!(" {err}"), theme::dim())
        } else if self.plain_http() {
            // Short enough to fit the half-width form box on 80 columns.
            Line::styled(" http:// is unencrypted; prefer https://", theme::dim())
        } else {
            Line::styled(" enter: connect   tab: next field", theme::dim())
        };
        line.render(row, frame.buffer_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> LoginForm {
        LoginForm::from_config(&JellyfinConfig::default())
    }

    #[test]
    fn esc_while_busy_cancels_the_attempt() {
        let mut form = form();
        form.busy = true;
        assert!(matches!(
            form.on_key(KeyEvent::from(KeyCode::Esc)),
            Some(LoginAction::Cancel)
        ));
    }

    #[test]
    fn other_keys_are_ignored_while_busy() {
        let mut form = form();
        form.busy = true;
        assert!(form.on_key(KeyEvent::from(KeyCode::Char('a'))).is_none());
        assert!(form.on_key(KeyEvent::from(KeyCode::Enter)).is_none());
    }

    #[test]
    fn esc_is_ignored_when_not_busy() {
        // The entry screen has nowhere to cancel back to, so Esc is a no-op
        // unless an attempt is in flight.
        let mut form = form();
        assert!(form.on_key(KeyEvent::from(KeyCode::Esc)).is_none());
    }
}
