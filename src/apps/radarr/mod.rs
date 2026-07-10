//! The Radarr app: a state machine over setup (host + API key), a connect
//! attempt, and the browse screen. Auth results carry `auth_gen` so a
//! cancelled or superseded attempt can never resurrect itself.

mod browse;
mod msg;
mod setup;

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
use crate::radarr::{Client, Error};
use crate::ui::theme;

use browse::{Browse, BrowseAction};
use msg::Msg;
use setup::{SetupAction, SetupForm};

#[allow(clippy::large_enum_variant)] // single instance, size is irrelevant
enum Screen {
    /// Not activated yet.
    Boot,
    /// Auto-connect with the stored API key in flight.
    Connecting,
    Setup(SetupForm),
    Browse(Browse),
}

pub struct RadarrApp {
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    sender: AppSender,
    screen: Screen,
    /// Bumped whenever a connect attempt starts or is cancelled, so stale
    /// results are ignored.
    auth_gen: u64,
    /// Fetch-generation allocator shared with every Browse instance, so
    /// generations stay monotonic across re-connects.
    browse_gen: Arc<std::sync::atomic::AtomicU64>,
}

impl RadarrApp {
    pub fn new(config: Arc<Mutex<Config>>, config_path: PathBuf, sender: AppSender) -> Self {
        Self {
            config,
            config_path,
            sender,
            screen: Screen::Boot,
            auth_gen: 0,
            browse_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn setup_form(&self) -> SetupForm {
        SetupForm::from_config(&self.config.lock().unwrap().radarr)
    }

    /// Connect with the key typed into the form, or (key `None`) with the
    /// one stored in the OS keyring — read inside the task, since keyring
    /// access is blocking and must stay off the render thread.
    fn spawn_connect(&mut self, host: String, api_key: Option<String>) {
        self.auth_gen += 1;
        let auth_gen = self.auth_gen;
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let api_key = match api_key {
                Some(key) => Some(key),
                None => tokio::task::spawn_blocking(|| {
                    crate::secrets::get(crate::secrets::RADARR_API_KEY)
                })
                .await
                .ok()
                .flatten(),
            };
            let result = match api_key.filter(|key| !key.is_empty()) {
                None => Err(Error::MissingApiKey),
                Some(key) => Client::connect(&host, key).await,
            };
            sender.send(Msg::ConnectDone { auth_gen, result });
        });
    }

    /// Persist a successful connect: the host into the config file, the key
    /// (form connects only) into the OS keyring.
    fn persist_auth(&self, client: &Client, form_key: Option<String>) {
        {
            let mut config = self.config.lock().unwrap();
            config.radarr.host = client.host().to_string();
            if let Err(err) = config.save(&self.config_path) {
                tracing::warn!(%err, "failed to persist config");
            }
        }
        let Some(key) = form_key else {
            return; // stored-key connect: the keyring already has it
        };
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(err) = crate::secrets::set(crate::secrets::RADARR_API_KEY, &key) {
                sender.send(Msg::KeyringError(format!(
                    "failed to store API key in system keyring (you will have to enter it again next start): {err}"
                )));
            }
        });
    }

    /// Drop the stored key and return to the setup form; used when the
    /// server starts answering 401 (key revoked or regenerated).
    fn on_session_expired(&mut self) {
        tokio::task::spawn_blocking(|| {
            if let Err(err) = crate::secrets::delete(crate::secrets::RADARR_API_KEY) {
                tracing::warn!(%err, "failed to delete rejected API key from keyring");
            }
        });
        self.auth_gen += 1;
        let mut form = self.setup_form();
        form.error = Some("API key rejected, please enter it again".into());
        self.screen = Screen::Setup(form);
    }

    fn on_connect_done(&mut self, result: Result<Client, Error>) {
        match result {
            Ok(client) => {
                let form_key = match &self.screen {
                    Screen::Setup(form) => Some(form.api_key.value().to_string()),
                    _ => None,
                };
                self.persist_auth(&client, form_key);
                let plain_http = crate::net::is_plain_http(client.host());
                let mut browse = Browse::new(client, self.sender.clone(), self.browse_gen.clone());
                if plain_http {
                    // The setup form warns about this too, but auto-connect
                    // (stored host + key) never shows the form.
                    browse.notice =
                        Some("connected over plain http://; traffic is unencrypted".into());
                }
                self.screen = Screen::Browse(browse);
            }
            // No key stored yet: first run, show a clean form.
            Err(Error::MissingApiKey) => {
                if !matches!(self.screen, Screen::Setup(_)) {
                    self.screen = Screen::Setup(self.setup_form());
                }
            }
            Err(err) => {
                tracing::warn!(%err, "radarr connect failed");
                let mut form = match std::mem::replace(&mut self.screen, Screen::Boot) {
                    Screen::Setup(form) => form,
                    _ => self.setup_form(),
                };
                form.busy = false;
                form.error = Some(err.to_string());
                self.screen = Screen::Setup(form);
            }
        }
    }
}

impl MediaApp for RadarrApp {
    fn id(&self) -> AppId {
        "radarr"
    }

    fn title(&self) -> &'static str {
        "Radarr"
    }

    fn activate(&mut self) {
        if let Screen::Boot = self.screen {
            let host = self.config.lock().unwrap().radarr.host.clone();
            if host.is_empty() {
                self.screen = Screen::Setup(self.setup_form());
            } else {
                self.screen = Screen::Connecting;
                self.spawn_connect(host, None);
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
                    self.screen = Screen::Setup(self.setup_form());
                    None
                }
                KeyCode::Char('q') => Some(ShellRequest::Quit),
                _ => None,
            },
            Screen::Setup(form) => {
                if let Some(SetupAction::Submit) = form.on_key(key) {
                    form.busy = true;
                    form.error = None;
                    let host = form.host.value().to_string();
                    let api_key = form.api_key.value().to_string();
                    self.spawn_connect(host, Some(api_key));
                }
                None
            }
            Screen::Browse(browse) => browse
                .on_key(key)
                .map(|BrowseAction::Quit| ShellRequest::Quit),
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
            Msg::ConnectDone { auth_gen, result } => {
                if auth_gen != self.auth_gen {
                    return;
                }
                self.on_connect_done(result);
            }
            // For every result kind, the browse checks staleness first and
            // only reports a key rejection for a CURRENT-generation 401; a
            // stale 401 belongs to an old key and must not kick a freshly
            // re-connected session back to setup. Results arriving while no
            // browse is on screen are inherently stale and dropped.
            Msg::MoviesLoaded { fetch_gen, result } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_movies_loaded(fetch_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::QueueLoaded { queue_gen, result } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_queue_loaded(queue_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::ReleasesLoaded { fetch_gen, result } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_releases_loaded(fetch_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::HistoryLoaded { fetch_gen, result } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_history_loaded(fetch_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::CommandDone {
                fetch_gen,
                kind,
                result,
            } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_command_done(fetch_gen, kind, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::LookupLoaded {
                add_lookup_gen,
                result,
            } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_lookup_loaded(add_lookup_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::AddOptionsLoaded { prereq_gen, result } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_add_options_loaded(prereq_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::MovieAdded { add_gen, result } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_movie_added(add_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::AutoSearchStarted {
                auto_search_gen,
                result,
            } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_auto_search_started(auto_search_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::AutoSearchPolled {
                auto_search_gen,
                result,
            } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_auto_search_polled(auto_search_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::KeyringError(message) => {
                tracing::warn!(message, "keyring problem");
                if let Screen::Browse(browse) = &mut self.screen {
                    browse.error = Some(message);
                }
            }
        }
    }

    /// Surface the background auto-search status in the shell status bar, so
    /// it stays visible even from another tab.
    fn status_line(&self) -> Option<Line<'static>> {
        match &self.screen {
            Screen::Browse(browse) => browse.status_line(),
            _ => None,
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
            Screen::Setup(form) => form.draw(frame, area),
            Screen::Browse(browse) => browse.draw(frame, area),
        }
    }
}
