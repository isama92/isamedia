use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::ui::theme;

/// Minimal single-line text input. The cursor is drawn as a reversed cell so
/// we never have to manage the real terminal cursor.
#[derive(Debug, Default, Clone)]
pub struct TextInput {
    value: String,
    /// Cursor position in characters (not bytes).
    cursor: usize,
    pub masked: bool,
    pub focused: bool,
}

impl TextInput {
    pub fn with_value(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            cursor: value.chars().count(),
            value,
            ..Self::default()
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_index)
            .map(|(idx, _)| idx)
            .unwrap_or(self.value.len())
    }

    /// Returns true when the key was consumed by the input.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('u') => {
                    self.clear();
                    return true;
                }
                _ => return false,
            }
        }
        match key.code {
            KeyCode::Char(c) => {
                let at = self.byte_index(self.cursor);
                self.value.insert(at, c);
                self.cursor += 1;
                true
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let at = self.byte_index(self.cursor);
                    self.value.remove(at);
                }
                true
            }
            KeyCode::Delete => {
                if self.cursor < self.value.chars().count() {
                    let at = self.byte_index(self.cursor);
                    self.value.remove(at);
                }
                true
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.value.chars().count());
                true
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.value.chars().count();
                true
            }
            _ => false,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        use unicode_width::UnicodeWidthChar;

        let shown: String = if self.masked {
            "•".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        };
        if !self.focused {
            Line::styled(shown, Style::new().fg(theme::FG)).render(area, buf);
            return;
        }

        let chars: Vec<char> = shown.chars().collect();
        let cursor = self.cursor.min(chars.len());
        let at = chars.get(cursor).copied().unwrap_or(' ');

        // Horizontal scroll: when the value outgrows the field, show the
        // window ending at the cursor instead of clipping it off the right
        // edge. Walk back from the cursor cell, spending the area's column
        // budget (display width, so double-width chars count as two).
        let mut used = at.width().unwrap_or(1).max(1);
        let mut start = cursor;
        while start > 0 {
            let char_width = chars[start - 1].width().unwrap_or(0);
            if used + char_width > area.width as usize {
                break;
            }
            used += char_width;
            start -= 1;
        }

        let before: String = chars[start..cursor].iter().collect();
        let after: String = chars.get(cursor + 1..).unwrap_or(&[]).iter().collect();
        Line::from(vec![
            Span::styled(before, Style::new().fg(theme::FG)),
            Span::styled(
                at.to_string(),
                Style::new().add_modifier(Modifier::REVERSED),
            ),
            Span::styled(after, Style::new().fg(theme::FG)),
        ])
        .render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn typing_and_editing() {
        let mut input = TextInput::default();
        for c in "héllo".chars() {
            input.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(input.value(), "héllo");
        input.on_key(key(KeyCode::Backspace));
        assert_eq!(input.value(), "héll");
        input.on_key(key(KeyCode::Home));
        input.on_key(key(KeyCode::Delete));
        assert_eq!(input.value(), "éll");
        input.on_key(key(KeyCode::Right));
        input.on_key(key(KeyCode::Char('x')));
        assert_eq!(input.value(), "éxll");
    }

    fn rendered_row(input: &TextInput, width: u16) -> String {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        input.render(area, &mut buf);
        (0..width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn scrolls_to_keep_cursor_visible() {
        let mut input = TextInput::with_value("https://jellyfin.example.com/media");
        input.focused = true;
        // Cursor at the end: the window shows the value's tail, not its head
        // (with the cursor's phantom space taking the last column).
        assert_eq!(rendered_row(&input, 10), "com/media");
        // Cursor at the start: the window shows the head.
        input.on_key(key(KeyCode::Home));
        assert_eq!(rendered_row(&input, 10), "https://je");
        // Short values are unaffected.
        let mut short = TextInput::with_value("demo");
        short.focused = true;
        assert_eq!(rendered_row(&short, 10), "demo");
    }
}
