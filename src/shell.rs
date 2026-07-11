use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;

use crate::app::{MediaApp, ShellRequest};
use crate::config::Config;
use crate::event::Event;
use crate::ui::theme;

pub struct Shell {
    apps: Vec<Box<dyn MediaApp>>,
    active: usize,
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    rx: mpsc::UnboundedReceiver<Event>,
    should_quit: bool,
}

impl Shell {
    pub fn new(
        apps: Vec<Box<dyn MediaApp>>,
        config: Arc<Mutex<Config>>,
        config_path: PathBuf,
        rx: mpsc::UnboundedReceiver<Event>,
    ) -> Self {
        let last_app = config.lock().unwrap().last_app.clone();
        let active = last_app
            .and_then(|id| apps.iter().position(|app| app.id() == id))
            .unwrap_or(0);
        Self {
            apps,
            active,
            config,
            config_path,
            rx,
            should_quit: false,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.apps[self.active].activate();
        loop {
            terminal.draw(|frame| render(frame, &mut self.apps, self.active))?;
            let Some(event) = self.rx.recv().await else {
                break;
            };
            match event {
                // Held keys arrive as `Repeat` on Windows and kitty-protocol
                // terminals; treat them like `Press` so auto-repeat drives the
                // UI. `Release` is still dropped (it would double every press).
                Event::Term(TermEvent::Key(key))
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    self.on_key(key);
                }
                Event::Term(_) => {}
                Event::Tick => {
                    for app in &mut self.apps {
                        app.on_tick();
                    }
                }
                Event::App(app_event) => {
                    if let Some(app) = self.apps.iter_mut().find(|app| app.id() == app_event.app) {
                        app.on_event(app_event.payload);
                    }
                }
            }
            if self.should_quit {
                let mut needs_grace = false;
                for app in &mut self.apps {
                    needs_grace |= app.on_quit();
                }
                if needs_grace {
                    self.drain_shutdown().await;
                }
                break;
            }
        }
        Ok(())
    }

    /// Keep routing app events until every app has finished its shutdown
    /// work (e.g. the player's Exited after mpv quit and the final playback
    /// report) or the deadline passes. A fixed sleep would either waste time
    /// or cut the final report short; this leaves as soon as the work is
    /// actually done.
    async fn drain_shutdown(&mut self) {
        // Match the supervisor's own worst case (mpv-quit grace + report
        // flush) so a slow quit is not abandoned mid-flush, which would leak
        // the IPC socket and lose the final playback report. Normal quits still
        // exit the loop the instant every app reports `ready_to_quit`.
        let deadline = tokio::time::Instant::now() + crate::player::SHUTDOWN_BUDGET;
        while !self.apps.iter().all(|app| app.ready_to_quit()) {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(Event::App(app_event))) => {
                    if let Some(app) = self.apps.iter_mut().find(|app| app.id() == app_event.app) {
                        app.on_event(app_event.payload);
                    }
                }
                // Ignore keys and ticks; the UI is already done.
                Ok(Some(_)) => {}
                // Channel closed or deadline reached: stop waiting.
                Ok(None) | Err(_) => break,
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Left => {
                    let target = (self.active + self.apps.len() - 1) % self.apps.len();
                    self.switch_to(target);
                    return;
                }
                KeyCode::Right => {
                    let target = (self.active + 1) % self.apps.len();
                    self.switch_to(target);
                    return;
                }
                KeyCode::Char(c @ '1'..='9') => {
                    let target = c as usize - '1' as usize;
                    if target < self.apps.len() {
                        self.switch_to(target);
                    }
                    return;
                }
                _ => {}
            }
        }
        // Global stop: the now-playing bar (and its `s: stop` hint) shows from
        // every tab, so `s` must stop playback from every tab too. Skip it when
        // the active app is capturing text, so it never eats a keystroke meant
        // for a search or credential field, and fall through otherwise so a
        // real `s` binding still reaches the active app.
        if key.code == KeyCode::Char('s')
            && key.modifiers.is_empty()
            && !self.apps[self.active].capturing_text()
        {
            let mut stopped = false;
            for app in &mut self.apps {
                stopped |= app.stop_player();
            }
            if stopped {
                return;
            }
        }
        if let Some(request) = self.apps[self.active].on_key(key) {
            match request {
                ShellRequest::Quit => self.should_quit = true,
            }
        }
    }

    fn switch_to(&mut self, target: usize) {
        if target == self.active {
            return;
        }
        self.apps[self.active].deactivate();
        self.active = target;
        let app = &mut self.apps[target];
        app.activate();
        let mut config = self.config.lock().unwrap();
        config.last_app = Some(app.id().to_string());
        if let Err(err) = config.save(&self.config_path) {
            tracing::warn!(%err, "failed to persist last_app");
        }
    }
}

/// Upper bound on status-bar rows, so several apps reporting at once (e.g.
/// Jellyfin now-playing plus a Radarr and a Sonarr auto-search) can never crush
/// the body.
const STATUS_BAR_MAX_ROWS: usize = 3;

fn render(frame: &mut Frame, apps: &mut [Box<dyn MediaApp>], active: usize) {
    // Each app's status line gets its own row, so a now-playing bar and an
    // auto-search status show at the same time. Collected before the (mutable)
    // body draw, since `status_line` borrows the apps immutably.
    let status_lines = collect_status_lines(apps, active);
    let status_rows = status_lines.len().clamp(1, STATUS_BAR_MAX_ROWS) as u16;

    let [tabs_area, body_area, status_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(status_rows),
    ])
    .areas(frame.area());

    render_app_tabs(frame, tabs_area, apps, active);
    apps[active].draw(frame, body_area);
    render_status_bar(frame, status_area, status_lines, apps.len());
}

/// Every app's status line, active app first so its row leads, then the others
/// (so e.g. a now-playing bar stays visible from another tab). Capped at
/// [`STATUS_BAR_MAX_ROWS`].
fn collect_status_lines(apps: &[Box<dyn MediaApp>], active: usize) -> Vec<Line<'static>> {
    let active_id = apps[active].id();
    std::iter::once(&apps[active])
        .chain(apps.iter().filter(|app| app.id() != active_id))
        .filter_map(|app| app.status_line())
        .take(STATUS_BAR_MAX_ROWS)
        .collect()
}

fn render_app_tabs(frame: &mut Frame, area: Rect, apps: &[Box<dyn MediaApp>], active: usize) {
    let mut spans = vec![Span::raw(" ")];
    for (i, app) in apps.iter().enumerate() {
        let style = if i == active {
            theme::selected()
        } else {
            Style::new().fg(theme::fg())
        };
        spans.push(Span::styled(format!(" {} ", app.title()), style));
        spans.push(Span::raw("  "));
    }
    let [tabs_row, rule_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    Line::from(spans).render(tabs_row, frame.buffer_mut());
    Line::styled("─".repeat(rule_row.width as usize), theme::dim())
        .render(rule_row, frame.buffer_mut());
}

fn render_status_bar(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>, app_count: usize) {
    if lines.is_empty() {
        Line::styled(
            format!(" ctrl+←/→ or ctrl+1..{app_count}: switch app"),
            theme::dim(),
        )
        .render(area, frame.buffer_mut());
        return;
    }
    // One row per line; the area was sized to match in `render`.
    let rows = Layout::vertical(vec![Constraint::Length(1); lines.len()]).split(area);
    for (line, row) in lines.into_iter().zip(rows.iter()) {
        line.render(*row, frame.buffer_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::app::AppId;

    /// A stand-in app that records whether its player was asked to stop and any
    /// key routed to its `on_key`, so the shell's global-`s` handling can be
    /// exercised without real apps.
    struct MockApp {
        id: AppId,
        capturing: bool,
        has_player: bool,
        stopped: Arc<AtomicBool>,
        keys: Arc<Mutex<Vec<KeyEvent>>>,
    }

    impl MediaApp for MockApp {
        fn id(&self) -> AppId {
            self.id
        }
        fn title(&self) -> &'static str {
            self.id
        }
        fn on_key(&mut self, key: KeyEvent) -> Option<ShellRequest> {
            self.keys.lock().unwrap().push(key);
            None
        }
        fn on_event(&mut self, _payload: Box<dyn Any + Send>) {}
        fn stop_player(&mut self) -> bool {
            if self.has_player {
                self.stopped.store(true, Ordering::SeqCst);
                true
            } else {
                false
            }
        }
        fn capturing_text(&self) -> bool {
            self.capturing
        }
        fn draw(&mut self, _frame: &mut Frame, _area: Rect) {}
    }

    fn mock(
        id: AppId,
        capturing: bool,
        has_player: bool,
    ) -> (MockApp, Arc<AtomicBool>, Arc<Mutex<Vec<KeyEvent>>>) {
        let stopped = Arc::new(AtomicBool::new(false));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let app = MockApp {
            id,
            capturing,
            has_player,
            stopped: stopped.clone(),
            keys: keys.clone(),
        };
        (app, stopped, keys)
    }

    fn shell(apps: Vec<Box<dyn MediaApp>>) -> Shell {
        // A default config has no `last_app`, so the first app is active.
        let (_tx, rx) = mpsc::unbounded_channel();
        Shell::new(
            apps,
            Arc::new(Mutex::new(Config::default())),
            PathBuf::from("shell-test-config.toml"),
            rx,
        )
    }

    fn press_s() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)
    }

    #[test]
    fn s_stops_a_background_player_from_another_tab() {
        // Active tab owns no player and is not capturing text; the player lives
        // on a background tab, exactly the case the feature fixes.
        let (active, _active_stopped, active_keys) = mock("active", false, false);
        let (player, player_stopped, _player_keys) = mock("player", false, true);
        let mut shell = shell(vec![Box::new(active), Box::new(player)]);

        shell.on_key(press_s());

        assert!(
            player_stopped.load(Ordering::SeqCst),
            "the background player should have been stopped"
        );
        assert!(
            active_keys.lock().unwrap().is_empty(),
            "a consumed `s` must not also reach the active app"
        );
    }

    #[test]
    fn s_is_left_for_typing_when_active_app_captures_text() {
        let (active, _active_stopped, active_keys) = mock("active", true, false);
        let (player, player_stopped, _player_keys) = mock("player", false, true);
        let mut shell = shell(vec![Box::new(active), Box::new(player)]);

        shell.on_key(press_s());

        assert!(
            !player_stopped.load(Ordering::SeqCst),
            "playback must not stop while the active app is capturing text"
        );
        assert_eq!(
            active_keys.lock().unwrap().len(),
            1,
            "`s` should fall through to the active app as text"
        );
    }

    #[test]
    fn s_falls_through_when_nothing_is_playing() {
        let (active, _stopped, active_keys) = mock("active", false, false);
        let mut shell = shell(vec![Box::new(active)]);

        shell.on_key(press_s());

        assert_eq!(
            active_keys.lock().unwrap().len(),
            1,
            "with no player to stop, `s` reaches the active app"
        );
    }
}
