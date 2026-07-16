mod browse;
mod login;
mod msg;

use std::any::Any;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::app::{AppId, MediaApp, ShellRequest};
use crate::config::{Config, LanguageOverrides};
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

#[allow(clippy::large_enum_variant)] // single instance, size is irrelevant
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
    /// Fetch-generation allocator shared with every Browse instance, so
    /// generations stay monotonic across re-logins (a fetch in flight from a
    /// pre-relogin Browse can never match a new instance's generation).
    browse_gen: Arc<std::sync::atomic::AtomicU64>,
    /// Bumped by the Settings tab when it re-authenticates Jellyfin. Compared
    /// against `seen_reauth` on activate so a live session reconnects with the
    /// freshly stored token instead of being forced back to the login form.
    reauth: Arc<std::sync::atomic::AtomicU64>,
    seen_reauth: u64,
    /// Remembered per-movie/per-show track switches. Only this app touches
    /// them (read at playback start, written on `TrackSwitched`, both on the
    /// render thread), so they live here as plain state, in their own file
    /// (`language_overrides.json`) rather than in the shared config.
    overrides: LanguageOverrides,
    overrides_path: PathBuf,
}

impl JellyfinApp {
    pub fn new(
        config: Arc<Mutex<Config>>,
        config_path: PathBuf,
        sender: AppSender,
        reauth: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let seen_reauth = reauth.load(Ordering::Relaxed);
        let overrides_path = LanguageOverrides::path_for(&config_path);
        let overrides = LanguageOverrides::load(&overrides_path);
        Self {
            config,
            config_path,
            sender,
            screen: Screen::Boot,
            auth_gen: 0,
            player: None,
            player_gen: 0,
            pending_play: None,
            browse_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reauth,
            seen_reauth,
            overrides,
            overrides_path,
        }
    }

    fn start_playback(&mut self, item: MediaItem) {
        let Screen::Browse(browse) = &self.screen else {
            return;
        };
        let client = browse.client.clone();
        let (skip_types, prefs) = {
            let config = self.config.lock().unwrap();
            let jf = &config.jellyfin;
            (
                jf.skip_segments.clone(),
                self.overrides
                    .language_prefs(player::override_key(&item), jf),
            )
        };
        self.player_gen += 1;
        let player_gen = self.player_gen;
        let sender = self.sender.clone();
        let handle = player::spawn(client, item, skip_types, prefs, move |event| {
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
            PlayerEvent::TrackSwitched {
                override_key,
                kind,
                selection,
            } => {
                let entry = self.overrides.entry(override_key);
                match kind {
                    player::TrackKind::Audio => entry.audio = Some(selection),
                    player::TrackKind::Subtitle => entry.subtitles = Some(selection),
                }
                // Small synchronous write, same weight as the theme saves;
                // the supervisor only emits genuine switches, so this stays
                // rare.
                if let Err(err) = self.overrides.save(&self.overrides_path) {
                    tracing::warn!(%err, "failed to persist language override");
                }
            }
            PlayerEvent::Failed(message) => {
                if let Screen::Browse(browse) = &mut self.screen {
                    browse.error = Some(message);
                }
            }
            PlayerEvent::SessionExpired => self.on_session_expired(),
            PlayerEvent::Exited => {
                self.player = None;
                // Playback ended on its own (track finished or mpv was closed),
                // so a pending replace prompt no longer has anything to replace.
                // Drop it: left set it strands the "Replace current playback?"
                // modal over stopped playback, and with the now-playing bar gone
                // `s` can no longer reach here to dismiss it. Only ever set while
                // a player existed, and stale exits are dropped before this runs.
                self.pending_play = None;
                if let Screen::Browse(browse) = &mut self.screen {
                    browse.on_playback_finished();
                }
            }
        }
    }

    /// Credentials from the config file only; secrets stay empty here and are
    /// filled from the keyring inside the connect task (keyring access is
    /// blocking and must stay off the render thread).
    fn credentials(&self) -> Credentials {
        let config = self.config.lock().unwrap();
        let jf = &config.jellyfin;
        Credentials {
            host: jf.host.clone(),
            username: jf.username.clone(),
            password: String::new(),
            device: jf.device.clone(),
            device_id: jf.device_id.clone(),
            version: VERSION.to_string(),
            token: String::new(),
            user_id: jf.user_id.clone(),
        }
    }

    fn spawn_connect(&mut self, mut creds: Credentials, use_stored_secrets: bool) {
        self.auth_gen += 1;
        let auth_gen = self.auth_gen;
        let sender = self.sender.clone();
        tokio::spawn(async move {
            if use_stored_secrets {
                let secrets = tokio::task::spawn_blocking(|| {
                    (
                        crate::secrets::get(crate::secrets::JELLYFIN_TOKEN),
                        crate::secrets::get(crate::secrets::JELLYFIN_PASSWORD),
                    )
                })
                .await;
                match secrets {
                    Ok((token, password)) => {
                        creds.token = token.unwrap_or_default();
                        creds.password = password.unwrap_or_default();
                    }
                    // The blocking keyring read panicked or was cancelled. Log
                    // the cause (it carries no secret material) instead of
                    // silently falling through to a bare "authentication
                    // failed" from the empty-credential connect below.
                    Err(err) => {
                        tracing::warn!(%err, "reading stored Jellyfin secrets failed");
                    }
                }
            }
            let result = Client::connect(creds).await;
            sender.send(Msg::AuthDone { auth_gen, result });
        });
    }

    fn login_form(&self) -> LoginForm {
        LoginForm::from_config(&self.config.lock().unwrap().jellyfin)
    }

    /// Persist a successful login: non-secrets into the config file, the
    /// token (and, for form logins, the password) into the OS keyring.
    fn persist_auth(&self, client: &Client, from_form: Option<(&str, &str, &str)>) {
        {
            let mut config = self.config.lock().unwrap();
            let jf = &mut config.jellyfin;
            if let Some((host, username, _)) = from_form {
                jf.host = host.to_string();
                jf.username = username.to_string();
            }
            jf.user_id = client.user_id.clone();
            if let Err(err) = config.save(&self.config_path) {
                tracing::warn!(%err, "failed to persist config");
            }
        }

        let token = client.token.clone();
        let form_password = from_form.map(|(_, _, password)| password.to_string());
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(err) = crate::secrets::set(crate::secrets::JELLYFIN_TOKEN, &token) {
                sender.send(Msg::KeyringError(format!(
                    "failed to store token in system keyring (you will have to log in again next start): {err}"
                )));
                return;
            }
            let Some(password) = form_password else {
                return;
            };
            // An empty password field means "don't keep one around".
            let result = if password.is_empty() {
                crate::secrets::delete(crate::secrets::JELLYFIN_PASSWORD)
            } else {
                crate::secrets::set(crate::secrets::JELLYFIN_PASSWORD, &password)
            };
            if let Err(err) = result {
                sender.send(Msg::KeyringError(format!(
                    "failed to store password in system keyring: {err}"
                )));
            }
        });
    }

    /// Drop the stored token and return to the login form; used when the
    /// server starts answering 401 (token revoked/expired).
    fn on_session_expired(&mut self) {
        // Stop playback: a revoked token breaks the stream's own requests
        // and every playback report, and the login screen has no way to
        // reach the stop key (its inputs swallow plain chars). Dropping the
        // handle also clears the now-playing bar and its "s: stop" hint.
        if let Some(player) = self.player.take() {
            player.stop();
        }
        self.pending_play = None;
        {
            let mut config = self.config.lock().unwrap();
            config.jellyfin.user_id.clear();
            if let Err(err) = config.save(&self.config_path) {
                tracing::warn!(%err, "failed to persist cleared session");
            }
        }
        tokio::task::spawn_blocking(|| {
            if let Err(err) = crate::secrets::delete(crate::secrets::JELLYFIN_TOKEN) {
                tracing::warn!(%err, "failed to delete expired token from keyring");
            }
        });
        self.auth_gen += 1;
        let mut form = self.login_form();
        form.error = Some("session expired, please log in again".into());
        self.screen = Screen::Login(form);
    }

    fn on_auth_done(&mut self, result: Result<Client, crate::jellyfin::Error>) {
        match result {
            Ok(client) => {
                let from_form = match &self.screen {
                    Screen::Login(form) => Some((form.host(), form.username(), form.password())),
                    _ => None,
                };
                self.persist_auth(
                    &client,
                    from_form
                        .as_ref()
                        .map(|(h, u, p)| (h.as_str(), u.as_str(), p.as_str())),
                );
                let plain_http = crate::jellyfin::url::is_plain_http(&client.host);
                let mut browse = Browse::new(client, self.sender.clone(), self.browse_gen.clone());
                if plain_http {
                    // The login form warns about this too, but auto-login
                    // (stored host + token) never shows the form.
                    browse.notice =
                        Some("connected over plain http://; traffic is unencrypted".into());
                }
                self.screen = Screen::Browse(browse);
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

    fn is_configured(&self) -> bool {
        let config = self.config.lock().unwrap();
        !config.jellyfin.host.is_empty() && !config.jellyfin.username.is_empty()
    }

    fn on_removed(&mut self) {
        // The supervisor owns mpv: `stop()` quits it, cleans the IPC socket
        // and flushes the final playback report on its own.
        if let Some(player) = self.player.take() {
            player.stop();
        }
        self.pending_play = None;
        // Invalidate everything in flight: the auth attempt, the stopped
        // player's trailing events, and (via the dropped Browse) its fetches.
        self.auth_gen += 1;
        self.player_gen += 1;
        self.screen = Screen::Boot;
        // Any pending reauth signal referred to the removed identity.
        self.seen_reauth = self.reauth.load(Ordering::Relaxed);
    }

    fn activate(&mut self) {
        if let Screen::Boot = self.screen {
            // The save that (re-)configured this backend also bumped the
            // reauth signal; consume it, or the next activate would reconnect
            // a session that is already fresh.
            self.seen_reauth = self.reauth.load(Ordering::Relaxed);
            let creds = self.credentials();
            if !creds.host.is_empty() && !creds.username.is_empty() {
                self.screen = Screen::Connecting;
                self.spawn_connect(creds, true);
            } else {
                self.screen = Screen::Login(self.login_form());
            }
            return;
        }

        // The Settings tab re-authenticated Jellyfin while we were away, so the
        // token this session holds is now stale. Reconnect with the freshly
        // stored one so the user isn't sent back to the login form (and asked
        // for the password a second time). Only a live Browse is rescued here;
        // other screens are left to their own auth path.
        let reauth = self.reauth.load(Ordering::Relaxed);
        if reauth != self.seen_reauth {
            self.seen_reauth = reauth;
            if matches!(self.screen, Screen::Browse(_)) {
                self.screen = Screen::Connecting;
                let creds = self.credentials();
                self.spawn_connect(creds, true);
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<ShellRequest> {
        // The replace-playback prompt is modal. Only y/n act, per the
        // `prompt::draw_confirm` key contract: Enter opened the prompt, so
        // honouring it here would let a double-tap replace by accident.
        if self.pending_play.is_some() {
            if crate::app::modified_char(&key) {
                return None;
            }
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let item = self.pending_play.take().unwrap();
                    if let Some(old) = self.player.take() {
                        old.stop();
                    }
                    self.start_playback(item);
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.pending_play = None;
                }
                _ => {}
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
                match form.on_key(key) {
                    Some(LoginAction::Submit) => {
                        form.busy = true;
                        form.error = None;
                        let (host, username, password) =
                            (form.host(), form.username(), form.password());
                        let mut creds = self.credentials();
                        creds.host = host;
                        creds.username = username;
                        creds.password = password;
                        // Fresh username/password auth: no stored token or user id.
                        creds.user_id = String::new();
                        self.spawn_connect(creds, false);
                    }
                    Some(LoginAction::Cancel) => {
                        // Release the form, then invalidate the in-flight attempt
                        // so its AuthDone is dropped on arrival (like the
                        // Connecting screen's Esc). Form use precedes the self
                        // borrow so this borrow-checks like the Submit arm.
                        form.busy = false;
                        self.auth_gen += 1;
                    }
                    None => {}
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
            // For both result kinds, the browse checks staleness first and
            // only reports a session expiry for a CURRENT-generation 401; a
            // stale 401 belongs to an old token and must not kick a freshly
            // re-authenticated session back to login. Results arriving while
            // no browse is on screen are inherently stale and dropped.
            Msg::ItemsLoaded { fetch_gen, result } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_items_loaded(fetch_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::SeriesLoaded { fetch_gen, result } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_series_loaded(fetch_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::LibraryItemsLoaded {
                fetch_gen,
                start_index,
                limit,
                result,
            } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_library_items_loaded(fetch_gen, start_index, limit, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::WatchedToggled { fetch_gen, result } => {
                if let Screen::Browse(browse) = &mut self.screen
                    && browse.on_watched_toggled(fetch_gen, result)
                {
                    self.on_session_expired();
                }
            }
            Msg::Player { player_gen, event } => {
                if player_gen != self.player_gen {
                    return; // event from a player that was replaced
                }
                self.on_player_event(event);
            }
            Msg::KeyringError(message) => {
                tracing::warn!(message, "keyring problem");
                if let Screen::Browse(browse) = &mut self.screen {
                    browse.error = Some(message);
                }
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

    fn ready_to_quit(&self) -> bool {
        // The player clears on Exited, which the supervisor emits only after
        // mpv is gone and the final playback report has been flushed.
        self.player.is_none()
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
            ratatui::text::Span::styled(
                format!("  {timing}"),
                ratatui::style::Style::new().fg(theme::fg()),
            ),
            ratatui::text::Span::styled("  s: stop", theme::dim()),
        ]))
    }

    fn stop_player(&mut self) -> bool {
        if let Some(player) = &self.player {
            player.stop();
            // The replace prompt (only set while a player exists) asked whether
            // to replace this playback; stopping makes it meaningless, so
            // dismiss it too rather than leave a dangling modal over a stopped
            // player now that `s` reaches here without going through on_key.
            self.pending_play = None;
            true
        } else {
            false
        }
    }

    fn capturing_text(&self) -> bool {
        match &self.screen {
            Screen::Login(form) => !form.action_row_focused(),
            Screen::Browse(browse) => browse.input_focused(),
            Screen::Boot | Screen::Connecting => false,
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
        if self.pending_play.is_some() {
            prompt::draw_confirm(frame, area, "Replace current playback?");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A live Browse can't be built without a connected Client (the app layer
    // deliberately keeps Client out of tests, see browse.rs), so the consumer
    // side of the reauth signal is covered where it can be offline: the signal
    // is consumed exactly once and never disturbs a non-Browse screen. The
    // Browse -> Connecting rescue itself is exercised manually.
    #[test]
    fn reauth_signal_is_consumed_without_disturbing_non_browse() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let reauth = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut jf = JellyfinApp::new(
            Arc::new(Mutex::new(Config::default())),
            PathBuf::from("jellyfin-test-config.toml"),
            AppSender::new("jellyfin", tx),
            reauth.clone(),
        );
        // Sit on the login screen, as after a prior session-expiry.
        let form = jf.login_form();
        jf.screen = Screen::Login(form);

        // Settings re-authenticated while we were away; returning to the tab
        // must consume the signal but leave the login form in place.
        reauth.fetch_add(1, Ordering::Relaxed);
        jf.activate();
        assert!(
            matches!(jf.screen, Screen::Login(_)),
            "a non-Browse screen must be left alone"
        );
        assert_eq!(jf.seen_reauth, 1, "the signal must be consumed once");

        // Idempotent: with no new signal, activate is a no-op.
        jf.activate();
        assert!(matches!(jf.screen, Screen::Login(_)));
        assert_eq!(jf.seen_reauth, 1);
    }

    // The player-stop half of on_removed needs a real PlayerHandle (an mpv
    // child), so it is exercised manually, like the Browse rescue above.
    #[test]
    fn on_removed_resets_to_boot_and_consumes_reauth() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let reauth = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut config = Config::default();
        config.jellyfin.host = "https://jelly.example".into();
        config.jellyfin.username = "alice".into();
        let mut jf = JellyfinApp::new(
            Arc::new(Mutex::new(config)),
            PathBuf::from("jellyfin-test-config.toml"),
            AppSender::new("jellyfin", tx),
            reauth.clone(),
        );
        assert!(jf.is_configured());
        jf.screen = Screen::Login(jf.login_form());
        let auth_gen = jf.auth_gen;
        let player_gen = jf.player_gen;

        // Settings removed the backend and (on an earlier save) had bumped the
        // reauth signal; the reset must swallow it and invalidate in-flight work.
        jf.config.lock().unwrap().jellyfin.host.clear();
        reauth.fetch_add(1, Ordering::Relaxed);
        jf.on_removed();

        assert!(!jf.is_configured());
        assert!(matches!(jf.screen, Screen::Boot), "must return to Boot");
        assert!(jf.auth_gen > auth_gen, "in-flight auth must land stale");
        assert!(jf.player_gen > player_gen, "player events must land stale");
        assert_eq!(jf.seen_reauth, 1, "a pending reauth signal is swallowed");
    }
}
