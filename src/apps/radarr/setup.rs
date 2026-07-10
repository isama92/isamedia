//! First-run / re-auth form: server host plus API key. A thin facade over the
//! shared [`Form`] widget (a single Connect button, no Cancel — there is
//! nowhere to cancel back to on the entry screen). Deliberate duplicate of the
//! Sonarr setup form — the app layer is never generalised.

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::config::RadarrConfig;
use crate::ui::form::{Field, Form, FormEvent};
use crate::ui::theme;

/// Stable field ids; values are read by id.
mod field {
    use crate::ui::form::FieldId;
    pub const HOST: FieldId = 0;
    pub const API_KEY: FieldId = 1;
}

pub enum SetupAction {
    Submit,
}

pub struct SetupForm {
    form: Form,
    pub error: Option<String>,
    /// Set while a connect attempt is in flight; input is ignored.
    pub busy: bool,
}

impl SetupForm {
    pub fn from_config(config: &RadarrConfig) -> Self {
        let fields = vec![
            Field::text(field::HOST, "Host", config.host.clone()),
            // Never prefilled: the stored key lives in the OS keyring and is
            // only read by the auto-connect task.
            Field::text(field::API_KEY, "API key", "").masked(),
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

    pub fn api_key(&self) -> String {
        self.form.text(field::API_KEY)
    }

    fn host_error(&self) -> Option<String> {
        let host = self.host();
        if host.is_empty() {
            return None;
        }
        crate::net::normalize_host(&host)
            .err()
            .map(|err| err.to_string())
    }

    /// Plain http means the API key crosses the network unencrypted.
    /// Legitimate on a trusted LAN, but worth a visible nudge.
    fn plain_http(&self) -> bool {
        crate::net::is_plain_http(&self.host())
    }

    /// Why the form can't be submitted yet, or `None` when it's ready. Unlike
    /// the Settings form, the API key is required here (nothing is stored yet).
    fn invalid_reason(&self) -> Option<String> {
        if self.host().is_empty() {
            return Some("enter the host".into());
        }
        if let Some(err) = self.host_error() {
            return Some(err);
        }
        if self.api_key().is_empty() {
            return Some("enter the API key".into());
        }
        None
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<SetupAction> {
        if self.busy {
            return None;
        }
        match self.form.on_key(key) {
            FormEvent::Save => {
                if let Some(reason) = self.invalid_reason() {
                    self.error = Some(reason);
                    None
                } else {
                    Some(SetupAction::Submit)
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
        // The form (title + 2 fields + Connect) plus a status/nudge row below,
        // centred vertically like the pre-widget card.
        let [form_area, status_area] =
            Layout::vertical([Constraint::Length(6), Constraint::Length(2)])
                .flex(Flex::Center)
                .areas(content);

        self.form.draw(frame, form_area, "isamedia · radarr");

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
            Line::styled(" connecting...", theme::dim())
        } else if let Some(err) = &self.error {
            Line::styled(format!(" {err}"), theme::error())
        } else if let Some(err) = self.host_error() {
            Line::styled(format!(" {err}"), theme::dim())
        } else if self.plain_http() {
            Line::styled(" http:// is unencrypted; prefer https://", theme::dim())
        } else {
            Line::styled(" enter: connect   tab: next field", theme::dim())
        };
        line.render(row, frame.buffer_mut());
    }
}
