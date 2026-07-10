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
use crate::jellyfin::{Client, Credentials};
use crate::ui::theme;

use browse::{Browse, BrowseAction};
use login::{LoginAction, LoginForm};
use msg::Msg;

const VERSION: &str = env!("CARGO_PKG_VERSION");

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
}

impl JellyfinApp {
    pub fn new(config: Arc<Mutex<Config>>, config_path: PathBuf, sender: AppSender) -> Self {
        Self {
            config,
            config_path,
            sender,
            screen: Screen::Boot,
            auth_gen: 0,
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
                    // Playback lands in M4.
                    browse.error = Some(format!(
                        "playback not implemented yet ({})",
                        crate::jellyfin::display::item_title(&item)
                    ));
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
        }
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
    }
}
