mod browse;
mod login;
mod msg;

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
use crate::jellyfin::{Client, Credentials, MediaItem};
use crate::player::{self, PlayerEvent, PlayerHandle};
use crate::ui::{prompt, theme};

use browse::{Browse, BrowseAction};
use login::{LoginAction, LoginForm};
use msg::Msg;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// "1:23:45" / "23:45" style clock for the now-playing bar.
fn format_clock(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

enum Screen {
    /// Not activated yet.
    Boot,
    /// Auto-login with stored credentials in flight.
    Connecting,
    Login(LoginForm),
    Browse(Browse),
}

pub struct JellyfinApp {
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    sender: AppSender,
    screen: Screen,
    /// Bumped whenever an auth attempt starts or is cancelled, so stale
    /// results are ignored.
    auth_gen: u64,
    player: Option<PlayerHandle>,
    /// Bumped per playback; events from a replaced player are dropped.
    player_gen: u64,
    /// Item waiting behind the "replace current playback?" prompt.
    pending_play: Option<MediaItem>,
}

impl JellyfinApp {
    pub fn new(config: Arc<Mutex<Config>>, config_path: PathBuf, sender: AppSender) -> Self {
        Self {
            config,
            config_path,
            sender,
            screen: Screen::Boot,
            auth_gen: 0,
            player: None,
            player_gen: 0,
            pending_play: None,
        }
    }

    fn start_playback(&mut self, item: MediaItem) {
        let Screen::Browse(browse) = &self.screen else {
            return;
        };
        let client = browse.client.clone();
        let skip_types = self.config.lock().unwrap().jellyfin.skip_segments.clone();
        self.player_gen += 1;
        let player_gen = self.player_gen;
        let sender = self.sender.clone();
        let handle = player::spawn(client, item, skip_types, move |event| {
            sender.send(Msg::Player { player_gen, event });
        });
        self.player = Some(handle);
    }

    fn on_player_event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::Started { title } => {
                if let Some(player) = &mut self.player {
                    player.now.title = title;
                    player.now.position_secs = 0.0;
                    player.now.duration_secs = None;
                }
            }
            PlayerEvent::Position { secs } => {
                if let Some(player) = &mut self.player {
                    player.now.position_secs = secs;
                }
            }
            PlayerEvent::Duration { secs } => {
                if let Some(player) = &mut self.player {
                    player.now.duration_secs = Some(secs);
                }
            }
            PlayerEvent::Failed(message) => {
                if let Screen::Browse(browse) = &mut self.screen {
                    browse.error = Some(message);
                }
            }
            PlayerEvent::Exited => {
                self.player = None;
                if let Screen::Browse(browse) = &mut self.screen {
                    browse.on_playback_finished();
                }
            }
        }
    }

    fn credentials(&self) -> Credentials {
        let config = self.config.lock().unwrap();
        let jf = &config.jellyfin;
        Credentials {
            host: jf.host.clone(),
            username: jf.username.clone(),
            password: jf.password.clone(),
            device: jf.device.clone(),
            device_id: jf.device_id.clone(),
            version: VERSION.to_string(),
            token: jf.token.clone(),
            user_id: jf.user_id.clone(),
        }
    }

    fn spawn_connect(&mut self, creds: Credentials) {
        self.auth_gen += 1;
        let auth_gen = self.auth_gen;
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = Client::connect(creds).await;
            sender.send(Msg::AuthDone { auth_gen, result });
        });
    }

    fn login_form(&self) -> LoginForm {
        LoginForm::from_config(&self.config.lock().unwrap().jellyfin)
    }

    /// Persist whatever the successful client used back into the config, so
    /// tokens survive restarts and form input becomes the stored credentials.
    fn persist_auth(&self, client: &Client, from_form: Option<(&str, &str, &str)>) {
        let mut config = self.config.lock().unwrap();
        let jf = &mut config.jellyfin;
        if let Some((host, username, password)) = from_form {
            jf.host = host.to_string();
            jf.username = username.to_string();
            jf.password = password.to_string();
        }
        jf.token = client.token.clone();
        jf.user_id = client.user_id.clone();
        if let Err(err) = config.save(&self.config_path) {
            tracing::warn!(%err, "failed to persist credentials");
        }
    }

    /// Drop the stored token and return to the login form; used when the
    /// server starts answering 401 (token revoked/expired).
    fn on_session_expired(&mut self) {
        {
            let mut config = self.config.lock().unwrap();
            config.jellyfin.token.clear();
            config.jellyfin.user_id.clear();
            if let Err(err) = config.save(&self.config_path) {
                tracing::warn!(%err, "failed to persist cleared token");
            }
        }
        self.auth_gen += 1;
        let mut form = self.login_form();
        form.error = Some("session expired, please log in again".into());
        self.screen = Screen::Login(form);
    }

    fn on_auth_done(&mut self, result: Result<Client, crate::jellyfin::Error>) {
        match result {
            Ok(client) => {
                let from_form = match &self.screen {
                    Screen::Login(form) => Some((
                        form.host.value().to_string(),
                        form.username.value().to_string(),
                        form.password.value().to_string(),
                    )),
                    _ => None,
                };
                self.persist_auth(
                    &client,
                    from_form
                        .as_ref()
                        .map(|(h, u, p)| (h.as_str(), u.as_str(), p.as_str())),
                );
                self.screen = Screen::Browse(Browse::new(client, self.sender.clone()));
            }
            Err(err) => {
                tracing::warn!(%err, "authentication failed");
                let mut form = match std::mem::replace(&mut self.screen, Screen::Boot) {
                    Screen::Login(form) => form,
                    _ => self.login_form(),
                };
                form.busy = false;
                form.error = Some(err.to_string());
                self.screen = Screen::Login(form);
            }
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

    fn activate(&mut self) {
        if let Screen::Boot = self.screen {
            let creds = self.credentials();
            if !creds.host.is_empty() && !creds.username.is_empty() {
                self.screen = Screen::Connecting;
                self.spawn_connect(creds);
            } else {
                self.screen = Screen::Login(self.login_form());
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<ShellRequest> {
        // The replace-playback prompt is modal.
        if self.pending_play.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let item = self.pending_play.take().unwrap();
                    if let Some(old) = self.player.take() {
                        old.stop();
                    }
                    self.start_playback(item);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.pending_play = None;
                }
                _ => {}
            }
            return None;
        }

        // Global stop key, active whenever no text input has focus.
        if key.code == KeyCode::Char('s')
            && self.player.is_some()
            && matches!(&self.screen, Screen::Browse(browse) if !browse.input_focused())
        {
            if let Some(player) = &self.player {
                player.stop();
            }
            return None;
        }

        match &mut self.screen {
            Screen::Boot => None,
            Screen::Connecting => match key.code {
                KeyCode::Esc => {
                    // Cancel the pending attempt and drop to the form.
                    self.auth_gen += 1;
                    self.screen = Screen::Login(self.login_form());
                    None
                }
                KeyCode::Char('q') => Some(ShellRequest::Quit),
                _ => None,
            },
            Screen::Login(form) => {
                if let Some(LoginAction::Submit) = form.on_key(key) {
                    form.busy = true;
                    form.error = None;
                    let (host, username, password) = (
                        form.host.value().to_string(),
                        form.username.value().to_string(),
                        form.password.value().to_string(),
                    );
                    let mut creds = self.credentials();
                    creds.host = host;
                    creds.username = username;
                    creds.password = password;
                    // Force a fresh username/password auth for form submissions.
                    creds.token = String::new();
                    creds.user_id = String::new();
                    self.spawn_connect(creds);
                }
                None
            }
            Screen::Browse(browse) => match browse.on_key(key) {
                Some(BrowseAction::Quit) => Some(ShellRequest::Quit),
                Some(BrowseAction::Play(item)) => {
                    if self.player.is_some() {
                        self.pending_play = Some(item);
                    } else {
                        self.start_playback(item);
                    }
                    None
                }
                None => None,
            },
        }
    }

    fn on_tick(&mut self) {
        if let Screen::Browse(browse) = &mut self.screen {
            browse.on_tick();
        }
    }

    fn on_event(&mut self, payload: Box<dyn Any + Send>) {
        let Ok(msg) = payload.downcast::<Msg>() else {
            return;
        };
        match *msg {
            Msg::AuthDone { auth_gen, result } => {
                if auth_gen != self.auth_gen {
                    return;
                }
                self.on_auth_done(result);
            }
            Msg::ItemsLoaded { fetch_gen, result } => {
                if matches!(result, Err(crate::jellyfin::Error::Unauthorized)) {
                    self.on_session_expired();
                    return;
                }
                if let Screen::Browse(browse) = &mut self.screen {
                    browse.on_items_loaded(fetch_gen, result);
                }
            }
            Msg::WatchedToggled(result) => {
                if matches!(result, Err(crate::jellyfin::Error::Unauthorized)) {
                    self.on_session_expired();
                    return;
                }
                if let Screen::Browse(browse) = &mut self.screen {
                    browse.on_watched_toggled(result);
                }
            }
            Msg::Player { player_gen, event } => {
                if player_gen != self.player_gen {
                    return; // event from a player that was replaced
                }
                self.on_player_event(event);
            }
        }
    }

    fn on_quit(&mut self) -> bool {
        if let Some(player) = &self.player {
            player.stop();
            return true;
        }
        false
    }

    fn status_line(&self) -> Option<ratatui::text::Line<'static>> {
        let player = self.player.as_ref()?;
        let position = format_clock(player.now.position_secs);
        let timing = match player.now.duration_secs {
            Some(duration) => format!("{position} / {}", format_clock(duration)),
            None => position,
        };
        Some(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(" ⏵ ", theme::selected()),
            ratatui::text::Span::styled(player.now.title.clone(), theme::accent()),
            ratatui::text::Span::styled(format!("  {timing}"), ratatui::style::Style::new().fg(theme::FG)),
            ratatui::text::Span::styled("  s: stop", theme::dim()),
        ]))
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        match &mut self.screen {
            Screen::Boot => {}
            Screen::Connecting => {
                Line::styled("connecting...", theme::dim())
                    .centered()
                    .render(area, frame.buffer_mut());
            }
            Screen::Login(form) => form.draw(frame, area),
            Screen::Browse(browse) => browse.draw(frame, area),
        }
        if self.pending_play.is_some() {
            prompt::draw_confirm(frame, area, "Replace current playback?");
        }
    }
}
