//! The Downloads (queue) view, shared by the Sonarr and Radarr tabs. The
//! selection state, the delete-prompt key routing, and the rendering are all
//! backend-agnostic: they work off `[QueueItem]` plus a set of marked ids.
//! Each app supplies only the per-item name (and, for Sonarr, the episode
//! code/title) through the `row` closure passed to [`Downloads::draw_list`].

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::arr::display;
use crate::arr::models::QueueItem;
use crate::ui::text::truncate;
use crate::ui::{list, prompt, theme};

/// Right-hand column budget of a row: the status label and a gap (fits
/// "downloading 100% (warning)").
const STATUS_WIDTH: u16 = 27;

/// The delete-downloads prompt: modal while set. Captures the target ids at
/// open time so a background queue poll can't shift what gets removed, and
/// carries the two server options the user toggles before committing.
struct DeletePrompt {
    ids: Vec<i64>,
    remove_from_client: bool,
    blocklist: bool,
    /// Which toggle is focused: 0 = remove from client, 1 = blocklist.
    focus: usize,
}

/// What routing a key through the open delete prompt resolved to.
pub enum DeleteAction {
    /// Key consumed; nothing for the app to do.
    Consumed,
    /// The user cancelled; the prompt is now closed.
    Cancelled,
    /// The user committed; the app should issue the removal with these options.
    Commit {
        ids: Vec<i64>,
        remove_from_client: bool,
        blocklist: bool,
    },
}

/// One rendered row's per-item, backend-specific pieces. Radarr fills only
/// `name`; Sonarr also fills `code` (SxxExx) and `episode_title`.
#[derive(Default)]
pub struct DownloadRow {
    pub name: String,
    pub code: Option<String>,
    pub episode_title: Option<String>,
}

/// Selection + prompt state for the Downloads level. The app owns one of these
/// and drives it from its key handler, queue poll, and draw.
#[derive(Default)]
pub struct Downloads {
    pub cursor: usize,
    marks: HashSet<i64>,
    prompt: Option<DeletePrompt>,
}

impl Downloads {
    /// Reset on entering or leaving the level.
    pub fn reset(&mut self) {
        self.cursor = 0;
        self.marks.clear();
        self.prompt = None;
    }

    pub fn marked(&self) -> usize {
        self.marks.len()
    }

    pub fn is_prompting(&self) -> bool {
        self.prompt.is_some()
    }

    pub fn clear_marks(&mut self) {
        self.marks.clear();
    }

    /// Keep the cursor in range; the background poll can shrink the queue.
    pub fn clamp(&mut self, len: usize) {
        self.cursor = self.cursor.min(len.saturating_sub(1));
    }

    /// Drop marks for downloads that have left the queue, so the count and the
    /// empty-selection fallback stay honest.
    pub fn prune(&mut self, queue: &[QueueItem]) {
        self.marks
            .retain(|id| queue.iter().any(|item| item.id == *id));
    }

    /// Toggle the mark on the highlighted row.
    pub fn toggle_mark(&mut self, queue: &[QueueItem]) {
        if let Some(item) = queue.get(self.cursor)
            && !self.marks.remove(&item.id)
        {
            self.marks.insert(item.id);
        }
    }

    /// Open the delete prompt targeting the marked ids still in the queue, or
    /// the highlighted row when nothing is marked. A no-op on an empty queue.
    pub fn begin_delete(&mut self, queue: &[QueueItem]) {
        let ids: Vec<i64> = if self.marks.is_empty() {
            queue
                .get(self.cursor)
                .map(|item| vec![item.id])
                .unwrap_or_default()
        } else {
            queue
                .iter()
                .map(|item| item.id)
                .filter(|id| self.marks.contains(id))
                .collect()
        };
        if ids.is_empty() {
            return;
        }
        self.prompt = Some(DeletePrompt {
            ids,
            remove_from_client: true,
            blocklist: false,
            focus: 0,
        });
    }

    /// Route a key while the delete prompt is open. Returns what the app should
    /// do; `Commit` also closes the prompt.
    pub fn handle_prompt_key(&mut self, key: KeyEvent) -> DeleteAction {
        if self.prompt.is_none() {
            return DeleteAction::Consumed;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.focus = prompt.focus.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.focus = (prompt.focus + 1).min(1);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(prompt) = self.prompt.as_mut() {
                    if prompt.focus == 0 {
                        prompt.remove_from_client = !prompt.remove_from_client;
                    } else {
                        prompt.blocklist = !prompt.blocklist;
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(prompt) = self.prompt.take() {
                    return DeleteAction::Commit {
                        ids: prompt.ids,
                        remove_from_client: prompt.remove_from_client,
                        blocklist: prompt.blocklist,
                    };
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.prompt = None;
                return DeleteAction::Cancelled;
            }
            _ => {}
        }
        DeleteAction::Consumed
    }

    /// Draw the delete prompt over the current frame when it is open.
    pub fn draw_prompt(&self, frame: &mut Frame, area: Rect) {
        let Some(prompt) = &self.prompt else {
            return;
        };
        let title = format!("Remove {} download(s)?", prompt.ids.len());
        let options = [
            ("Remove from download client", prompt.remove_from_client),
            ("Blocklist release", prompt.blocklist),
        ];
        prompt::draw_toggles(
            frame,
            area,
            &title,
            &options,
            prompt.focus,
            "space: toggle   enter: remove   esc: cancel",
        );
    }

    /// The queue as a markable list. Two rows per item: a mark box plus the
    /// name (and Sonarr's SxxExx) with the right-aligned status, then a dim
    /// line of episode title / language / quality / time-left. `row` resolves
    /// the per-item backend-specific pieces. The caller is expected to have
    /// called [`clamp`](Self::clamp) for this queue length already.
    pub fn draw_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        queue: &[QueueItem],
        row: impl Fn(&QueueItem) -> DownloadRow,
    ) {
        let buf = frame.buffer_mut();
        if queue.is_empty() {
            Line::styled("  No active downloads.".to_string(), theme::dim()).render(area, buf);
            return;
        }
        let items_per_page = (area.height as usize / 3).max(1);
        let first = list::window_start(self.cursor, queue.len(), items_per_page);
        let last = (first + items_per_page).min(queue.len());
        let title_width = area.width.saturating_sub(STATUS_WIDTH + 6).max(4) as usize;
        let text_width = area.width.saturating_sub(6) as usize;

        let gutter = |accent: bool| {
            if accent {
                Span::styled(" │ ", Style::new().fg(theme::accent_bright()))
            } else {
                Span::raw("   ")
            }
        };

        let mut y = area.y;
        for (i, item) in queue.iter().enumerate().take(last).skip(first) {
            let selected = i == self.cursor;
            let marked = self.marks.contains(&item.id);
            let parts = row(item);
            let mark = if marked { "[x]" } else { "[ ]" };
            let name_line = match &parts.code {
                Some(code) => format!("{mark} {} — {code}", parts.name),
                None => format!("{mark} {}", parts.name),
            };
            let title = truncate(&name_line, title_width);

            let meta_parts: Vec<String> = [
                parts.episode_title,
                display::queue_language(item),
                display::queue_quality(item),
                Some(display::queue_timeleft(item)),
            ]
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
            .collect();
            let meta = truncate(&meta_parts.join(" • "), text_width);

            let title_style = if selected {
                Style::new()
                    .fg(theme::accent_bright())
                    .add_modifier(Modifier::BOLD)
            } else if marked {
                Style::new().fg(theme::accent_color())
            } else {
                Style::new().fg(theme::fg())
            };
            let meta_style = if selected {
                Style::new().fg(theme::accent_color())
            } else {
                theme::dim()
            };
            let status_style = match item.tracked_download_status.as_deref() {
                Some("error") => theme::error(),
                Some("warning") => Style::new().fg(theme::accent_color()),
                _ => Style::new().fg(theme::accent_bright()),
            };

            if y + 1 < area.y + area.height {
                let title_area = Rect::new(area.x, y, area.width.saturating_sub(1), 1);
                let [left, right] =
                    Layout::horizontal([Constraint::Fill(1), Constraint::Length(STATUS_WIDTH)])
                        .areas(title_area);
                Line::from(vec![gutter(selected), Span::styled(title, title_style)])
                    .render(left, buf);
                Line::from(vec![
                    Span::styled(display::queue_state_label(item), status_style),
                    Span::raw(" "),
                ])
                .right_aligned()
                .render(right, buf);
                Line::from(vec![gutter(selected), Span::styled(meta, meta_style)]).render(
                    Rect::new(area.x, y + 1, area.width.saturating_sub(1), 1),
                    buf,
                );
            }
            y += 3;
        }
        if queue.len() > items_per_page {
            list::draw_scrollbar(buf, area, self.cursor, queue.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    fn queue(ids: &[i64]) -> Vec<QueueItem> {
        ids.iter()
            .map(|id| QueueItem {
                id: *id,
                ..QueueItem::default()
            })
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn toggle_mark_flips_the_cursor_row() {
        let q = queue(&[10, 20, 30]);
        let mut d = Downloads {
            cursor: 1,
            ..Default::default()
        };
        d.toggle_mark(&q);
        assert_eq!(d.marked(), 1);
        d.toggle_mark(&q);
        assert_eq!(d.marked(), 0);
    }

    #[test]
    fn begin_delete_falls_back_to_cursor_row() {
        let q = queue(&[10, 20, 30]);
        let mut d = Downloads {
            cursor: 2,
            ..Default::default()
        };
        d.begin_delete(&q);
        assert!(d.is_prompting());
        // Committing yields exactly the highlighted id.
        match d.handle_prompt_key(key(KeyCode::Enter)) {
            DeleteAction::Commit { ids, .. } => assert_eq!(ids, vec![30]),
            _ => panic!("expected commit"),
        }
        assert!(!d.is_prompting());
    }

    #[test]
    fn begin_delete_targets_marked_in_queue_order() {
        let q = queue(&[10, 20, 30]);
        let mut d = Downloads::default();
        d.toggle_mark(&q); // cursor 0 -> marks 10
        d.cursor = 2;
        d.toggle_mark(&q); // marks 30
        d.begin_delete(&q);
        match d.handle_prompt_key(key(KeyCode::Enter)) {
            DeleteAction::Commit { ids, .. } => assert_eq!(ids, vec![10, 30]),
            _ => panic!("expected commit"),
        }
    }

    #[test]
    fn begin_delete_is_a_noop_on_empty_queue() {
        let mut d = Downloads::default();
        d.begin_delete(&[]);
        assert!(!d.is_prompting());
    }

    #[test]
    fn prune_drops_marks_that_left_the_queue() {
        let q = queue(&[10, 20, 30]);
        let mut d = Downloads::default();
        for cursor in 0..3 {
            d.cursor = cursor;
            d.toggle_mark(&q);
        }
        assert_eq!(d.marked(), 3);
        d.prune(&queue(&[20]));
        assert_eq!(d.marked(), 1);
    }

    #[test]
    fn prompt_space_toggles_focused_option_and_esc_cancels() {
        let q = queue(&[10]);
        let mut d = Downloads::default();
        d.begin_delete(&q);
        // Focus starts on "remove from client" (default true); space turns it
        // off, then down+space turns blocklist on.
        d.handle_prompt_key(key(KeyCode::Char(' ')));
        d.handle_prompt_key(key(KeyCode::Down));
        d.handle_prompt_key(key(KeyCode::Char(' ')));
        match d.handle_prompt_key(key(KeyCode::Enter)) {
            DeleteAction::Commit {
                remove_from_client,
                blocklist,
                ..
            } => {
                assert!(!remove_from_client);
                assert!(blocklist);
            }
            _ => panic!("expected commit"),
        }

        d.begin_delete(&q);
        assert!(matches!(
            d.handle_prompt_key(key(KeyCode::Esc)),
            DeleteAction::Cancelled
        ));
        assert!(!d.is_prompting());
    }
}
