//! A small full-body form widget: a list of labelled fields the user changes in
//! place — cycle-selects (Left/Right) and free-text inputs (type) — plus
//! Save/Cancel action rows. It owns its focus, navigation and (per-field)
//! visibility and draws itself into a body `Rect`, replacing the level content
//! while open — the single-view alternative to a chain of centered
//! `prompt::draw_menu` popups.
//!
//! It is domain-agnostic: the caller builds [`Field`]s from its own lists,
//! reads results back by [`FieldId`], and does the commit. State lives here
//! (like [`crate::ui::input::TextInput`]), so the two apps share one tested
//! focus/clip/visibility implementation instead of duplicating it.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::ui::input::TextInput;
use crate::ui::{list, text, theme};

/// A caller-assigned, unique field identifier. Results are read and visibility
/// toggled by id, never by row order, so a hidden/conditional field never
/// shifts what the caller reads.
pub type FieldId = u16;

/// The control a field exposes.
enum Control {
    /// Cycle through `choices` with Left/Right. `initial` is the opened-on index
    /// so [`Field::changed`] gives the caller a free diff.
    Select {
        choices: Vec<String>,
        selected: usize,
        initial: usize,
    },
    /// A free-text input the user types into. `initial` is the opened-on value.
    Text { input: TextInput, initial: String },
}

/// One labelled field.
pub struct Field {
    id: FieldId,
    label: String,
    control: Control,
    visible: bool,
}

impl Field {
    /// A select over `choices`, opened on `selected` (clamped, so an empty or
    /// short list can never panic or point out of range).
    pub fn select(
        id: FieldId,
        label: impl Into<String>,
        choices: Vec<String>,
        selected: usize,
    ) -> Self {
        let selected = if choices.is_empty() {
            0
        } else {
            selected.min(choices.len() - 1)
        };
        Self {
            id,
            label: label.into(),
            control: Control::Select {
                choices,
                selected,
                initial: selected,
            },
            visible: true,
        }
    }

    /// A Yes/No toggle (`on` selects "Yes").
    pub fn toggle(id: FieldId, label: impl Into<String>, on: bool) -> Self {
        Self::select(
            id,
            label,
            vec!["Yes".into(), "No".into()],
            if on { 0 } else { 1 },
        )
    }

    /// A free-text field pre-filled with `value`.
    pub fn text(id: FieldId, label: impl Into<String>, value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            id,
            label: label.into(),
            control: Control::Text {
                input: TextInput::with_value(value.clone()),
                initial: value,
            },
            visible: true,
        }
    }

    /// Start hidden; the caller reveals it with [`Form::set_visible`].
    pub fn hidden(mut self) -> Self {
        self.visible = false;
        self
    }

    /// Mask this text field's value (rendered as bullets), so a secret never
    /// shows in the clear. A no-op on a select field. Readback via
    /// [`Form::text`] still returns the real, unmasked value.
    pub fn masked(mut self) -> Self {
        if let Control::Text { input, .. } = &mut self.control {
            input.masked = true;
        }
        self
    }

    /// Whether the current value differs from the one it opened on.
    pub fn changed(&self) -> bool {
        match &self.control {
            Control::Select {
                selected, initial, ..
            } => selected != initial,
            Control::Text { input, initial } => input.value() != initial,
        }
    }

    fn selected(&self) -> usize {
        match &self.control {
            Control::Select { selected, .. } => *selected,
            Control::Text { .. } => 0,
        }
    }

    fn text_value(&self) -> String {
        match &self.control {
            Control::Text { input, .. } => input.value().to_string(),
            Control::Select {
                choices, selected, ..
            } => choices.get(*selected).cloned().unwrap_or_default(),
        }
    }
}

/// What a key did to the form. `Changed` lets the caller react (e.g. reveal a
/// conditional field); everything else is either a nav/no-op (`Consumed`) or a
/// terminal action.
pub enum FormEvent {
    Consumed,
    Changed(FieldId),
    Save,
    Cancel,
}

/// A modal form. Focus runs over the visible fields in order, then the Save and
/// (optionally) Cancel action rows.
pub struct Form {
    fields: Vec<Field>,
    focus: usize,
    save_label: &'static str,
    /// Whether a Cancel action row is shown. `false` drops it for entry screens
    /// that have nowhere to cancel back to (Esc still yields `Cancel`).
    show_cancel: bool,
}

impl Form {
    /// `save_label` is the confirm button's text ("Save" or "Add").
    pub fn new(fields: Vec<Field>, save_label: &'static str) -> Self {
        let mut form = Self {
            fields,
            focus: 0,
            save_label,
            show_cancel: true,
        };
        form.sync_focus();
        form
    }

    /// Render only the confirm button (no Cancel row) — for entry screens with
    /// nowhere to cancel back to. Esc still yields [`FormEvent::Cancel`]; the
    /// caller decides what, if anything, that means.
    pub fn without_cancel(mut self) -> Self {
        self.show_cancel = false;
        self
    }

    /// Indices into `fields` of the currently visible fields, in order.
    fn visible_indices(&self) -> Vec<usize> {
        self.fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.visible)
            .map(|(i, _)| i)
            .collect()
    }

    /// Number of focusable rows: visible fields plus Save and, unless
    /// suppressed, Cancel.
    fn nav_len(&self) -> usize {
        self.visible_indices().len() + self.action_rows()
    }

    /// Number of action rows below the fields: Save always, Cancel optionally.
    fn action_rows(&self) -> usize {
        if self.show_cancel { 2 } else { 1 }
    }

    /// The current choice index of the select field with `id` (0 if absent or
    /// not a select).
    pub fn selected(&self, id: FieldId) -> usize {
        self.fields
            .iter()
            .find(|field| field.id == id)
            .map_or(0, Field::selected)
    }

    /// The current value of the text field with `id` (empty if absent).
    pub fn text(&self, id: FieldId) -> String {
        self.fields
            .iter()
            .find(|field| field.id == id)
            .map(Field::text_value)
            .unwrap_or_default()
    }

    /// Whether the field with `id` differs from its opened-on value.
    pub fn changed(&self, id: FieldId) -> bool {
        self.fields
            .iter()
            .find(|field| field.id == id)
            .is_some_and(Field::changed)
    }

    /// Show or hide a field; focus is re-clamped so a hidden row never keeps it.
    pub fn set_visible(&mut self, id: FieldId, on: bool) {
        if let Some(field) = self.fields.iter_mut().find(|field| field.id == id) {
            field.visible = on;
        }
        let last = self.nav_len().saturating_sub(1);
        self.focus = self.focus.min(last);
        self.sync_focus();
    }

    /// Snap a field back to the value it opened on (used when re-hiding a
    /// conditional field so it re-shows on its default).
    pub fn reset(&mut self, id: FieldId) {
        if let Some(field) = self.fields.iter_mut().find(|field| field.id == id) {
            match &mut field.control {
                Control::Select {
                    selected, initial, ..
                } => *selected = *initial,
                Control::Text { input, initial } => *input = TextInput::with_value(initial.clone()),
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> FormEvent {
        let visible = self.visible_indices();
        let nav = visible.len() + self.action_rows();
        let save_row = visible.len();
        // Cancel exists only when shown; its absence never shifts the Save row.
        let cancel_row = self.show_cancel.then_some(visible.len() + 1);
        // Visibility may have changed since the last key; keep focus in range.
        if self.focus >= nav {
            self.focus = nav - 1;
        }
        let event = match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                self.focus = (self.focus + nav - 1) % nav;
                FormEvent::Consumed
            }
            KeyCode::Down | KeyCode::Tab => {
                self.focus = (self.focus + 1) % nav;
                FormEvent::Consumed
            }
            KeyCode::Enter => {
                if self.focus == save_row {
                    FormEvent::Save
                } else if Some(self.focus) == cancel_row {
                    FormEvent::Cancel
                } else {
                    // Enter on a field advances, so the form can be walked to
                    // Save with one key.
                    self.focus = (self.focus + 1) % nav;
                    FormEvent::Consumed
                }
            }
            KeyCode::Esc => FormEvent::Cancel,
            // Everything else is routed to the focused field's control (cycle a
            // select, or type into a text input).
            _ => self.edit_focused(&visible, key),
        };
        self.sync_focus();
        event
    }

    /// Route a key to the focused field: Left/Right cycles a select (wraps);
    /// any editing key mutates a text input. A no-op on the action rows.
    fn edit_focused(&mut self, visible: &[usize], key: KeyEvent) -> FormEvent {
        let Some(&idx) = visible.get(self.focus) else {
            return FormEvent::Consumed;
        };
        let id = self.fields[idx].id;
        match &mut self.fields[idx].control {
            Control::Select {
                choices, selected, ..
            } => {
                let len = choices.len();
                if len == 0 {
                    return FormEvent::Consumed;
                }
                match key.code {
                    KeyCode::Left => {
                        *selected = (*selected + len - 1) % len;
                        FormEvent::Changed(id)
                    }
                    KeyCode::Right => {
                        *selected = (*selected + 1) % len;
                        FormEvent::Changed(id)
                    }
                    _ => FormEvent::Consumed,
                }
            }
            Control::Text { input, .. } => {
                let before = input.value().to_string();
                input.on_key(key);
                if input.value() == before {
                    FormEvent::Consumed
                } else {
                    FormEvent::Changed(id)
                }
            }
        }
    }

    /// Keep each text input's `focused` flag matching the form focus, so only
    /// the focused text field shows a cursor.
    fn sync_focus(&mut self) {
        let visible = self.visible_indices();
        for (pos, &idx) in visible.iter().enumerate() {
            if let Control::Text { input, .. } = &mut self.fields[idx].control {
                input.focused = pos == self.focus;
            }
        }
    }

    /// Draw the form full-body: an accent `title` row then the field rows and
    /// the Save/Cancel rows. Rows scroll only when they can't fit. The key hint
    /// is left to the caller's status/help bar, so it isn't drawn here.
    pub fn draw(&self, frame: &mut Frame, area: Rect, title: &str) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let buf = frame.buffer_mut();

        Line::styled(format!("  {title}"), theme::selected())
            .render(Rect::new(area.x, area.y, area.width, 1), buf);

        // Rows sit below the title + a blank line and fill the rest.
        let rows_top = area.y.saturating_add(2);
        let bottom = area.y + area.height;
        let rows_height = bottom.saturating_sub(rows_top);

        let visible = self.visible_indices();
        let nav_len = visible.len() + self.action_rows();
        // Width over EVERY field, visible or not, so revealing a conditional
        // field (e.g. Move files) never shifts the value column.
        let label_w = self
            .fields
            .iter()
            .map(|field| field.label.chars().count())
            .max()
            .unwrap_or(0);

        let per_page = rows_height as usize;
        if per_page == 0 {
            return;
        }
        let needs_scroll = nav_len > per_page;
        // Leave the right-hand column for the scrollbar when one is drawn, so it
        // never overwrites row text.
        let content_width = area.width.saturating_sub(needs_scroll as u16);
        let first = if needs_scroll {
            list::window_start(self.focus, nav_len, per_page)
        } else {
            0
        };
        for row in first..(first + per_page).min(nav_len) {
            let rect = Rect::new(area.x, rows_top + (row - first) as u16, content_width, 1);
            if let Some(&idx) = visible.get(row) {
                self.draw_field(&self.fields[idx], label_w, row == self.focus, rect, buf);
            } else if row == visible.len() {
                button_line(self.save_label, self.focus == visible.len()).render(rect, buf);
            } else if self.show_cancel {
                button_line("Cancel", self.focus == visible.len() + 1).render(rect, buf);
            }
        }
        if needs_scroll {
            let rows_area = Rect::new(area.x, rows_top, area.width, rows_height);
            list::draw_scrollbar(buf, rows_area, self.focus, nav_len);
        }
    }

    /// Draw one field row as two aligned columns: the label (indent + focus
    /// marker + label, padded to `label_w`) then, after a fixed gap, the value.
    /// A text field renders its input; a select renders its value (accent-bold
    /// when focused, dim otherwise). `label_w` spans every field, visible or
    /// not (see `draw`), so revealing a longer label never shifts the value
    /// column.
    fn draw_field(
        &self,
        field: &Field,
        label_w: usize,
        focused: bool,
        rect: Rect,
        buf: &mut Buffer,
    ) {
        // Columns of blank space between the label and value columns.
        const LABEL_GAP: u16 = 3;
        let marker = if focused { "> " } else { "  " };
        let label_style = if focused {
            theme::selected()
        } else {
            Style::new().fg(theme::fg())
        };
        // "  " indent + marker(2) + label(label_w) + gap; the value fills the rest.
        let label_col = 2 + 2 + label_w as u16 + LABEL_GAP;
        let [label_area, value_area] =
            Layout::horizontal([Constraint::Length(label_col), Constraint::Fill(1)]).areas(rect);
        Line::from(Span::styled(
            format!("  {marker}{label:<label_w$}", label = field.label),
            label_style,
        ))
        .render(label_area, buf);
        match &field.control {
            Control::Text { input, .. } => input.render(value_area, buf),
            Control::Select {
                choices, selected, ..
            } => {
                let value = choices.get(*selected).map(String::as_str).unwrap_or("");
                let value = text::truncate(value, (value_area.width as usize).max(1));
                let style = if focused {
                    Style::new()
                        .fg(theme::accent_bright())
                        .add_modifier(Modifier::BOLD)
                } else {
                    theme::dim()
                };
                Line::from(Span::styled(value, style)).render(value_area, buf);
            }
        }
    }
}

/// An action row (`[ Save ]` / `[ Cancel ]`), highlighted with the same
/// background fill the modal menus use when focused.
fn button_line(label: &str, focused: bool) -> Line<'static> {
    let style = if focused {
        Style::new()
            .fg(theme::on_accent())
            .bg(theme::accent_color())
    } else {
        Style::new().fg(theme::fg())
    };
    Line::from(Span::styled(format!("  [ {label} ]"), style))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn sample() -> Form {
        Form::new(
            vec![
                Field::select(0, "Root", vec!["a".into(), "b".into(), "c".into()], 0),
                Field::toggle(1, "Monitor", true),
            ],
            "Save",
        )
    }

    #[test]
    fn left_right_cycles_focused_field_and_wraps() {
        let mut form = sample();
        // focus starts on field 0 (3 choices).
        assert!(matches!(
            form.on_key(key(KeyCode::Right)),
            FormEvent::Changed(0)
        ));
        assert_eq!(form.selected(0), 1);
        form.on_key(key(KeyCode::Right));
        form.on_key(key(KeyCode::Right));
        assert_eq!(form.selected(0), 0); // wrapped 0->1->2->0
        form.on_key(key(KeyCode::Left));
        assert_eq!(form.selected(0), 2); // wrapped backwards
    }

    #[test]
    fn enter_on_save_and_cancel() {
        let mut form = sample();
        // 2 fields, then Save (idx 2), then Cancel (idx 3).
        form.on_key(key(KeyCode::Down)); // -> field 1
        form.on_key(key(KeyCode::Down)); // -> Save
        assert!(matches!(form.on_key(key(KeyCode::Enter)), FormEvent::Save));
        form.on_key(key(KeyCode::Down)); // -> Cancel
        assert!(matches!(
            form.on_key(key(KeyCode::Enter)),
            FormEvent::Cancel
        ));
    }

    #[test]
    fn esc_always_cancels() {
        let mut form = sample();
        assert!(matches!(form.on_key(key(KeyCode::Esc)), FormEvent::Cancel));
    }

    #[test]
    fn navigation_skips_hidden_fields() {
        let mut form = Form::new(
            vec![
                Field::select(0, "A", vec!["x".into()], 0),
                Field::toggle(1, "B", true).hidden(),
                Field::select(2, "C", vec!["y".into()], 0),
            ],
            "Save",
        );
        // Visible fields: A(0), C(2), then Save, Cancel -> nav len 4.
        // Down four times must return to the start, never landing on B.
        for _ in 0..4 {
            form.on_key(key(KeyCode::Down));
        }
        assert_eq!(form.focus, 0);
    }

    #[test]
    fn revealing_a_field_keeps_focus_valid() {
        let mut form = sample();
        // Move focus to Cancel (last row of a 4-row nav).
        for _ in 0..3 {
            form.on_key(key(KeyCode::Down));
        }
        assert_eq!(form.focus, 3);
        // Hiding a field shrinks nav to 3; focus must clamp into range.
        form.set_visible(1, false);
        assert!(form.focus < 3);
    }

    #[test]
    fn changed_and_reset() {
        let mut form = sample();
        assert!(!form.changed(0));
        form.on_key(key(KeyCode::Right));
        assert!(form.changed(0));
        form.reset(0);
        assert!(!form.changed(0));
    }

    #[test]
    fn select_clamps_out_of_range_initial() {
        let field = Field::select(0, "x", vec!["only".into()], 9);
        assert_eq!(field.selected(), 0);
        let empty = Field::select(1, "y", Vec::new(), 3);
        assert_eq!(empty.selected(), 0);
        assert_eq!(empty.text_value(), "");
    }

    #[test]
    fn text_field_edits_reports_changed_and_reads_back() {
        let mut form = Form::new(vec![Field::text(0, "Root", "/movies")], "Save");
        assert!(!form.changed(0));
        assert_eq!(form.text(0), "/movies");
        // Cursor opens at the end; typing appends.
        assert!(matches!(
            form.on_key(key(KeyCode::Char('4'))),
            FormEvent::Changed(0)
        ));
        assert_eq!(form.text(0), "/movies4");
        assert!(form.changed(0));
        // Left/Right move the text cursor, not a cycle.
        assert!(matches!(
            form.on_key(key(KeyCode::Left)),
            FormEvent::Consumed
        ));
        form.reset(0);
        assert_eq!(form.text(0), "/movies");
        assert!(!form.changed(0));
    }

    /// The whole rendered screen as text, for content assertions.
    fn rendered(form: &Form, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| form.draw(frame, frame.area(), "Title"))
            .unwrap();
        let buf = terminal.backend().buffer();
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        out
    }

    #[test]
    fn masked_text_field_renders_bullets() {
        let form = Form::new(vec![Field::text(0, "Secret", "abcd").masked()], "Save");
        // Readback returns the real value; only the rendering is masked.
        assert_eq!(form.text(0), "abcd");
        let text = rendered(&form, 40, 8);
        assert!(text.contains('\u{2022}'), "expected bullets in:\n{text}");
        assert!(!text.contains("abcd"), "secret leaked in:\n{text}");
    }

    #[test]
    fn without_cancel_has_no_cancel_row() {
        let mut form =
            Form::new(vec![Field::text(0, "Host", "example")], "Connect").without_cancel();
        // Nav is field(0) then Save only: Down twice wraps back to the field,
        // never landing on a Cancel row.
        form.on_key(key(KeyCode::Down)); // -> Save
        assert!(matches!(form.on_key(key(KeyCode::Enter)), FormEvent::Save));
        form.on_key(key(KeyCode::Down)); // Save -> field 0 (wrap)
        assert_eq!(form.focus, 0);
        // Esc still cancels even without a Cancel button.
        assert!(matches!(form.on_key(key(KeyCode::Esc)), FormEvent::Cancel));
        // The rendered form shows Save but no Cancel button.
        let text = rendered(&form, 40, 8);
        assert!(text.contains("Connect"), "missing Save button in:\n{text}");
        assert!(!text.contains("Cancel"), "unexpected Cancel in:\n{text}");
    }

    #[test]
    fn draw_does_not_panic_on_narrow_terminals() {
        for (w, h) in [(10, 5), (1, 1), (0, 0), (24, 3), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let form = Form::new(
                vec![
                    Field::text(0, "Root folder", "/movies/library"),
                    Field::select(1, "Quality", vec!["HD".into(), "SD".into()], 0),
                    Field::toggle(2, "Monitor", true),
                ],
                "Save",
            );
            terminal
                .draw(|frame| form.draw(frame, frame.area(), "Edit options — Test"))
                .unwrap();
        }
    }
}
