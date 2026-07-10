use std::any::Any;
use std::time::Duration;

use ratatui::crossterm::event as term_event;
use tokio::sync::mpsc;

use crate::app::AppId;

/// Everything the main loop reacts to. All state mutation happens
/// synchronously in the shell loop; background tasks only ever send events.
pub enum Event {
    Term(term_event::Event),
    Tick,
    App(AppEvent),
}

/// A message for a specific app. The payload is type-erased so the shell and
/// event plumbing never need to know about app-internal message types; the
/// owning app downcasts it back in `MediaApp::on_event`.
pub struct AppEvent {
    pub app: AppId,
    pub payload: Box<dyn Any + Send>,
}

/// Handle apps use to send messages back to themselves from spawned tasks.
#[derive(Clone)]
pub struct AppSender {
    app: AppId,
    tx: mpsc::UnboundedSender<Event>,
}

impl AppSender {
    pub fn new(app: AppId, tx: mpsc::UnboundedSender<Event>) -> Self {
        Self { app, tx }
    }

    pub fn send<M: Any + Send>(&self, msg: M) {
        let _ = self.tx.send(Event::App(AppEvent {
            app: self.app,
            payload: Box::new(msg),
        }));
    }
}

/// Terminal input is read on a plain blocking thread; `crossterm::event::read`
/// has no async story without extra features, and a thread keeps the
/// dependency surface small.
pub fn spawn_input_thread(tx: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || {
        while let Ok(ev) = term_event::read() {
            if tx.send(Event::Term(ev)).is_err() {
                break;
            }
        }
    });
}

pub fn spawn_tick_task(tx: mpsc::UnboundedSender<Event>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        loop {
            interval.tick().await;
            if tx.send(Event::Tick).is_err() {
                break;
            }
        }
    });
}
