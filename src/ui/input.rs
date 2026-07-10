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
        let shown: String = if self.masked {
            "•".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        };
        let mut spans: Vec<Span> = Vec::with_capacity(3);
        if self.focused {
            let chars: Vec<char> = shown.chars().collect();
            let before: String = chars[..self.cursor].iter().collect();
            let at = chars.get(self.cursor).copied().unwrap_or(' ');
            let after: String = chars.get(self.cursor + 1..).unwrap_or(&[]).iter().collect();
            spans.push(Span::styled(before, Style::new().fg(theme::FG)));
            spans.push(Span::styled(
                at.to_string(),
                Style::new().add_modifier(Modifier::REVERSED),
            ));
            spans.push(Span::styled(after, Style::new().fg(theme::FG)));
        } else {
            spans.push(Span::styled(shown, Style::new().fg(theme::FG)));
        }
        Line::from(spans).render(area, buf);
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
}
