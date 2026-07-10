//! First-run / re-auth form: server host plus API key. Modelled on the
//! Jellyfin login form, two fields instead of three.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::config::SonarrConfig;
use crate::ui::input::TextInput;
use crate::ui::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Host,
    ApiKey,
}

const FIELDS: [Field; 2] = [Field::Host, Field::ApiKey];

pub enum SetupAction {
    Submit,
}

pub struct SetupForm {
    pub host: TextInput,
    pub api_key: TextInput,
    focus: usize,
    pub error: Option<String>,
    /// Set while a connect attempt is in flight; input is ignored.
    pub busy: bool,
}

impl SetupForm {
    pub fn from_config(config: &SonarrConfig) -> Self {
        let mut form = Self {
            host: TextInput::with_value(&config.host),
            // Never prefilled: the stored key lives in the OS keyring and is
            // only read by the auto-connect task.
            api_key: TextInput::default(),
            focus: 0,
            error: None,
            busy: false,
        };
        form.api_key.masked = true;
        form.host.focused = true;
        form
    }

    fn host_error(&self) -> Option<String> {
        if self.host.value().is_empty() {
            return None;
        }
        crate::net::normalize_host(self.host.value())
            .err()
            .map(|err| err.to_string())
    }

    /// Plain http means the API key crosses the network unencrypted.
    /// Legitimate on a trusted LAN, but worth a visible nudge.
    fn plain_http(&self) -> bool {
        crate::net::is_plain_http(self.host.value())
    }

    fn valid(&self) -> bool {
        !self.host.value().is_empty()
            && self.host_error().is_none()
            && !self.api_key.value().is_empty()
    }

    fn focused_input(&mut self) -> &mut TextInput {
        match FIELDS[self.focus] {
            Field::Host => &mut self.host,
            Field::ApiKey => &mut self.api_key,
        }
    }

    fn set_focus(&mut self, focus: usize) {
        self.focus = focus;
        self.host.focused = false;
        self.api_key.focused = false;
        self.focused_input().focused = true;
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<SetupAction> {
        if self.busy {
            return None;
        }
        match key.code {
            KeyCode::Enter => {
                if self.focus == FIELDS.len() - 1 && self.valid() {
                    return Some(SetupAction::Submit);
                }
                self.set_focus((self.focus + 1) % FIELDS.len());
            }
            KeyCode::Tab | KeyCode::Down => {
                self.set_focus((self.focus + 1) % FIELDS.len());
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.set_focus((self.focus + FIELDS.len() - 1) % FIELDS.len());
            }
            _ => {
                self.focused_input().on_key(key);
            }
        }
        None
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let width = (area.width / 2 + 10).min(area.width);
        let [content] = Layout::horizontal([Constraint::Length(width)])
            .flex(Flex::Center)
            .areas(area);
        let rows = Layout::vertical([
            Constraint::Length(1), // title
            Constraint::Length(1),
            Constraint::Length(1), // host
            Constraint::Length(1), // host error / plain-http nudge
            Constraint::Length(1), // api key
            Constraint::Length(1),
            Constraint::Length(1), // status/error
        ])
        .flex(Flex::Center)
        .split(content);

        let buf = frame.buffer_mut();
        Line::from(Span::styled(
            " isamedia · sonarr ",
            Style::new()
                .bg(theme::accent_color())
                .fg(theme::on_accent()),
        ))
        .render(rows[0], buf);

        let label = |text: &'static str| Span::styled(format!(" {text:<9}"), theme::selected());

        let field_area = |row: Rect| {
            let [label_area, input_area] =
                Layout::horizontal([Constraint::Length(11), Constraint::Fill(1)]).areas(row);
            (label_area, input_area)
        };

        let (label_area, input_area) = field_area(rows[2]);
        label("Host").render(label_area, buf);
        self.host.render(input_area, buf);
        if let Some(err) = self.host_error() {
            Line::styled(format!(" {err}"), theme::dim()).render(rows[3], buf);
        } else if self.plain_http() {
            Line::styled(" http:// is unencrypted; prefer https://", theme::dim())
                .render(rows[3], buf);
        }

        let (label_area, input_area) = field_area(rows[4]);
        label("API key").render(label_area, buf);
        self.api_key.render(input_area, buf);

        if self.busy {
            Line::styled(" connecting...", theme::dim()).render(rows[6], buf);
        } else if let Some(err) = &self.error {
            Line::styled(format!(" {err}"), theme::error()).render(rows[6], buf);
        } else {
            Line::styled(" enter: next/submit  tab: next field", theme::dim()).render(rows[6], buf);
        }
    }
}
