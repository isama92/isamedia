//! A centered modal single-select list with a type-to-filter input, for
//! choice sets too large to scroll through (the ~185-language picker).
//! Unlike the draw-only `ui::prompt` helpers it owns its state, like
//! `ui::form::Form` does: the caller forwards keys and acts on the returned
//! event. Enter can only ever return a listed key, so free typing can never
//! produce a value.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::ui::input::TextInput;
use crate::ui::{list, theme};

pub struct PickerItem {
    pub key: String,
    pub label: String,
}

impl PickerItem {
    pub fn new(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
        }
    }
}

pub enum PickerEvent {
    Consumed,
    /// The key of the chosen row.
    Chosen(String),
    Cancel,
}

pub struct Picker {
    /// Rows shown above the list, exempt from the filter (e.g. "No
    /// preference"), so the special choices stay reachable mid-filter.
    pinned: Vec<PickerItem>,
    items: Vec<PickerItem>,
    filter: TextInput,
    /// Indices into `items` matching the filter.
    visible: Vec<usize>,
    /// Over pinned + visible combined.
    cursor: usize,
}

impl Picker {
    pub fn new(pinned: Vec<PickerItem>, items: Vec<PickerItem>) -> Self {
        let visible = (0..items.len()).collect();
        let mut filter = TextInput::default();
        filter.focused = true;
        Self {
            pinned,
            items,
            filter,
            visible,
            cursor: 0,
        }
    }

    /// Open with the row for `key` selected, so the picker reflects the
    /// current value the way the theme/accent choice lists do.
    pub fn select(&mut self, key: &str) {
        let position = self.rows().position(|item| item.key == key);
        if let Some(position) = position {
            self.cursor = position;
        }
    }

    fn rows(&self) -> impl Iterator<Item = &PickerItem> {
        self.pinned
            .iter()
            .chain(self.visible.iter().map(|&index| &self.items[index]))
    }

    fn len(&self) -> usize {
        self.pinned.len() + self.visible.len()
    }

    fn refilter(&mut self) {
        let needle = self.filter.value().to_lowercase();
        self.visible = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.label.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect();
        self.cursor = 0;
    }

    pub fn on_key(&mut self, key: KeyEvent) -> PickerEvent {
        match key.code {
            KeyCode::Up if self.len() > 0 => {
                self.cursor = self.cursor.checked_sub(1).unwrap_or(self.len() - 1);
                PickerEvent::Consumed
            }
            KeyCode::Down if self.len() > 0 => {
                self.cursor = (self.cursor + 1) % self.len();
                PickerEvent::Consumed
            }
            KeyCode::Enter => match self.rows().nth(self.cursor) {
                Some(item) => PickerEvent::Chosen(item.key.clone()),
                // Filter matched nothing: keep the picker open.
                None => PickerEvent::Consumed,
            },
            KeyCode::Esc => {
                // Esc backs out one level: first the filter, then the picker.
                if self.filter.value().is_empty() {
                    return PickerEvent::Cancel;
                }
                self.filter.clear();
                self.refilter();
                PickerEvent::Consumed
            }
            _ => {
                // Only re-filter on actual value changes, so cursor movement
                // inside the input doesn't reset the list selection.
                let before = self.filter.value().to_string();
                if self.filter.on_key(key) && self.filter.value() != before {
                    self.refilter();
                }
                PickerEvent::Consumed
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, title: &str) {
        let footer = "type: filter   enter: select   esc: back";
        let content_width = self
            .rows()
            .map(|item| item.label.chars().count())
            .chain([title.chars().count() + 2, footer.chars().count()])
            .max()
            .unwrap_or(0) as u16;
        // max-then-min, not clamp: clamp panics on terminals narrower than
        // the minimum (same guard as the ui::prompt helpers).
        let width = (content_width + 6).max(30).min(area.width);
        let height = (self.len() as u16 + 6).min(area.height);
        let [box_area] = Layout::horizontal([Constraint::Length(width)])
            .flex(Flex::Center)
            .areas(area);
        let [box_area] = Layout::vertical([Constraint::Length(height)])
            .flex(Flex::Center)
            .areas(box_area);

        let buf = frame.buffer_mut();
        Clear.render(box_area, buf);
        let block = Block::bordered()
            .title(format!(" {title} "))
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::accent_bright()));
        let inner = block.inner(box_area);
        block.render(box_area, buf);
        if inner.width < 2 || inner.height == 0 {
            return;
        }

        let filter_row = Rect::new(inner.x + 1, inner.y, inner.width - 2, 1);
        self.filter.render(filter_row, buf);

        // Filter row, separator, list, footer (footer only when there is
        // room for it under at least one list row).
        let footer_rows = u16::from(inner.height >= 4);
        let per_page = inner.height.saturating_sub(2 + footer_rows) as usize;
        if per_page > 0 {
            let start = list::window_start(self.cursor, self.len(), per_page);
            for (offset, item) in self.rows().skip(start).take(per_page).enumerate() {
                let row = Rect::new(inner.x, inner.y + 2 + offset as u16, inner.width - 1, 1);
                let style = if start + offset == self.cursor {
                    Style::new()
                        .fg(theme::on_accent())
                        .bg(theme::accent_color())
                } else {
                    Style::new().fg(theme::fg())
                };
                Line::styled(format!(" {} ", item.label), style).render(row, buf);
            }
            if self.len() > per_page {
                let list_area = Rect::new(inner.x, inner.y + 2, inner.width, per_page as u16);
                list::draw_scrollbar(buf, list_area, self.cursor, self.len());
            }
        }
        if footer_rows > 0 {
            let row = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
            Line::styled(footer, theme::dim())
                .centered()
                .render(row, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn picker() -> Picker {
        Picker::new(
            vec![
                PickerItem::new("unset", "No preference"),
                PickerItem::new("default", "Default track"),
            ],
            vec![
                PickerItem::new("eng", "English (eng)"),
                PickerItem::new("ger", "German (ger)"),
                PickerItem::new("ita", "Italian (ita)"),
            ],
        )
    }

    fn type_str(picker: &mut Picker, text: &str) {
        for c in text.chars() {
            picker.on_key(key(KeyCode::Char(c)));
        }
    }

    fn chosen(event: PickerEvent) -> Option<String> {
        match event {
            PickerEvent::Chosen(key) => Some(key),
            _ => None,
        }
    }

    #[test]
    fn filter_narrows_case_insensitively_and_resets_cursor() {
        let mut picker = picker();
        picker.on_key(key(KeyCode::Down));
        picker.on_key(key(KeyCode::Down));
        picker.on_key(key(KeyCode::Down)); // on German
        type_str(&mut picker, "ITA");
        // Pinned rows survive the filter; the list narrowed to Italian.
        assert_eq!(picker.len(), 3);
        assert_eq!(picker.cursor, 0);
        picker.on_key(key(KeyCode::Down));
        picker.on_key(key(KeyCode::Down));
        assert_eq!(
            chosen(picker.on_key(key(KeyCode::Enter))).as_deref(),
            Some("ita")
        );
    }

    #[test]
    fn matches_code_inside_label() {
        let mut picker = picker();
        type_str(&mut picker, "ger");
        picker.on_key(key(KeyCode::Down)); // skip the two pinned rows
        picker.on_key(key(KeyCode::Down));
        assert_eq!(
            chosen(picker.on_key(key(KeyCode::Enter))).as_deref(),
            Some("ger")
        );
    }

    #[test]
    fn pinned_rows_always_choosable() {
        let mut picker = picker();
        type_str(&mut picker, "zzz");
        assert_eq!(picker.len(), 2); // only the pinned rows remain
        picker.on_key(key(KeyCode::Down));
        assert_eq!(
            chosen(picker.on_key(key(KeyCode::Enter))).as_deref(),
            Some("default")
        );
    }

    #[test]
    fn cursor_wraps() {
        let mut picker = picker();
        picker.on_key(key(KeyCode::Up));
        assert_eq!(
            chosen(picker.on_key(key(KeyCode::Enter))).as_deref(),
            Some("ita")
        );
    }

    #[test]
    fn select_seeds_cursor_to_current_value() {
        let mut picker = picker();
        picker.select("ger");
        assert_eq!(
            chosen(picker.on_key(key(KeyCode::Enter))).as_deref(),
            Some("ger")
        );
        // Unknown keys leave the cursor alone.
        picker.select("nope");
        assert_eq!(
            chosen(picker.on_key(key(KeyCode::Enter))).as_deref(),
            Some("ger")
        );
    }

    #[test]
    fn esc_clears_filter_before_cancelling() {
        let mut picker = picker();
        type_str(&mut picker, "ita");
        assert!(matches!(
            picker.on_key(key(KeyCode::Esc)),
            PickerEvent::Consumed
        ));
        assert_eq!(picker.len(), 5); // filter cleared, full list back
        assert!(matches!(
            picker.on_key(key(KeyCode::Esc)),
            PickerEvent::Cancel
        ));
    }

    #[test]
    fn enter_on_empty_match_is_a_no_op() {
        let mut picker = Picker::new(vec![], vec![PickerItem::new("eng", "English (eng)")]);
        type_str(&mut picker, "zzz");
        assert_eq!(picker.len(), 0);
        assert!(matches!(
            picker.on_key(key(KeyCode::Enter)),
            PickerEvent::Consumed
        ));
        assert!(matches!(
            picker.on_key(key(KeyCode::Up)),
            PickerEvent::Consumed
        ));
    }

    #[test]
    fn draw_does_not_panic_on_narrow_terminals() {
        let mut picker = picker();
        type_str(&mut picker, "a");
        for (width, height) in [(10, 5), (1, 1), (0, 0), (24, 3), (30, 4), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| picker.draw(frame, frame.area(), "Preferred audio language"))
                .unwrap();
        }
    }
}
