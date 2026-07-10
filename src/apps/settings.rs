use std::any::Any;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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
use crate::ui::form::{Field, Form, FormEvent};
use crate::ui::theme::{self, Theme};

/// Stable field ids for the backend credentials form; values are read by id so
/// the absent Username row (Radarr/Sonarr) never shifts what the others read.
mod field {
    use crate::ui::form::FieldId;
    pub const HOST: FieldId = 0;
    pub const USERNAME: FieldId = 1;
    pub const SECRET: FieldId = 2;
}

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
    Backend(BackendEditor),
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
    /// Bumped after a successful Jellyfin save so the running Jellyfin tab
    /// reconnects with the freshly stored token (see `apps::build_apps`).
    jellyfin_reauth: Arc<AtomicU64>,
}

impl SettingsApp {
    pub fn new(
        config: Arc<Mutex<Config>>,
        config_path: PathBuf,
        sender: AppSender,
        jellyfin_reauth: Arc<AtomicU64>,
    ) -> Self {
        Self {
            config,
            config_path,
            sender,
            cursor: 0,
            editor: Editor::None,
            save_gen: 0,
            jellyfin_reauth,
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
                let editor = BackendEditor::from_config(setting, &self.config.lock().unwrap());
                self.editor = Editor::Backend(editor);
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
            KeyCode::Up if count > 0 => {
                self.editor = Editor::Choice(Editing {
                    setting,
                    cursor: (cursor + count - 1) % count,
                });
            }
            KeyCode::Down if count > 0 => {
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
        let Editor::Backend(editor) = &mut self.editor else {
            return;
        };
        editor.busy = true;
        editor.error = None;
        let backend = editor.backend;

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
            backend,
            host: editor.form.text(field::HOST),
            // Username only exists (and is read) for Jellyfin.
            username: (backend == Setting::Jellyfin).then(|| editor.form.text(field::USERNAME)),
            typed_secret: editor.form.text(field::SECRET),
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
            Line::styled("  enter: change   up/down: move   q: quit", theme::dim()).render(
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
    // The typed credential is authoritative: it is never backfilled from the
    // keyring. A blank field is sent as-is, so the server rejects it with an
    // auth error instead of the save silently re-authenticating with the stored
    // secret (which for Jellyfin would mint a new session token and boot the
    // live tab).
    let secret = typed_secret;

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
            // Store the fresh token (so the next launch reuses the session), and
            // the password unless it was left blank (a passwordless account
            // re-auths on the token alone).
            let token = client.token.clone();
            let store_password = !secret.is_empty();
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
            if !secret.is_empty() {
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

/// The credential's label: the password for Jellyfin, the API key for the *arr.
fn secret_label(backend: Setting) -> &'static str {
    match backend {
        Setting::Jellyfin => "Password",
        _ => "API key",
    }
}

/// The open backend credentials form: the shared [`Form`] widget plus the async
/// state the widget is deliberately ignorant of (a connect is in flight; the
/// last connect failed). Modelled on `radarr::browse`'s active-form wrapper.
struct BackendEditor {
    backend: Setting,
    form: Form,
    error: Option<String>,
    /// Set while a connect attempt is in flight; keys other than Esc are ignored.
    busy: bool,
}

impl BackendEditor {
    fn from_config(backend: Setting, config: &Config) -> BackendEditor {
        let (host, username) = match backend {
            Setting::Jellyfin => (
                config.jellyfin.host.clone(),
                Some(config.jellyfin.username.clone()),
            ),
            Setting::Radarr => (config.radarr.host.clone(), None),
            Setting::Sonarr => (config.sonarr.host.clone(), None),
            Setting::Theme | Setting::Accent => (String::new(), None),
        };
        let mut fields = vec![Field::text(field::HOST, "Host", host)];
        if let Some(username) = username {
            fields.push(Field::text(field::USERNAME, "Username", username));
        }
        // Never prefilled, never backfilled: the typed value is used as-is on
        // every save, so leaving it blank fails auth rather than reusing the
        // stored secret (a bare re-save would otherwise re-authenticate and, for
        // Jellyfin, invalidate the live session token).
        fields.push(Field::text(field::SECRET, secret_label(backend), "").masked());
        BackendEditor {
            backend,
            form: Form::new(fields, "Save"),
            error: None,
            busy: false,
        }
    }

    fn host_value(&self) -> String {
        self.form.text(field::HOST)
    }

    fn host_error(&self) -> Option<String> {
        let host = self.host_value();
        if host.is_empty() {
            return None;
        }
        crate::net::normalize_host(&host)
            .err()
            .map(|err| err.to_string())
    }

    /// Plain http means the credential crosses the network unencrypted.
    /// Legitimate on a trusted LAN, but worth a visible nudge.
    fn plain_http(&self) -> bool {
        crate::net::is_plain_http(&self.host_value())
    }

    /// Why the form can't be submitted yet, or `None` when it's ready. The
    /// secret may be left blank (reuse the stored one), so only the host and,
    /// for Jellyfin, the username are required.
    fn invalid_reason(&self) -> Option<String> {
        if self.host_value().is_empty() {
            return Some("enter the host".into());
        }
        if let Some(err) = self.host_error() {
            return Some(err);
        }
        if self.backend == Setting::Jellyfin && self.form.text(field::USERNAME).is_empty() {
            return Some("enter the username".into());
        }
        None
    }

    /// Draw the form full-body, then a status line below it. This folds the old
    /// inter-field host nudge into a prioritised line under the buttons, since
    /// the `Form` now owns the contiguous field rows.
    fn draw(&self, frame: &mut Frame, area: Rect) {
        // Split first (no buffer borrow), draw the form (needs &mut Frame), THEN
        // take the buffer for the status line — the two frame borrows never overlap.
        let [form_area, status_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(area);
        self.form
            .draw(frame, form_area, backend_label(self.backend));

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
            Line::styled("  connecting...", theme::dim())
        } else if let Some(err) = &self.error {
            Line::styled(format!("  {err}"), theme::error())
        } else if let Some(err) = self.host_error() {
            Line::styled(format!("  {err}"), theme::dim())
        } else if self.plain_http() {
            Line::styled("  http:// is unencrypted; prefer https://", theme::dim())
        } else {
            Line::styled("  enter: save   esc: back", theme::dim())
        };
        line.render(row, frame.buffer_mut());
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
        if let Editor::Backend(editor) = &mut self.editor {
            // Mid-connect: only Esc cancels (the in-flight result is dropped by
            // the save_gen guard in on_event); every other key is ignored.
            if editor.busy {
                if key.code == KeyCode::Esc {
                    self.save_gen += 1;
                    self.editor = Editor::None;
                }
                return None;
            }
            let outcome = match editor.form.on_key(key) {
                // Save gates on validity: an invalid form stays open with the
                // reason shown, rather than firing a doomed connect.
                FormEvent::Save => match editor.invalid_reason() {
                    Some(reason) => {
                        editor.error = Some(reason);
                        BackendOutcome::Ignore
                    }
                    None => BackendOutcome::Submit,
                },
                // Esc or the Cancel button.
                FormEvent::Cancel => BackendOutcome::Cancel,
                // Editing a field clears a stale validation/connect error.
                FormEvent::Changed(_) => {
                    editor.error = None;
                    BackendOutcome::Ignore
                }
                FormEvent::Consumed => BackendOutcome::Ignore,
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
            KeyCode::Up if !rows.is_empty() => {
                self.cursor = (self.cursor + rows.len() - 1) % rows.len();
            }
            KeyCode::Down if !rows.is_empty() => {
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
                // A Jellyfin save minted a fresh token in the keyring; signal
                // the running Jellyfin tab to adopt it instead of dropping to a
                // re-login. (*arr keys are stateless, so they need no signal.)
                if let Editor::Backend(editor) = &self.editor
                    && editor.backend == Setting::Jellyfin
                {
                    self.jellyfin_reauth.fetch_add(1, Ordering::Relaxed);
                }
                self.editor = Editor::None;
                self.clamp_cursor();
            }
            Err(message) => {
                if let Editor::Backend(editor) = &mut self.editor {
                    editor.busy = false;
                    editor.error = Some(message);
                }
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let [_, title_row, _, body] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(area);
        Line::styled("  Settings", theme::selected()).render(title_row, frame.buffer_mut());
        // A fresh statement-scoped buffer borrow per arm, so the Backend arm can
        // hand the whole frame to the `Form`-based editor (it needs &mut Frame).
        match &self.editor {
            Editor::None => self.draw_list(body, frame.buffer_mut()),
            Editor::Choice(editing) => draw_editor(editing, body, frame.buffer_mut()),
            Editor::Backend(editor) => editor.draw(frame, body),
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
            Arc::new(AtomicU64::new(0)),
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
                    Editor::Backend(BackendEditor::from_config(backend, &Config::default()));
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
            Arc::new(AtomicU64::new(0)),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| settings.draw(f, f.area())).unwrap();
        assert!(screen(&terminal).contains("https://radarr.example"));
    }

    /// Render the current settings screen (with whatever editor is open) to text.
    fn draw_to_text(settings: &mut SettingsApp) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| settings.draw(f, f.area())).unwrap();
        screen(&terminal)
    }

    #[test]
    fn opening_a_backend_shows_the_form_and_esc_cancels() {
        let mut settings = app();
        settings.open(Setting::Jellyfin);
        assert!(matches!(settings.editor, Editor::Backend(_)));

        // The Jellyfin form shows host, username and password fields plus the
        // Save/Cancel action rows.
        let text = draw_to_text(&mut settings);
        for label in ["Jellyfin", "Host", "Username", "Password", "Save", "Cancel"] {
            assert!(text.contains(label), "missing {label} in:\n{text}");
        }

        // The *arr forms show an API key field and omit the username row.
        settings.open(Setting::Radarr);
        let text = draw_to_text(&mut settings);
        assert!(text.contains("API key"), "missing API key in:\n{text}");
        assert!(
            !text.contains("Username"),
            "unexpected Username in:\n{text}"
        );

        settings.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(settings.editor, Editor::None));
    }

    #[test]
    fn save_with_empty_host_shows_error() {
        let mut settings = app();
        // Default config has an empty Jellyfin host.
        settings.open(Setting::Jellyfin);
        // Walk Host -> Username -> Password -> Save, then confirm.
        for _ in 0..3 {
            settings.on_key(KeyEvent::from(KeyCode::Down));
        }
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        // Still open, with an error and no in-flight connect (no task spawned).
        match &settings.editor {
            Editor::Backend(editor) => {
                assert!(editor.error.is_some(), "expected a validation error");
                assert!(!editor.busy, "must not start connecting on an invalid form");
            }
            _ => panic!("form should stay open on an invalid save"),
        }
    }

    #[test]
    fn blank_secret_is_left_to_the_server() {
        // The credential is validated by the server, not by the form: a blank
        // secret still submits (and then fails auth), so the form only requires
        // the host and, for Jellyfin, the username. The stored secret is never
        // reused, so a bare re-save cannot silently re-authenticate.
        let mut config = Config::default();
        config.radarr.host = "https://radarr.example".into();
        let radarr = BackendEditor::from_config(Setting::Radarr, &config);
        assert!(
            radarr.invalid_reason().is_none(),
            "the form must not require the API key itself"
        );

        config.jellyfin.host = "https://jelly.example".into();
        config.jellyfin.username = "alice".into();
        let jellyfin = BackendEditor::from_config(Setting::Jellyfin, &config);
        assert!(
            jellyfin.invalid_reason().is_none(),
            "the form must not require the password itself"
        );
    }

    #[test]
    fn successful_jellyfin_save_signals_reauth() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let reauth = Arc::new(AtomicU64::new(0));
        let mut settings = SettingsApp::new(
            Arc::new(Mutex::new(Config::default())),
            PathBuf::from("settings-test-config.toml"),
            AppSender::new("settings", tx),
            reauth.clone(),
        );

        // A successful Jellyfin save bumps the counter so the Jellyfin tab picks
        // up the fresh token instead of forcing a re-login.
        settings.open(Setting::Jellyfin);
        let save_gen = settings.save_gen;
        settings.on_event(Box::new(Msg::SaveDone {
            save_gen,
            result: Ok(()),
        }));
        assert_eq!(reauth.load(Ordering::Relaxed), 1);

        // An *arr save must not signal it: API-key auth is stateless, so the
        // running tab keeps working.
        settings.open(Setting::Radarr);
        let save_gen = settings.save_gen;
        settings.on_event(Box::new(Msg::SaveDone {
            save_gen,
            result: Ok(()),
        }));
        assert_eq!(reauth.load(Ordering::Relaxed), 1);
    }
}
