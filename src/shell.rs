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
    /// Which apps currently deserve a tab, parallel to `apps`. Refreshed once
    /// per loop iteration so a backend configured (or removed) in Settings
    /// shows up (or disappears) on the very next frame.
    visible: Vec<bool>,
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    rx: mpsc::UnboundedReceiver<Event>,
    should_quit: bool,
}

fn compute_visible(apps: &[Box<dyn MediaApp>]) -> Vec<bool> {
    apps.iter().map(|app| app.is_configured()).collect()
}

impl Shell {
    pub fn new(
        apps: Vec<Box<dyn MediaApp>>,
        config: Arc<Mutex<Config>>,
        config_path: PathBuf,
        rx: mpsc::UnboundedReceiver<Event>,
    ) -> Self {
        let visible = compute_visible(&apps);
        let last_app = config.lock().unwrap().last_app.clone();
        // `last_app` may point at an app whose backend was since removed;
        // fall back to the first visible tab (Settings is always visible, so
        // one exists — and on a fresh config it is the only one).
        let active = last_app
            .and_then(|id| apps.iter().position(|app| app.id() == id))
            .filter(|&i| visible[i])
            .unwrap_or_else(|| visible.iter().position(|&v| v).unwrap_or(0));
        Self {
            apps,
            active,
            visible,
            config,
            config_path,
            rx,
            should_quit: false,
        }
    }

    /// The tab-bar order: indices of the visible apps. `ctrl+N` targets the
    /// Nth entry of this list, so the shortcuts renumber as tabs appear.
    fn visible_indices(&self) -> Vec<usize> {
        (0..self.apps.len()).filter(|&i| self.visible[i]).collect()
    }

    /// Re-derive tab visibility and reset any app whose configuration was
    /// removed while it was live (it can never be activated again, and e.g. a
    /// removed Jellyfin must stop its player now, not on next activation).
    fn refresh_visibility(&mut self) {
        let new = compute_visible(&self.apps);
        for (i, app) in self.apps.iter_mut().enumerate() {
            if self.visible[i] && !new[i] {
                app.on_removed();
            }
        }
        self.visible = new;
        if !self.visible[self.active] {
            // Unreachable through Settings-driven removal (Settings is the
            // active tab then, and it is always visible), but keep the
            // invariant that `active` is a visible tab.
            let fallback = self.visible.iter().position(|&v| v).unwrap_or(0);
            self.switch_to(fallback);
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.apps[self.active].activate();
        loop {
            self.refresh_visibility();
            terminal.draw(|frame| render(frame, &mut self.apps, self.active, &self.visible))?;
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
                KeyCode::Left | KeyCode::Right => {
                    let targets = self.visible_indices();
                    if targets.is_empty() {
                        return;
                    }
                    let pos = targets.iter().position(|&i| i == self.active).unwrap_or(0);
                    let step = if key.code == KeyCode::Left {
                        targets.len() - 1
                    } else {
                        1
                    };
                    self.switch_to(targets[(pos + step) % targets.len()]);
                    return;
                }
                KeyCode::Char(c @ '1'..='9') => {
                    let targets = self.visible_indices();
                    if let Some(&target) = targets.get(c as usize - '1' as usize) {
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

fn render(frame: &mut Frame, apps: &mut [Box<dyn MediaApp>], active: usize, visible: &[bool]) {
    // Each app's status line gets its own row, so a now-playing bar and an
    // auto-search status show at the same time. Collected before the (mutable)
    // body draw, since `status_line` borrows the apps immutably.
    let status_lines = collect_status_lines(apps, active, visible);
    let status_rows = status_lines.len().clamp(1, STATUS_BAR_MAX_ROWS) as u16;

    let [tabs_area, body_area, status_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(status_rows),
    ])
    .areas(frame.area());

    render_app_tabs(frame, tabs_area, apps, active, visible);
    apps[active].draw(frame, body_area);
    let visible_count = visible.iter().filter(|&&v| v).count();
    render_status_bar(frame, status_area, status_lines, visible_count);
}

/// Every visible app's status line, active app first so its row leads, then
/// the others (so e.g. a now-playing bar stays visible from another tab).
/// Capped at [`STATUS_BAR_MAX_ROWS`].
fn collect_status_lines(
    apps: &[Box<dyn MediaApp>],
    active: usize,
    visible: &[bool],
) -> Vec<Line<'static>> {
    std::iter::once(active)
        .chain((0..apps.len()).filter(|&i| i != active))
        .filter(|&i| visible[i])
        .filter_map(|i| apps[i].status_line())
        .take(STATUS_BAR_MAX_ROWS)
        .collect()
}

fn render_app_tabs(
    frame: &mut Frame,
    area: Rect,
    apps: &[Box<dyn MediaApp>],
    active: usize,
    visible: &[bool],
) {
    let mut spans = vec![Span::raw(" ")];
    for (i, app) in apps.iter().enumerate() {
        if !visible[i] {
            continue;
        }
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

fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    visible_count: usize,
) {
    if lines.is_empty() {
        // With a single tab there is nothing to switch to; point first-run
        // users at Settings instead of advertising `ctrl+1..1`.
        let hint = if visible_count <= 1 {
            " configure a backend in the settings tab to add apps".to_string()
        } else {
            format!(" ctrl+←/→ or ctrl+1..{visible_count}: switch app")
        };
        Line::styled(hint, theme::dim()).render(area, frame.buffer_mut());
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

    /// A stand-in app that records whether its player was asked to stop, any
    /// key routed to its `on_key`, and `on_removed` calls, so the shell's
    /// global-`s` and tab-visibility handling can be exercised without real
    /// apps. `configured` is shared so a test can flip it mid-run, like a
    /// Settings save/removal would.
    struct MockApp {
        id: AppId,
        capturing: bool,
        has_player: bool,
        configured: Arc<AtomicBool>,
        removed: Arc<Mutex<usize>>,
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
        fn is_configured(&self) -> bool {
            self.configured.load(Ordering::SeqCst)
        }
        fn on_removed(&mut self) {
            *self.removed.lock().unwrap() += 1;
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

    struct MockHandles {
        configured: Arc<AtomicBool>,
        removed: Arc<Mutex<usize>>,
        stopped: Arc<AtomicBool>,
        keys: Arc<Mutex<Vec<KeyEvent>>>,
    }

    fn mock(id: AppId, capturing: bool, has_player: bool) -> (MockApp, MockHandles) {
        let handles = MockHandles {
            configured: Arc::new(AtomicBool::new(true)),
            removed: Arc::new(Mutex::new(0)),
            stopped: Arc::new(AtomicBool::new(false)),
            keys: Arc::new(Mutex::new(Vec::new())),
        };
        let app = MockApp {
            id,
            capturing,
            has_player,
            configured: handles.configured.clone(),
            removed: handles.removed.clone(),
            stopped: handles.stopped.clone(),
            keys: handles.keys.clone(),
        };
        (app, handles)
    }

    fn shell(apps: Vec<Box<dyn MediaApp>>) -> Shell {
        shell_with_config(apps, Config::default())
    }

    fn shell_with_config(apps: Vec<Box<dyn MediaApp>>, config: Config) -> Shell {
        // `switch_to` persists `last_app`, so point the config at a scratch
        // file instead of the working directory.
        let (_tx, rx) = mpsc::unbounded_channel();
        let path =
            std::env::temp_dir().join(format!("isamedia-shell-test-{}.toml", std::process::id()));
        Shell::new(apps, Arc::new(Mutex::new(config)), path, rx)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn press_s() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)
    }

    #[test]
    fn s_stops_a_background_player_from_another_tab() {
        // Active tab owns no player and is not capturing text; the player lives
        // on a background tab, exactly the case the feature fixes.
        let (active, active_handles) = mock("active", false, false);
        let (player, player_handles) = mock("player", false, true);
        let mut shell = shell(vec![Box::new(active), Box::new(player)]);

        shell.on_key(press_s());

        assert!(
            player_handles.stopped.load(Ordering::SeqCst),
            "the background player should have been stopped"
        );
        assert!(
            active_handles.keys.lock().unwrap().is_empty(),
            "a consumed `s` must not also reach the active app"
        );
    }

    #[test]
    fn s_is_left_for_typing_when_active_app_captures_text() {
        let (active, active_handles) = mock("active", true, false);
        let (player, player_handles) = mock("player", false, true);
        let mut shell = shell(vec![Box::new(active), Box::new(player)]);

        shell.on_key(press_s());

        assert!(
            !player_handles.stopped.load(Ordering::SeqCst),
            "playback must not stop while the active app is capturing text"
        );
        assert_eq!(
            active_handles.keys.lock().unwrap().len(),
            1,
            "`s` should fall through to the active app as text"
        );
    }

    #[test]
    fn s_falls_through_when_nothing_is_playing() {
        let (active, active_handles) = mock("active", false, false);
        let mut shell = shell(vec![Box::new(active)]);

        shell.on_key(press_s());

        assert_eq!(
            active_handles.keys.lock().unwrap().len(),
            1,
            "with no player to stop, `s` reaches the active app"
        );
    }

    #[test]
    fn ctrl_digits_map_to_visible_tabs_in_order() {
        // Three apps with the first hidden: ctrl+1 is the second app, ctrl+2
        // the third, ctrl+3 nothing (only two visible tabs).
        let (a, a_handles) = mock("a", false, false);
        let (b, _b_handles) = mock("b", false, false);
        let (c, _c_handles) = mock("c", false, false);
        a_handles.configured.store(false, Ordering::SeqCst);
        let mut shell = shell(vec![Box::new(a), Box::new(b), Box::new(c)]);
        shell.refresh_visibility();
        assert_eq!(shell.active, 1, "initial tab must skip the hidden app");

        shell.on_key(ctrl(KeyCode::Char('2')));
        assert_eq!(shell.active, 2, "ctrl+2 targets the second visible tab");
        shell.on_key(ctrl(KeyCode::Char('1')));
        assert_eq!(shell.active, 1, "ctrl+1 targets the first visible tab");
        shell.on_key(ctrl(KeyCode::Char('3')));
        assert_eq!(shell.active, 1, "a digit past the visible tabs is ignored");
    }

    #[test]
    fn ctrl_arrows_cycle_visible_tabs_only() {
        // The middle app is hidden: arrows hop straight between the outer two.
        let (a, _a_handles) = mock("a", false, false);
        let (b, b_handles) = mock("b", false, false);
        let (c, _c_handles) = mock("c", false, false);
        b_handles.configured.store(false, Ordering::SeqCst);
        let mut shell = shell(vec![Box::new(a), Box::new(b), Box::new(c)]);
        shell.refresh_visibility();

        shell.on_key(ctrl(KeyCode::Right));
        assert_eq!(shell.active, 2, "Right must skip the hidden middle tab");
        shell.on_key(ctrl(KeyCode::Right));
        assert_eq!(shell.active, 0, "Right wraps over visible tabs");
        shell.on_key(ctrl(KeyCode::Left));
        assert_eq!(shell.active, 2, "Left wraps over visible tabs");
    }

    #[test]
    fn hidden_last_app_falls_back_to_first_visible() {
        let (a, a_handles) = mock("a", false, false);
        let (b, _b_handles) = mock("b", false, false);
        a_handles.configured.store(false, Ordering::SeqCst);
        let config = Config {
            last_app: Some("a".into()),
            ..Config::default()
        };
        let shell = shell_with_config(vec![Box::new(a), Box::new(b)], config);
        assert_eq!(
            shell.active, 1,
            "a last_app pointing at a hidden tab must fall back to the first visible one"
        );
    }

    #[test]
    fn visibility_loss_fires_on_removed_once() {
        let (a, _a_handles) = mock("a", false, false);
        let (b, b_handles) = mock("b", false, false);
        let mut shell = shell(vec![Box::new(a), Box::new(b)]);
        shell.refresh_visibility();
        assert_eq!(shell.visible_indices(), vec![0, 1]);

        b_handles.configured.store(false, Ordering::SeqCst);
        shell.refresh_visibility();
        shell.refresh_visibility();
        assert_eq!(
            *b_handles.removed.lock().unwrap(),
            1,
            "on_removed fires exactly once per configured -> unconfigured flip"
        );
        assert_eq!(shell.visible_indices(), vec![0]);

        // Re-configuring reveals the tab again without another reset.
        b_handles.configured.store(true, Ordering::SeqCst);
        shell.refresh_visibility();
        assert_eq!(shell.visible_indices(), vec![0, 1]);
        assert_eq!(*b_handles.removed.lock().unwrap(), 1);
    }
}
