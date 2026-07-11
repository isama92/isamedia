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
use crate::config::{Config, TrackPreference};
use crate::event::AppSender;
use crate::ui::form::{Field, Form, FormEvent};
use crate::ui::picker::{Picker, PickerEvent, PickerItem};
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

/// The overlay currently open on top of the settings list. The variants are
/// mutually exclusive; the Jellyfin row opens a submenu (credentials or
/// language) while Radarr/Sonarr open their credentials form directly.
#[allow(clippy::large_enum_variant)] // one instance per app; boxing buys nothing
enum Editor {
    None,
    Choice(Editing),
    Backend(BackendEditor),
    /// The Jellyfin submenu: 0 = Credentials, 1 = Language.
    JellyfinMenu {
        cursor: usize,
    },
    Language(LanguageEditor),
}

/// The Jellyfin language page: a row per track kind, each opening a
/// filterable picker over the ISO 639-2 table.
struct LanguageEditor {
    /// 0 = audio, 1 = subtitles.
    cursor: usize,
    /// The open picker, modal over the page.
    picker: Option<Picker>,
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

    /// Open the editor for the selected row: a choice list for Theme/Accent,
    /// the submenu for Jellyfin (which has more than credentials to edit), a
    /// credentials form for the other backends.
    fn open(&mut self, setting: Setting) {
        match setting {
            Setting::Theme | Setting::Accent => {
                self.editor = Editor::Choice(Editing {
                    setting,
                    cursor: current_choice_index(setting),
                });
            }
            Setting::Jellyfin => self.editor = Editor::JellyfinMenu { cursor: 0 },
            Setting::Radarr | Setting::Sonarr => {
                let editor = BackendEditor::from_config(setting, &self.config.lock().unwrap());
                self.editor = Editor::Backend(editor);
            }
        }
    }

    /// Where closing a backend form lands: back in the submenu it was opened
    /// from for Jellyfin, the settings list for the direct-entry backends.
    fn close_backend(&mut self, backend: Setting) {
        self.editor = if backend == Setting::Jellyfin {
            Editor::JellyfinMenu { cursor: 0 }
        } else {
            Editor::None
        };
        self.clamp_cursor();
    }

    /// Persist a picker choice for a language row. Synchronous like the
    /// Theme/Accent apply: no server round trip, so no generation counter.
    fn apply_language(&mut self, row: usize, key: &str) {
        let value = match key {
            "unset" => None,
            "default" => Some(TrackPreference::Default),
            "off" => Some(TrackPreference::Off),
            code => Some(TrackPreference::Language(code.to_string())),
        };
        let mut config = self.config.lock().unwrap();
        if row == 0 {
            config.jellyfin.audio_language = value;
        } else {
            config.jellyfin.subtitle_language = value;
        }
        if let Err(err) = config.save(&self.config_path) {
            tracing::warn!(%err, "failed to persist language preference");
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

    fn draw_jellyfin_menu(&self, cursor: usize, area: Rect, buf: &mut Buffer) {
        let (host, audio, subtitles) = {
            let config = self.config.lock().unwrap();
            let jf = &config.jellyfin;
            (
                jf.host.clone(),
                preference_label(jf.audio_language.as_ref()),
                preference_label(jf.subtitle_language.as_ref()),
            )
        };
        let host_summary = if host.is_empty() {
            "not configured".to_string()
        } else {
            host
        };
        Line::styled("  Jellyfin", theme::selected())
            .render(Rect::new(area.x, area.y, area.width, 1), buf);
        let rows = [
            menu_row(cursor == 0, "Credentials", &host_summary),
            menu_row(
                cursor == 1,
                "Language",
                &format!("audio: {audio}   subs: {subtitles}"),
            ),
        ];
        for (i, row) in rows.into_iter().enumerate() {
            let y = area.y + 2 + i as u16;
            if y < area.y + area.height {
                row.render(Rect::new(area.x, y, area.width, 1), buf);
            }
        }
        if area.height > 4 {
            Line::styled("  enter: open   esc: back", theme::dim()).render(
                Rect::new(area.x, area.y + area.height - 1, area.width, 1),
                buf,
            );
        }
    }

    fn draw_language(&self, editor: &LanguageEditor, frame: &mut Frame, area: Rect) {
        let (audio, subtitles) = {
            let config = self.config.lock().unwrap();
            let jf = &config.jellyfin;
            (
                preference_label(jf.audio_language.as_ref()),
                preference_label(jf.subtitle_language.as_ref()),
            )
        };
        // The page rows first (buffer borrow), then the picker modal on top
        // (needs &mut Frame) — the two frame borrows never overlap.
        {
            let buf = frame.buffer_mut();
            Line::styled("  Jellyfin language", theme::selected())
                .render(Rect::new(area.x, area.y, area.width, 1), buf);
            let rows = [
                menu_row(editor.cursor == 0, "Audio", &audio),
                menu_row(editor.cursor == 1, "Subtitles", &subtitles),
            ];
            for (i, row) in rows.into_iter().enumerate() {
                let y = area.y + 2 + i as u16;
                if y < area.y + area.height {
                    row.render(Rect::new(area.x, y, area.width, 1), buf);
                }
            }
            if area.height > 4 {
                Line::styled("  enter: change   esc: back", theme::dim()).render(
                    Rect::new(area.x, area.y + area.height - 1, area.width, 1),
                    buf,
                );
            }
        }
        if let Some(picker) = &editor.picker {
            let title = if editor.cursor == 0 {
                "Preferred audio language"
            } else {
                "Preferred subtitles language"
            };
            picker.draw(frame, area, title);
        }
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
            // Mutate under the lock, then snapshot and drop the guard before the
            // blocking disk write: the render thread locks this same mutex every
            // frame, so holding it across `save` could stall a frame. Two
            // backend saves racing could lose one's host update, but the write
            // is atomic (temp + rename) so it can't corrupt, and the race
            // self-corrects on the next save.
            let snapshot = {
                let mut config = config.lock().unwrap();
                config.jellyfin.host = host;
                if let Some(username) = username {
                    config.jellyfin.username = username;
                }
                config.jellyfin.user_id = client.user_id.clone();
                config.clone()
            };
            snapshot.save(&config_path).map_err(|err| err.to_string())?;
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
            // Snapshot under the lock, then save outside it (see the Jellyfin
            // branch above): never hold the config mutex across the disk write.
            let snapshot = {
                let mut config = config.lock().unwrap();
                match backend {
                    Setting::Radarr => config.radarr.host = normalized_host,
                    Setting::Sonarr => config.sonarr.host = normalized_host,
                    _ => {}
                }
                config.clone()
            };
            snapshot.save(&config_path).map_err(|err| err.to_string())?;
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

/// The picker for a language row, with the special choices pinned above the
/// filterable ISO 639-2 list. Sentinel keys can't collide with language
/// codes ("unset"/"default"/"off" are not ISO 639-2 codes).
fn language_picker(row: usize) -> Picker {
    let special = if row == 0 {
        PickerItem::new("default", "Default track")
    } else {
        PickerItem::new("off", "None (off)")
    };
    let pinned = vec![PickerItem::new("unset", "No preference"), special];
    let items = crate::lang::LANGUAGES
        .iter()
        .map(|lang| PickerItem::new(lang.code, format!("{} ({})", lang.name, lang.code)))
        .collect();
    Picker::new(pinned, items)
}

/// The picker key for a stored preference, to seed the cursor on open.
/// Language codes are canonicalised so a hand-edited config holding a /T or
/// 639-1 form ("deu", "it") still pre-highlights the right row; every write
/// path already stores the /B form.
fn preference_key(pref: Option<&TrackPreference>) -> String {
    match pref {
        None => "unset".into(),
        Some(TrackPreference::Default) => "default".into(),
        Some(TrackPreference::Off) => "off".into(),
        Some(TrackPreference::Language(code)) => crate::lang::canonical(code),
    }
}

/// Row summary for a stored preference: "Italian (ita)", "off", ...
fn preference_label(pref: Option<&TrackPreference>) -> String {
    match pref {
        None => "no preference".into(),
        Some(TrackPreference::Default) => "default track".into(),
        Some(TrackPreference::Off) => "off".into(),
        Some(TrackPreference::Language(code)) => match crate::lang::name(code) {
            Some(name) => format!("{name} ({code})"),
            None => code.clone(),
        },
    }
}

/// A `Label   summary` row for the submenu and language pages, in the same
/// shape as the settings-list rows.
fn menu_row(selected: bool, label: &str, summary: &str) -> Line<'static> {
    let style = if selected {
        theme::selected()
    } else {
        Style::new().fg(theme::fg())
    };
    let marker = if selected { "> " } else { "  " };
    Line::from(vec![
        Span::styled(format!("  {marker}"), style),
        Span::styled(format!("{label:<12}"), style),
        Span::styled(summary.to_string(), theme::dim()),
    ])
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

        // The Jellyfin submenu: two fixed entries, Credentials and Language.
        if let Editor::JellyfinMenu { cursor } = self.editor {
            match key.code {
                // Two entries: Up and Down both land on the other one.
                KeyCode::Up | KeyCode::Down => {
                    self.editor = Editor::JellyfinMenu { cursor: cursor ^ 1 };
                }
                KeyCode::Enter if cursor == 0 => {
                    let editor =
                        BackendEditor::from_config(Setting::Jellyfin, &self.config.lock().unwrap());
                    self.editor = Editor::Backend(editor);
                }
                KeyCode::Enter => {
                    self.editor = Editor::Language(LanguageEditor {
                        cursor: 0,
                        picker: None,
                    });
                }
                KeyCode::Esc | KeyCode::Backspace => self.editor = Editor::None,
                KeyCode::Char('q') if key.modifiers.is_empty() => {
                    return Some(ShellRequest::Quit);
                }
                _ => {}
            }
            return None;
        }

        // The language page: rows when no picker is open, else the picker.
        // Outcomes that need `&mut self` are collected first, so the borrow
        // on the editor has ended by the time they run (the same dance as
        // the backend form below).
        if let Editor::Language(editor) = &mut self.editor {
            let mut chosen = None;
            let mut close = false;
            if let Some(picker) = &mut editor.picker {
                match picker.on_key(key) {
                    PickerEvent::Chosen(choice) => {
                        chosen = Some((editor.cursor, choice));
                        editor.picker = None;
                    }
                    PickerEvent::Cancel => editor.picker = None,
                    PickerEvent::Consumed => {}
                }
            } else {
                match key.code {
                    // Two rows: Up and Down both land on the other one.
                    KeyCode::Up | KeyCode::Down => editor.cursor ^= 1,
                    KeyCode::Enter => {
                        let row = editor.cursor;
                        let current = {
                            let config = self.config.lock().unwrap();
                            let jf = &config.jellyfin;
                            let pref = if row == 0 {
                                jf.audio_language.as_ref()
                            } else {
                                jf.subtitle_language.as_ref()
                            };
                            preference_key(pref)
                        };
                        let mut picker = language_picker(row);
                        picker.select(&current);
                        editor.picker = Some(picker);
                    }
                    KeyCode::Esc | KeyCode::Backspace => close = true,
                    KeyCode::Char('q') if key.modifiers.is_empty() => {
                        return Some(ShellRequest::Quit);
                    }
                    _ => {}
                }
            }
            if let Some((row, choice)) = chosen {
                self.apply_language(row, &choice);
            }
            if close {
                self.editor = Editor::JellyfinMenu { cursor: 1 };
            }
            return None;
        }

        // Backend credentials form: decide the outcome while the form is
        // borrowed, then act once the borrow has ended.
        if let Editor::Backend(editor) = &mut self.editor {
            let backend = editor.backend;
            // Mid-connect: only Esc/Backspace cancel (the in-flight result is
            // dropped by the save_gen guard in on_event); every other key is
            // ignored.
            if editor.busy {
                if matches!(key.code, KeyCode::Esc | KeyCode::Backspace) {
                    self.save_gen += 1;
                    self.close_backend(backend);
                }
                return None;
            }
            // On the Save/Cancel rows 'q' can't be input, so it keeps its
            // app-wide quit meaning (on a field it types into the value).
            // Backspace there is handled by the form itself as Cancel.
            if key.code == KeyCode::Char('q')
                && key.modifiers.is_empty()
                && editor.form.action_row_focused()
            {
                return Some(ShellRequest::Quit);
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
                    self.close_backend(backend);
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
                let backend = match &self.editor {
                    Editor::Backend(editor) => editor.backend,
                    _ => return,
                };
                if backend == Setting::Jellyfin {
                    self.jellyfin_reauth.fetch_add(1, Ordering::Relaxed);
                }
                self.close_backend(backend);
            }
            Err(message) => {
                if let Editor::Backend(editor) = &mut self.editor {
                    editor.busy = false;
                    editor.error = Some(message);
                }
            }
        }
    }

    fn capturing_text(&self) -> bool {
        match &self.editor {
            // A field (not the action row) has focus in the credentials form.
            Editor::Backend(editor) => !editor.form.action_row_focused(),
            // An open language picker always holds a focused filter input.
            Editor::Language(editor) => editor.picker.is_some(),
            Editor::None | Editor::Choice(_) | Editor::JellyfinMenu { .. } => false,
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
            Editor::JellyfinMenu { cursor } => {
                self.draw_jellyfin_menu(*cursor, body, frame.buffer_mut())
            }
            Editor::Language(editor) => self.draw_language(editor, frame, body),
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
            // The Jellyfin submenu and the language page, picker closed and open.
            settings.editor = Editor::JellyfinMenu { cursor: 1 };
            terminal.draw(|f| settings.draw(f, f.area())).unwrap();
            settings.editor = Editor::Language(LanguageEditor {
                cursor: 0,
                picker: None,
            });
            terminal.draw(|f| settings.draw(f, f.area())).unwrap();
            settings.editor = Editor::Language(LanguageEditor {
                cursor: 1,
                picker: Some(language_picker(1)),
            });
            terminal.draw(|f| settings.draw(f, f.area())).unwrap();
            settings.editor = Editor::None;
        }
    }

    #[test]
    fn capturing_text_tracks_the_open_text_editor() {
        let mut settings = app();
        // The list and the navigable overlays are not text entry, so `s` is
        // free to act as the global stop shortcut over them.
        assert!(!settings.capturing_text());
        settings.editor = Editor::Choice(Editing {
            setting: Setting::Theme,
            cursor: 0,
        });
        assert!(!settings.capturing_text());
        settings.editor = Editor::JellyfinMenu { cursor: 0 };
        assert!(!settings.capturing_text());
        // A language page with the picker closed is navigable; opening the
        // picker focuses its filter input.
        settings.editor = Editor::Language(LanguageEditor {
            cursor: 0,
            picker: None,
        });
        assert!(!settings.capturing_text());
        settings.editor = Editor::Language(LanguageEditor {
            cursor: 0,
            picker: Some(language_picker(0)),
        });
        assert!(settings.capturing_text());
        // A freshly opened credentials form has a field (not the action row)
        // focused, so a keystroke is text and must not stop playback.
        settings.editor = Editor::Backend(BackendEditor::from_config(
            Setting::Jellyfin,
            &Config::default(),
        ));
        assert!(settings.capturing_text());
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
        // Jellyfin opens the submenu, not the form; Enter on Credentials
        // opens the form.
        settings.open(Setting::Jellyfin);
        assert!(matches!(
            settings.editor,
            Editor::JellyfinMenu { cursor: 0 }
        ));
        let text = draw_to_text(&mut settings);
        for label in ["Credentials", "Language"] {
            assert!(text.contains(label), "missing {label} in:\n{text}");
        }
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(settings.editor, Editor::Backend(_)));

        // The Jellyfin form shows host, username and password fields plus the
        // Save/Cancel action rows.
        let text = draw_to_text(&mut settings);
        for label in ["Jellyfin", "Host", "Username", "Password", "Save", "Cancel"] {
            assert!(text.contains(label), "missing {label} in:\n{text}");
        }

        // Esc backs out one level at a time: form -> submenu -> list.
        settings.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(settings.editor, Editor::JellyfinMenu { .. }));
        settings.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(settings.editor, Editor::None));

        // The *arr rows keep opening their form directly, without a username
        // row, and Esc returns straight to the list.
        settings.open(Setting::Radarr);
        assert!(matches!(settings.editor, Editor::Backend(_)));
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
        // Default config has an empty Jellyfin host. Enter the credentials
        // form through the submenu.
        settings.open(Setting::Jellyfin);
        settings.on_key(KeyEvent::from(KeyCode::Enter));
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
        // up the fresh token instead of forcing a re-login, and lands back in
        // the submenu the form was opened from.
        settings.open(Setting::Jellyfin);
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        let save_gen = settings.save_gen;
        settings.on_event(Box::new(Msg::SaveDone {
            save_gen,
            result: Ok(()),
        }));
        assert_eq!(reauth.load(Ordering::Relaxed), 1);
        assert!(matches!(settings.editor, Editor::JellyfinMenu { .. }));

        // An *arr save must not signal it: API-key auth is stateless, so the
        // running tab keeps working. Its form closes back to the list.
        settings.open(Setting::Radarr);
        let save_gen = settings.save_gen;
        settings.on_event(Box::new(Msg::SaveDone {
            save_gen,
            result: Ok(()),
        }));
        assert_eq!(reauth.load(Ordering::Relaxed), 1);
        assert!(matches!(settings.editor, Editor::None));
    }

    /// An app whose config saves land in a scratch file that is cleaned up.
    fn app_with_temp_config(name: &str) -> (SettingsApp, PathBuf) {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let path =
            std::env::temp_dir().join(format!("isamedia-{name}-{}.toml", std::process::id()));
        let settings = SettingsApp::new(
            Arc::new(Mutex::new(Config::default())),
            path.clone(),
            AppSender::new("settings", tx),
            Arc::new(AtomicU64::new(0)),
        );
        (settings, path)
    }

    #[test]
    fn language_page_persists_picker_choices() {
        let (mut settings, path) = app_with_temp_config("lang-pick");
        settings.open(Setting::Jellyfin);
        settings.on_key(KeyEvent::from(KeyCode::Down)); // Credentials -> Language
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(settings.editor, Editor::Language(_)));

        // Audio row: filter down to Italian and choose it.
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        for c in "italia".chars() {
            settings.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        settings.on_key(KeyEvent::from(KeyCode::Down)); // skip "No preference"
        settings.on_key(KeyEvent::from(KeyCode::Down)); // skip "Default track"
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            settings.config.lock().unwrap().jellyfin.audio_language,
            Some(TrackPreference::Language("ita".into()))
        );

        // Subtitles row: the pinned "None (off)" choice.
        settings.on_key(KeyEvent::from(KeyCode::Down));
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        settings.on_key(KeyEvent::from(KeyCode::Down)); // "No preference" -> "None (off)"
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            settings.config.lock().unwrap().jellyfin.subtitle_language,
            Some(TrackPreference::Off)
        );

        // Back to "No preference" clears the value again. The picker reopens
        // seeded on the current value ("None (off)"), so step up once.
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        settings.on_key(KeyEvent::from(KeyCode::Up));
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            settings.config.lock().unwrap().jellyfin.subtitle_language,
            None
        );

        // Esc chain: language page -> submenu (Language row) -> list.
        settings.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(
            settings.editor,
            Editor::JellyfinMenu { cursor: 1 }
        ));
        settings.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(settings.editor, Editor::None));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn submenu_backspace_backs_out_and_q_quits() {
        let mut settings = app();
        settings.open(Setting::Jellyfin);
        assert!(matches!(
            settings.on_key(KeyEvent::from(KeyCode::Char('q'))),
            Some(ShellRequest::Quit)
        ));
        // Quitting is the shell's business; the submenu stays as it was.
        assert!(matches!(settings.editor, Editor::JellyfinMenu { .. }));
        settings.on_key(KeyEvent::from(KeyCode::Backspace));
        assert!(matches!(settings.editor, Editor::None));
    }

    #[test]
    fn language_page_backspace_and_q_unless_picker_is_typing() {
        let mut settings = app();
        settings.editor = Editor::Language(LanguageEditor {
            cursor: 0,
            picker: None,
        });
        assert!(matches!(
            settings.on_key(KeyEvent::from(KeyCode::Char('q'))),
            Some(ShellRequest::Quit)
        ));

        // With the picker open, q and Backspace belong to the filter input.
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(
            settings
                .on_key(KeyEvent::from(KeyCode::Char('q')))
                .is_none()
        );
        assert!(
            settings
                .on_key(KeyEvent::from(KeyCode::Backspace))
                .is_none()
        );
        match &settings.editor {
            Editor::Language(editor) => assert!(editor.picker.is_some()),
            _ => panic!("picker should still be open"),
        }
        settings.on_key(KeyEvent::from(KeyCode::Esc)); // close the picker

        // Back on the page rows, Backspace returns to the submenu.
        settings.on_key(KeyEvent::from(KeyCode::Backspace));
        assert!(matches!(
            settings.editor,
            Editor::JellyfinMenu { cursor: 1 }
        ));
    }

    #[test]
    fn credentials_form_backspace_and_q_on_the_buttons() {
        let mut settings = app();
        settings.open(Setting::Radarr);
        // On a field, q is input, not quit.
        assert!(
            settings
                .on_key(KeyEvent::from(KeyCode::Char('q')))
                .is_none()
        );
        match &settings.editor {
            Editor::Backend(editor) => assert_eq!(editor.form.text(field::HOST), "q"),
            _ => panic!("form should be open"),
        }
        // Walk Host -> API key -> Save: there q quits and Backspace cancels.
        settings.on_key(KeyEvent::from(KeyCode::Down));
        settings.on_key(KeyEvent::from(KeyCode::Down));
        assert!(matches!(
            settings.on_key(KeyEvent::from(KeyCode::Char('q'))),
            Some(ShellRequest::Quit)
        ));
        settings.on_key(KeyEvent::from(KeyCode::Backspace));
        assert!(matches!(settings.editor, Editor::None));

        // The Jellyfin form cancels back into the submenu instead.
        settings.open(Setting::Jellyfin);
        settings.on_key(KeyEvent::from(KeyCode::Enter));
        for _ in 0..3 {
            settings.on_key(KeyEvent::from(KeyCode::Down)); // -> Save
        }
        settings.on_key(KeyEvent::from(KeyCode::Backspace));
        assert!(matches!(settings.editor, Editor::JellyfinMenu { .. }));
    }

    #[test]
    fn picker_seeds_from_non_canonical_codes() {
        // A hand-edited config may hold a /T or 639-1 form; the picker must
        // still open on the matching row (which then re-stores it as /B).
        let (mut settings, path) = app_with_temp_config("lang-seed");
        settings.config.lock().unwrap().jellyfin.audio_language =
            Some(TrackPreference::Language("deu".into()));
        settings.editor = Editor::Language(LanguageEditor {
            cursor: 0,
            picker: None,
        });
        settings.on_key(KeyEvent::from(KeyCode::Enter)); // open, seeded on German
        settings.on_key(KeyEvent::from(KeyCode::Enter)); // choose it
        assert_eq!(
            settings.config.lock().unwrap().jellyfin.audio_language,
            Some(TrackPreference::Language("ger".into()))
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn language_summaries_are_shown() {
        let (mut settings, _path) = app_with_temp_config("lang-summary");
        {
            let mut config = settings.config.lock().unwrap();
            config.jellyfin.audio_language = Some(TrackPreference::Language("ita".into()));
            config.jellyfin.subtitle_language = Some(TrackPreference::Off);
        }
        settings.editor = Editor::JellyfinMenu { cursor: 1 };
        let text = draw_to_text(&mut settings);
        assert!(text.contains("audio: Italian (ita)"), "{text}");
        assert!(text.contains("subs: off"), "{text}");

        settings.editor = Editor::Language(LanguageEditor {
            cursor: 0,
            picker: None,
        });
        let text = draw_to_text(&mut settings);
        assert!(text.contains("Italian (ita)"), "{text}");
    }
}
