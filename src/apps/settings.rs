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
use crate::event::AppSender;
use crate::ui::input::TextInput;
use crate::ui::theme::{self, Theme};

/// Which setting a row edits. Theme/Accent open a choice list; the backend
/// rows open a URL + credentials form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setting {
    Theme,
    Accent,
    Jellyfin,
    Radarr,
    Sonarr,
}

/// Open editor state for a choice list (Theme/Accent): which setting, and the
/// cursor within its choices.
struct Editing {
    setting: Setting,
    cursor: usize,
}

/// The overlay currently open on top of the settings list. Choice (Theme/
/// Accent) and Backend (a credentials form) are mutually exclusive.
#[allow(clippy::large_enum_variant)] // one instance per app; boxing buys nothing
enum Editor {
    None,
    Choice(Editing),
    Backend(BackendForm),
}

/// Result of a submitted backend form, reported back from the connect task.
enum Msg {
    SaveDone {
        save_gen: u64,
        result: Result<(), String>,
    },
}

/// A settings screen. It mutates the global palette and persists choices to the
/// config file synchronously; the backend forms additionally connect to a
/// server and write to the OS keyring, which is why it now holds an
/// [`AppSender`] to report those async results back into `on_event`.
pub struct SettingsApp {
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    sender: AppSender,
    /// Cursor over the visible setting rows.
    cursor: usize,
    /// The open overlay, if any (modal within the tab).
    editor: Editor,
    /// Bumped on each backend submit and on cancel, so a connect result that
    /// lands after the user moved on is dropped (the generation-counter
    /// convention every async path in this app follows).
    save_gen: u64,
}

impl SettingsApp {
    pub fn new(config: Arc<Mutex<Config>>, config_path: PathBuf, sender: AppSender) -> Self {
        Self {
            config,
            config_path,
            sender,
            cursor: 0,
            editor: Editor::None,
            save_gen: 0,
        }
    }

    /// The settings rows currently shown. Accent only appears when the active
    /// theme offers accents; the backend rows are always shown.
    fn rows(&self) -> Vec<Setting> {
        let mut rows = vec![Setting::Theme];
        if !theme::active_theme().accents().is_empty() {
            rows.push(Setting::Accent);
        }
        rows.extend([Setting::Jellyfin, Setting::Radarr, Setting::Sonarr]);
        rows
    }

    fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
        }
    }

    /// Open the editor for the selected row: a choice list for Theme/Accent, a
    /// credentials form for a backend.
    fn open(&mut self, setting: Setting) {
        match setting {
            Setting::Theme | Setting::Accent => {
                self.editor = Editor::Choice(Editing {
                    setting,
                    cursor: current_choice_index(setting),
                });
            }
            Setting::Jellyfin | Setting::Radarr | Setting::Sonarr => {
                let form = BackendForm::from_config(setting, &self.config.lock().unwrap());
                self.editor = Editor::Backend(form);
            }
        }
    }

    /// Apply the chosen value for a choice-list setting: update the live palette
    /// and persist. Never called for a backend row (those open a form).
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
            Setting::Jellyfin | Setting::Radarr | Setting::Sonarr => {}
        }
    }

    /// Handle a key while a choice list (Theme/Accent) is open.
    fn on_choice_key(&mut self, key: KeyEvent) {
        let Editor::Choice(editing) = &self.editor else {
            return;
        };
        let setting = editing.setting;
        let cursor = editing.cursor;
        let count = choice_count(setting);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                self.editor = Editor::Choice(Editing {
                    setting,
                    cursor: (cursor + count - 1) % count,
                });
            }
            KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                self.editor = Editor::Choice(Editing {
                    setting,
                    cursor: (cursor + 1) % count,
                });
            }
            KeyCode::Enter => {
                self.apply(setting, cursor);
                self.editor = Editor::None;
                self.clamp_cursor();
            }
            KeyCode::Esc => self.editor = Editor::None,
            _ => {}
        }
    }

    /// Kick off a save for the open backend form: connect to the server to
    /// validate the URL + credential, and only persist on success. Everything
    /// (connect, config write, keyring write) runs off the render thread and
    /// reports back one `Msg::SaveDone`.
    fn submit_backend(&mut self) {
        let Editor::Backend(form) = &mut self.editor else {
            return;
        };
        form.busy = true;
        form.error = None;

        // The Jellyfin auth header needs the device identity; snapshot it here
        // so the task never locks the config just to read it.
        let (device, device_id) = {
            let config = self.config.lock().unwrap();
            (
                config.jellyfin.device.clone(),
                config.jellyfin.device_id.clone(),
            )
        };
        let request = SaveRequest {
            backend: form.backend,
            host: form.host.value().to_string(),
            username: form.username.as_ref().map(|u| u.value().to_string()),
            typed_secret: form.secret.value().to_string(),
            device,
            device_id,
        };

        self.save_gen += 1;
        let save_gen = self.save_gen;
        let sender = self.sender.clone();
        let config = self.config.clone();
        let config_path = self.config_path.clone();
        tokio::spawn(async move {
            let result = validate_and_persist(request, config, config_path).await;
            sender.send(Msg::SaveDone { save_gen, result });
        });
    }

    fn draw_list(&self, area: Rect, buf: &mut Buffer) {
        let rows = self.rows();
        {
            let config = self.config.lock().unwrap();
            for (i, &setting) in rows.iter().enumerate() {
                if (i as u16) < area.height {
                    setting_row(setting, i == self.cursor, backend_host(setting, &config))
                        .render(Rect::new(area.x, area.y + i as u16, area.width, 1), buf);
                }
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

/// What a submitted backend form should do, decided before the borrow on the
/// form ends so the follow-up mutation of `self` is legal.
enum BackendOutcome {
    Ignore,
    Submit,
    Cancel,
}

/// The configured host for a backend row, or `None` for Theme/Accent.
fn backend_host(setting: Setting, config: &Config) -> Option<&str> {
    match setting {
        Setting::Jellyfin => Some(&config.jellyfin.host),
        Setting::Radarr => Some(&config.radarr.host),
        Setting::Sonarr => Some(&config.sonarr.host),
        Setting::Theme | Setting::Accent => None,
    }
}

/// The display label for a backend row.
fn backend_label(setting: Setting) -> &'static str {
    match setting {
        Setting::Jellyfin => "Jellyfin",
        Setting::Radarr => "Radarr",
        Setting::Sonarr => "Sonarr",
        Setting::Theme | Setting::Accent => "",
    }
}

/// The keyring key holding a backend's stored secret.
fn secret_key(backend: Setting) -> &'static str {
    match backend {
        Setting::Jellyfin => crate::secrets::JELLYFIN_PASSWORD,
        Setting::Radarr => crate::secrets::RADARR_API_KEY,
        Setting::Sonarr => crate::secrets::SONARR_API_KEY,
        Setting::Theme | Setting::Accent => "",
    }
}

/// A snapshot of a backend form's inputs, taken on the render thread and moved
/// into the connect task.
struct SaveRequest {
    backend: Setting,
    host: String,
    username: Option<String>,
    typed_secret: String,
    device: String,
    device_id: String,
}

/// Connect to the backend to validate the URL + credential, then persist the
/// host to the config file and the secret to the OS keyring. Persists nothing
/// on a failed connection. Runs entirely off the render thread.
async fn validate_and_persist(
    request: SaveRequest,
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
) -> Result<(), String> {
    let SaveRequest {
        backend,
        host,
        username,
        typed_secret,
        device,
        device_id,
    } = request;
    // Fall back to the stored secret when the field was left blank, so the user
    // can change only the host without re-typing a credential.
    let secret = if !typed_secret.is_empty() {
        typed_secret.clone()
    } else {
        let key = secret_key(backend);
        tokio::task::spawn_blocking(move || crate::secrets::get(key))
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    };
    if secret.is_empty() {
        return Err(match backend {
            Setting::Jellyfin => "enter the password".into(),
            _ => "enter the API key".into(),
        });
    }

    match backend {
        Setting::Jellyfin => {
            let creds = crate::jellyfin::Credentials {
                host: host.clone(),
                username: username.clone().unwrap_or_default(),
                password: secret.clone(),
                device,
                device_id,
                version: env!("CARGO_PKG_VERSION").to_string(),
                // Empty token/user_id force a fresh AuthenticateByName, so we
                // validate the actual host/username/password.
                token: String::new(),
                user_id: String::new(),
            };
            let client = crate::jellyfin::Client::connect(creds)
                .await
                .map_err(|err| err.to_string())?;
            {
                let mut config = config.lock().unwrap();
                config.jellyfin.host = host;
                if let Some(username) = username {
                    config.jellyfin.username = username;
                }
                config.jellyfin.user_id = client.user_id.clone();
                config.save(&config_path).map_err(|err| err.to_string())?;
            }
            // Store the fresh token (so the next launch reuses the session) and
            // the password only when the user typed a new one.
            let token = client.token.clone();
            let store_password = !typed_secret.is_empty();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                crate::secrets::set(crate::secrets::JELLYFIN_TOKEN, &token)
                    .map_err(|err| err.to_string())?;
                if store_password {
                    crate::secrets::set(crate::secrets::JELLYFIN_PASSWORD, &secret)
                        .map_err(|err| err.to_string())?;
                }
                Ok(())
            })
            .await
            .map_err(|err| err.to_string())??;
        }
        Setting::Radarr | Setting::Sonarr => {
            let normalized_host = if backend == Setting::Radarr {
                crate::radarr::Client::connect(&host, secret.clone())
                    .await
                    .map_err(|err| err.to_string())?
                    .host()
                    .to_string()
            } else {
                crate::sonarr::Client::connect(&host, secret.clone())
                    .await
                    .map_err(|err| err.to_string())?
                    .host()
                    .to_string()
            };
            {
                let mut config = config.lock().unwrap();
                match backend {
                    Setting::Radarr => config.radarr.host = normalized_host,
                    Setting::Sonarr => config.sonarr.host = normalized_host,
                    _ => {}
                }
                config.save(&config_path).map_err(|err| err.to_string())?;
            }
            if !typed_secret.is_empty() {
                let key = secret_key(backend);
                tokio::task::spawn_blocking(move || crate::secrets::set(key, &secret))
                    .await
                    .map_err(|err| err.to_string())?
                    .map_err(|err| err.to_string())?;
            }
        }
        Setting::Theme | Setting::Accent => return Err("not a backend".into()),
    }
    Ok(())
}

/// A settings row: `> Theme    Catppuccin Latte` (Accent adds a colour swatch,
/// a backend shows its configured host).
fn setting_row(setting: Setting, selected: bool, host: Option<&str>) -> Line<'static> {
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
        Setting::Jellyfin | Setting::Radarr | Setting::Sonarr => {
            spans.push(Span::styled(
                format!("{:<9}", backend_label(setting)),
                label_style,
            ));
            let host = host.unwrap_or("");
            let summary = if host.is_empty() {
                "not configured".to_string()
            } else {
                host.to_string()
            };
            spans.push(Span::styled(summary, theme::dim()));
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
        // Backends never open a choice list.
        Setting::Jellyfin | Setting::Radarr | Setting::Sonarr => ("", Vec::new()),
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

/// Number of choices in a choice-list setting.
fn choice_count(setting: Setting) -> usize {
    match setting {
        Setting::Theme => Theme::ALL.len(),
        Setting::Accent => theme::active_theme().accents().len(),
        Setting::Jellyfin | Setting::Radarr | Setting::Sonarr => 0,
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
        Setting::Jellyfin | Setting::Radarr | Setting::Sonarr => 0,
    }
}

/// Draw a `label + input` row, guarded so it never overruns a short area.
fn draw_field(area: Rect, y: u16, label: &str, input: &TextInput, buf: &mut Buffer) {
    if y >= area.y + area.height {
        return;
    }
    let row = Rect::new(area.x, y, area.width, 1);
    let [label_area, input_area] =
        Layout::horizontal([Constraint::Length(11), Constraint::Fill(1)]).areas(row);
    Line::from(Span::styled(format!(" {label:<9}"), theme::selected())).render(label_area, buf);
    input.render(input_area, buf);
}

/// Which action a submitted backend form is asking for.
enum FormAction {
    Submit,
}

/// A URL + credentials form for one backend, opened from its settings row.
/// Modelled on the per-app setup forms, but rendered inline in the Settings
/// body and validated against the server on save. The username row is present
/// only for Jellyfin.
struct BackendForm {
    backend: Setting,
    host: TextInput,
    username: Option<TextInput>,
    /// Masked: the Jellyfin password or the *arr API key.
    secret: TextInput,
    focus: usize,
    error: Option<String>,
    /// Set while a connect attempt is in flight; typing is ignored.
    busy: bool,
}

impl BackendForm {
    fn from_config(backend: Setting, config: &Config) -> BackendForm {
        let (host, username) = match backend {
            Setting::Jellyfin => (
                config.jellyfin.host.clone(),
                Some(TextInput::with_value(&config.jellyfin.username)),
            ),
            Setting::Radarr => (config.radarr.host.clone(), None),
            Setting::Sonarr => (config.sonarr.host.clone(), None),
            Setting::Theme | Setting::Accent => (String::new(), None),
        };
        let mut secret = TextInput::default();
        // Never prefilled: the stored secret lives in the OS keyring and is
        // only read by the connect task when the field is left blank.
        secret.masked = true;
        let mut form = BackendForm {
            backend,
            host: TextInput::with_value(&host),
            username,
            secret,
            focus: 0,
            error: None,
            busy: false,
        };
        form.host.focused = true;
        form
    }

    /// The number of focusable fields: 3 for Jellyfin (with username), else 2.
    fn fields(&self) -> usize {
        if self.username.is_some() { 3 } else { 2 }
    }

    fn host_error(&self) -> Option<String> {
        if self.host.value().is_empty() {
            return None;
        }
        crate::net::normalize_host(self.host.value())
            .err()
            .map(|err| err.to_string())
    }

    /// Plain http means the credential crosses the network unencrypted.
    /// Legitimate on a trusted LAN, but worth a visible nudge.
    fn plain_http(&self) -> bool {
        crate::net::is_plain_http(self.host.value())
    }

    /// The secret may be left blank (reuse the stored one), so only the host
    /// and, for Jellyfin, the username are required.
    fn valid(&self) -> bool {
        !self.host.value().is_empty()
            && self.host_error().is_none()
            && self
                .username
                .as_ref()
                .map(|u| !u.value().is_empty())
                .unwrap_or(true)
    }

    fn secret_label(&self) -> &'static str {
        match self.backend {
            Setting::Jellyfin => "Password",
            _ => "API key",
        }
    }

    fn focused_input(&mut self) -> &mut TextInput {
        if self.focus == 0 {
            &mut self.host
        } else if self.focus == 1 && self.username.is_some() {
            self.username.as_mut().expect("username present at focus 1")
        } else {
            &mut self.secret
        }
    }

    fn set_focus(&mut self, focus: usize) {
        self.focus = focus;
        self.host.focused = false;
        if let Some(username) = self.username.as_mut() {
            username.focused = false;
        }
        self.secret.focused = false;
        self.focused_input().focused = true;
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<FormAction> {
        if self.busy {
            return None;
        }
        let fields = self.fields();
        match key.code {
            KeyCode::Enter => {
                if self.focus == fields - 1 && self.valid() {
                    return Some(FormAction::Submit);
                }
                self.set_focus((self.focus + 1) % fields);
            }
            KeyCode::Tab | KeyCode::Down => {
                self.set_focus((self.focus + 1) % fields);
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.set_focus((self.focus + fields - 1) % fields);
            }
            _ => {
                self.focused_input().on_key(key);
            }
        }
        None
    }

    fn draw(&self, area: Rect, buf: &mut Buffer) {
        let bottom = area.y + area.height;
        let title = backend_label(self.backend);

        // Header.
        if area.y < bottom {
            Line::styled(format!("  {title}"), theme::selected())
                .render(Rect::new(area.x, area.y, area.width, 1), buf);
        }

        // Host, then its validation nudge one row below.
        let host_y = area.y + 2;
        draw_field(area, host_y, "Host", &self.host, buf);
        let nudge_y = host_y + 1;
        if nudge_y < bottom {
            if let Some(err) = self.host_error() {
                Line::styled(format!(" {err}"), theme::dim())
                    .render(Rect::new(area.x, nudge_y, area.width, 1), buf);
            } else if self.plain_http() {
                Line::styled(" http:// is unencrypted; prefer https://", theme::dim())
                    .render(Rect::new(area.x, nudge_y, area.width, 1), buf);
            }
        }

        // Username (Jellyfin only), then the masked secret.
        let mut y = nudge_y + 1;
        if let Some(username) = &self.username {
            draw_field(area, y, "Username", username, buf);
            y += 1;
        }
        draw_field(area, y, self.secret_label(), &self.secret, buf);

        // Status line, one blank row below the last field.
        let status_y = y + 2;
        if status_y < bottom {
            let line = if self.busy {
                Line::styled(" connecting...", theme::dim())
            } else if let Some(err) = &self.error {
                Line::styled(format!(" {err}"), theme::error())
            } else {
                Line::styled(" enter: save   esc: back", theme::dim())
            };
            line.render(Rect::new(area.x, status_y, area.width, 1), buf);
        }
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
        // Choice list (Theme/Accent): modal navigate/apply/cancel.
        if let Editor::Choice(_) = &self.editor {
            self.on_choice_key(key);
            return None;
        }

        // Backend credentials form: decide the outcome while the form is
        // borrowed, then act once the borrow has ended.
        if let Editor::Backend(form) = &mut self.editor {
            let outcome = if key.code == KeyCode::Esc {
                // Esc cancels even mid-connect; the in-flight result is dropped
                // by the save_gen guard in on_event.
                BackendOutcome::Cancel
            } else if let Some(FormAction::Submit) = form.on_key(key) {
                BackendOutcome::Submit
            } else {
                BackendOutcome::Ignore
            };
            match outcome {
                BackendOutcome::Cancel => {
                    self.save_gen += 1;
                    self.editor = Editor::None;
                }
                BackendOutcome::Submit => self.submit_backend(),
                BackendOutcome::Ignore => {}
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
                    self.open(setting);
                }
            }
            KeyCode::Char('q') if key.modifiers.is_empty() => return Some(ShellRequest::Quit),
            _ => {}
        }
        None
    }

    fn on_event(&mut self, payload: Box<dyn Any + Send>) {
        let Ok(msg) = payload.downcast::<Msg>() else {
            return;
        };
        let Msg::SaveDone { save_gen, result } = *msg;
        if save_gen != self.save_gen {
            return; // superseded or cancelled
        }
        match result {
            Ok(()) => {
                self.editor = Editor::None;
                self.clamp_cursor();
            }
            Err(message) => {
                if let Editor::Backend(form) = &mut self.editor {
                    form.busy = false;
                    form.error = Some(message);
                }
            }
        }
    }

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
        match &self.editor {
            Editor::None => self.draw_list(body, buf),
            Editor::Choice(editing) => draw_editor(editing, body, buf),
            Editor::Backend(form) => form.draw(body, buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn app() -> SettingsApp {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SettingsApp::new(
            Arc::new(Mutex::new(Config::default())),
            PathBuf::from("settings-test-config.toml"),
            AppSender::new("settings", tx),
        )
    }

    /// The whole rendered screen as text, for content assertions.
    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn draw_does_not_panic_on_narrow_terminals() {
        let mut settings = app();
        for (width, height) in [(10, 5), (1, 1), (0, 0), (24, 3), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            // The settings list.
            terminal.draw(|f| settings.draw(f, f.area())).unwrap();
            // The choice-list editors.
            for setting in [Setting::Theme, Setting::Accent] {
                settings.editor = Editor::Choice(Editing { setting, cursor: 0 });
                terminal.draw(|f| settings.draw(f, f.area())).unwrap();
            }
            // The backend credentials forms (host summary uses a placeholder).
            for backend in [Setting::Jellyfin, Setting::Radarr, Setting::Sonarr] {
                settings.editor =
                    Editor::Backend(BackendForm::from_config(backend, &Config::default()));
                terminal.draw(|f| settings.draw(f, f.area())).unwrap();
            }
            settings.editor = Editor::None;
        }
    }

    #[test]
    fn list_shows_backend_rows() {
        let mut settings = app();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| settings.draw(f, f.area())).unwrap();
        let text = screen(&terminal);
        for label in ["Jellyfin", "Radarr", "Sonarr"] {
            assert!(text.contains(label), "missing {label} row in:\n{text}");
        }
        // An empty config host reads as unconfigured.
        assert!(text.contains("not configured"), "{text}");
    }

    #[test]
    fn configured_host_is_shown() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = Config::default();
        config.radarr.host = "https://radarr.example".into();
        let mut settings = SettingsApp::new(
            Arc::new(Mutex::new(config)),
            PathBuf::from("settings-test-config.toml"),
            AppSender::new("settings", tx),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| settings.draw(f, f.area())).unwrap();
        assert!(screen(&terminal).contains("https://radarr.example"));
    }

    #[test]
    fn opening_a_backend_shows_the_form_and_esc_cancels() {
        let mut settings = app();
        settings.open(Setting::Jellyfin);
        assert!(matches!(settings.editor, Editor::Backend(_)));

        // The Jellyfin form has the username row; the *arr forms do not.
        if let Editor::Backend(form) = &settings.editor {
            assert_eq!(form.fields(), 3);
            assert!(form.secret.masked);
        }
        settings.open(Setting::Radarr);
        if let Editor::Backend(form) = &settings.editor {
            assert_eq!(form.fields(), 2);
            assert_eq!(form.secret_label(), "API key");
        }

        settings.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(settings.editor, Editor::None));
    }
}
