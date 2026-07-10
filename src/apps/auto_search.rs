//! Background auto-search monitors, shared by the Radarr and Sonarr browse
//! screens.
//!
//! When a movie/series is added with "Search & download now?", the add itself
//! only creates the library record; the actual search runs as a *tracked*
//! server command (`MoviesSearch` / `SeriesSearch`). This module models one
//! such job: it is polled to completion and its outcome confirmed against the
//! download queue, so the user learns whether a release was grabbed or nothing
//! was found — instead of the add silently returning with no feedback.
//!
//! A [`Monitor`] runs on its own generation (allocated from the browse's shared
//! generation counter), independent of the view's `fetch_gen`, so it survives
//! navigating away and is never invalidated by a list refetch. The owning
//! browse holds a `Vec<Monitor>` (all pending adds are tracked concurrently),
//! feeds each one a 250ms [`tick`](Monitor::tick), and performs the side
//! effects a tick asks for (poll a command, refresh the queue). Messages whose
//! monitor has been cleared — or that belong to a superseded browse instance —
//! find no matching generation and are dropped.
//!
//! The queue-matching field differs per backend (Radarr keys on `movieId`,
//! Sonarr on `seriesId`), so the owning browse computes the new-item count and
//! passes it in; everything else here is backend-agnostic.

use std::collections::HashSet;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::ui::theme;

/// Command-status poll cadence, in 250ms ticks (~2s). Also the re-check
/// cadence while confirming the outcome against the queue.
const POLL_TICKS: u32 = 8;
/// How long to keep re-checking the queue after the search command completes
/// before concluding nothing was grabbed (~6s). Absorbs the lag between the
/// command reporting done and the grab surfacing in the queue.
const CONFIRM_GRACE_TICKS: u32 = 24;
/// How long a finished monitor's message lingers before it clears (~8s). The
/// toast-like lifetime of the terminal result.
const CLEAR_TICKS: u32 = 32;
/// Overall safety cap (~180s): a command stuck in `queued`/`started` can't keep
/// a monitor alive forever.
const TIMEOUT_TICKS: u32 = 720;

/// Where a monitor is in its lifecycle.
enum Phase {
    /// Waiting for the tracked command to be created (its id is not known yet).
    Starting,
    /// Command created; polling its status until it completes.
    Searching,
    /// Command completed; checking the queue for a freshly grabbed release.
    Confirming,
    /// Terminal. `message` is shown (prefixed with the app name) until the
    /// clear deadline; `ok` picks the style (a grab vs everything else).
    Done { message: String, ok: bool },
}

/// The side effect a [`tick`](Monitor::tick) asks the owning browse to perform.
pub enum TickAction {
    /// Nothing to do this tick.
    Idle,
    /// Poll this command id for its status.
    Poll(i64),
    /// Refresh the download queue (a confirming re-check).
    FetchQueue,
    /// The monitor has expired; the owner should drop it.
    Drop,
}

/// One tracked auto-search for a just-added movie or series.
pub struct Monitor {
    /// This monitor's generation, matched against incoming `AutoSearch*`
    /// messages; unique across browse instances (drawn from the shared counter).
    pub generation: u64,
    /// The added movie/series id, used to match its queue items.
    pub target_id: i64,
    title: String,
    command_id: Option<i64>,
    /// Queue item ids already present for `target_id` when the search started,
    /// so a pre-existing download isn't mistaken for this search's result.
    baseline: HashSet<i64>,
    phase: Phase,
    /// Ticks since creation, for the overall timeout.
    age_ticks: u32,
    /// Ticks since entering the current phase, for poll cadence / grace / clear.
    phase_ticks: u32,
    /// A status poll is in flight; polls never overlap.
    poll_inflight: bool,
}

impl Monitor {
    pub fn new(generation: u64, target_id: i64, title: String, baseline: HashSet<i64>) -> Self {
        Self {
            generation,
            target_id,
            title,
            command_id: None,
            baseline,
            phase: Phase::Starting,
            age_ticks: 0,
            phase_ticks: 0,
            poll_inflight: false,
        }
    }

    /// True if the queue item with this id was already present when the search
    /// started (so it is not attributable to this search).
    pub fn is_baseline(&self, queue_item_id: i64) -> bool {
        self.baseline.contains(&queue_item_id)
    }

    /// Not yet in a terminal state.
    pub fn is_active(&self) -> bool {
        !matches!(self.phase, Phase::Done { .. })
    }

    /// The tracked command was created: start polling it.
    pub fn started(&mut self, command_id: i64) {
        if matches!(self.phase, Phase::Starting) {
            self.command_id = Some(command_id);
            self.enter(Phase::Searching);
        }
    }

    /// The tracked command could not be created: terminal failure.
    pub fn failed_to_start(&mut self) {
        self.enter(Phase::Done {
            message: format!("search failed for {}", self.title),
            ok: false,
        });
    }

    /// A status poll landed. Returns true if the command completed (so the
    /// owner should refresh the queue to confirm the outcome now).
    pub fn polled(&mut self, status: Option<&str>) -> bool {
        self.poll_inflight = false;
        if !matches!(self.phase, Phase::Searching) {
            return false; // a late poll after we already moved on
        }
        match status {
            Some("completed") => {
                self.enter(Phase::Confirming);
                true
            }
            Some("failed") | Some("aborted") | Some("cancelled") => {
                self.enter(Phase::Done {
                    message: format!("search failed for {}", self.title),
                    ok: false,
                });
                false
            }
            // queued / started / unknown: keep waiting.
            _ => false,
        }
    }

    /// Clear the in-flight flag after a poll that errored transiently, so the
    /// next cadence tick retries (the overall timeout backs this up).
    pub fn clear_poll_inflight(&mut self) {
        self.poll_inflight = false;
    }

    /// Advance one 250ms tick. `new_items` is how many queue items for this
    /// target were not present when the search started (the owner computes it
    /// from the current queue and `baseline`). Returns the side effect to run.
    pub fn tick(&mut self, new_items: usize) -> TickAction {
        self.age_ticks = self.age_ticks.saturating_add(1);
        self.phase_ticks = self.phase_ticks.saturating_add(1);
        // Only time out while still waiting on the command; Confirming is
        // already bounded by the grace window and shouldn't be pre-empted by a
        // slow command that has since finished.
        if matches!(self.phase, Phase::Starting | Phase::Searching)
            && self.age_ticks >= TIMEOUT_TICKS
        {
            self.enter(Phase::Done {
                message: format!("{}: search timed out, check the queue", self.title),
                ok: false,
            });
            return TickAction::Idle;
        }
        match self.phase {
            Phase::Starting => TickAction::Idle,
            Phase::Searching => match self.command_id {
                Some(id) if !self.poll_inflight && self.phase_ticks.is_multiple_of(POLL_TICKS) => {
                    self.poll_inflight = true;
                    TickAction::Poll(id)
                }
                _ => TickAction::Idle,
            },
            Phase::Confirming => {
                if new_items >= 1 {
                    let message = if new_items == 1 {
                        format!("{} added to queue", self.title)
                    } else {
                        format!("{}: {new_items} downloads added", self.title)
                    };
                    self.enter(Phase::Done { message, ok: true });
                    TickAction::Idle
                } else if self.phase_ticks >= CONFIRM_GRACE_TICKS {
                    // "yet": the search finished without a grab surfacing in the
                    // grace window, but a slow or late grab could still land.
                    self.enter(Phase::Done {
                        message: format!("no releases found yet for {}", self.title),
                        ok: false,
                    });
                    TickAction::Idle
                } else if self.phase_ticks.is_multiple_of(POLL_TICKS) {
                    TickAction::FetchQueue
                } else {
                    TickAction::Idle
                }
            }
            Phase::Done { .. } => {
                if self.phase_ticks >= CLEAR_TICKS {
                    TickAction::Drop
                } else {
                    TickAction::Idle
                }
            }
        }
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.phase_ticks = 0;
    }
}

/// The one status-bar line summarising a browse's monitors, or `None` when
/// there are none. The newest still-visible terminal result wins (with a count
/// of any searches still running); otherwise a live "searching" summary.
pub fn status_line(app: &str, monitors: &[Monitor]) -> Option<Line<'static>> {
    if monitors.is_empty() {
        return None;
    }
    let active = monitors.iter().filter(|m| m.is_active()).count();
    // The most recently finished monitor has the smallest phase_ticks.
    let newest_done = monitors
        .iter()
        .filter_map(|m| match &m.phase {
            Phase::Done { message, ok } => Some((m.phase_ticks, message.clone(), *ok)),
            _ => None,
        })
        .min_by_key(|(ticks, _, _)| *ticks);
    if let Some((_, message, ok)) = newest_done {
        let style = if ok {
            theme::accent()
        } else {
            Style::new().fg(theme::fg())
        };
        let mut spans = vec![
            Span::styled(format!(" {app}: "), theme::dim()),
            Span::styled(message, style),
        ];
        if active > 0 {
            spans.push(Span::styled(
                format!("  (+{active} searching)"),
                theme::dim(),
            ));
        }
        return Some(Line::from(spans));
    }
    if active == 0 {
        return None;
    }
    let first = monitors
        .iter()
        .find(|m| m.is_active())
        .map(|m| m.title.as_str())
        .unwrap_or_default();
    let text = if active == 1 {
        format!(" {app}: searching for {first}...")
    } else {
        format!(" {app}: searching {active} titles ({first}...)")
    };
    Some(Line::styled(text, theme::accent()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> Monitor {
        Monitor::new(1, 42, "Dune (2021)".into(), HashSet::new())
    }

    #[test]
    fn added_when_a_new_queue_item_appears() {
        let mut m = monitor();
        m.started(7);
        assert!(m.polled(Some("completed"))); // -> Confirming, asks for a queue refresh
        // A new (non-baseline) queue item confirms the grab on the next tick.
        match m.tick(1) {
            TickAction::Idle => {}
            _ => panic!("expected idle after resolving"),
        }
        assert!(!m.is_active());
        let line = status_line("Radarr", std::slice::from_ref(&m)).unwrap();
        assert!(format!("{line:?}").contains("added to queue"));
    }

    #[test]
    fn nothing_found_after_the_grace_window() {
        let mut m = monitor();
        m.started(7);
        assert!(m.polled(Some("completed")));
        // No queue item ever appears; after the grace window it concludes empty.
        let mut concluded = false;
        for _ in 0..CONFIRM_GRACE_TICKS + 1 {
            m.tick(0);
            if !m.is_active() {
                concluded = true;
                break;
            }
        }
        assert!(concluded);
        let line = status_line("Radarr", std::slice::from_ref(&m)).unwrap();
        assert!(format!("{line:?}").contains("no releases found"));
    }

    #[test]
    fn polls_on_cadence_while_searching() {
        let mut m = monitor();
        m.started(7);
        // No poll until the cadence tick; then exactly one, then none until the
        // in-flight flag is cleared.
        for _ in 0..POLL_TICKS - 1 {
            assert!(matches!(m.tick(0), TickAction::Idle));
        }
        assert!(matches!(m.tick(0), TickAction::Poll(7)));
        assert!(matches!(m.tick(0), TickAction::Idle)); // poll_inflight blocks a second
    }

    #[test]
    fn done_monitor_drops_after_clearing() {
        let mut m = monitor();
        m.failed_to_start();
        assert!(!m.is_active());
        let mut dropped = false;
        for _ in 0..CLEAR_TICKS + 1 {
            if matches!(m.tick(0), TickAction::Drop) {
                dropped = true;
                break;
            }
        }
        assert!(dropped);
    }

    #[test]
    fn times_out_when_the_command_never_completes() {
        let mut m = monitor();
        m.started(7);
        let mut timed_out = false;
        for _ in 0..TIMEOUT_TICKS + 1 {
            m.clear_poll_inflight(); // pretend every poll comes back "started"
            m.tick(0);
            if !m.is_active() {
                timed_out = true;
                break;
            }
        }
        assert!(timed_out);
        let line = status_line("Radarr", std::slice::from_ref(&m)).unwrap();
        assert!(format!("{line:?}").contains("timed out"));
    }

    #[test]
    fn summarises_multiple_active_searches() {
        let a = Monitor::new(1, 1, "Dune".into(), HashSet::new());
        let b = Monitor::new(2, 2, "Tenet".into(), HashSet::new());
        let line = status_line("Radarr", &[a, b]).unwrap();
        assert!(format!("{line:?}").contains("searching 2 titles"));
    }
}
