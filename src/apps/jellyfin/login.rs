use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::config::JellyfinConfig;
use crate::jellyfin::url::normalize_host;
use crate::ui::input::TextInput;
use crate::ui::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Host,
    Username,
    Password,
}

const FIELDS: [Field; 3] = [Field::Host, Field::Username, Field::Password];

pub enum LoginAction {
    Submit,
}

pub struct LoginForm {
    pub host: TextInput,
    pub username: TextInput,
    pub password: TextInput,
    focus: usize,
    pub error: Option<String>,
    /// Set while an auth attempt is in flight; input is ignored except esc.
    pub busy: bool,
}

impl LoginForm {
    pub fn from_config(config: &JellyfinConfig) -> Self {
        let mut form = Self {
            host: TextInput::with_value(&config.host),
            username: TextInput::with_value(&config.username),
            // Never prefilled: the stored password lives in the OS keyring
            // and is only read by the auto-login task.
            password: TextInput::default(),
            focus: 0,
            error: None,
            busy: false,
        };
        form.password.masked = true;
        form.host.focused = true;
        form
    }

    fn host_error(&self) -> Option<String> {
        if self.host.value().is_empty() {
            return None;
        }
        normalize_host(self.host.value())
            .err()
            .map(|e| e.to_string())
    }

    /// Plain http means the password and token cross the network unencrypted.
    /// Legitimate on a trusted LAN, but worth a visible nudge.
    fn plain_http(&self) -> bool {
        self.host
            .value()
            .trim()
            .to_ascii_lowercase()
            .starts_with("http://")
    }

    fn valid(&self) -> bool {
        !self.host.value().is_empty()
            && self.host_error().is_none()
            && !self.username.value().is_empty()
    }

    fn focused_input(&mut self) -> &mut TextInput {
        match FIELDS[self.focus] {
            Field::Host => &mut self.host,
            Field::Username => &mut self.username,
            Field::Password => &mut self.password,
        }
    }

    fn set_focus(&mut self, focus: usize) {
        self.focus = focus;
        self.host.focused = false;
        self.username.focused = false;
        self.password.focused = false;
        self.focused_input().focused = true;
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<LoginAction> {
        if self.busy {
            return None;
        }
        match key.code {
            KeyCode::Enter => {
                if self.focus == FIELDS.len() - 1 && self.valid() {
                    return Some(LoginAction::Submit);
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
            Constraint::Length(1), // host error
            Constraint::Length(1), // username
            Constraint::Length(1),
            Constraint::Length(1), // password
            Constraint::Length(1),
            Constraint::Length(1), // status/error
        ])
        .flex(Flex::Center)
        .split(content);

        let buf = frame.buffer_mut();
        Line::from(Span::styled(
            " isamedia ",
            Style::new().bg(theme::ACCENT).fg(theme::FG),
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
            // Short enough to fit the half-width form box on 80 columns.
            Line::styled(" http:// is unencrypted; prefer https://", theme::dim())
                .render(rows[3], buf);
        }

        let (label_area, input_area) = field_area(rows[4]);
        label("Username").render(label_area, buf);
        self.username.render(input_area, buf);

        let (label_area, input_area) = field_area(rows[6]);
        label("Password").render(label_area, buf);
        self.password.render(input_area, buf);

        if self.busy {
            Line::styled(" authenticating...", theme::dim()).render(rows[8], buf);
        } else if let Some(err) = &self.error {
            Line::styled(format!(" {err}"), theme::error()).render(rows[8], buf);
        } else {
            Line::styled(" enter: next/submit  tab: next field", theme::dim()).render(rows[8], buf);
        }
    }
}
