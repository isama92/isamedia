use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::ui::theme::{self, Theme};

/// Centered modal theme picker, drawn over whatever is below it. `cursor` is
/// the index into `Theme::ALL` of the highlighted row. Mirrors the recipe in
/// `prompt::draw_confirm`.
pub fn draw(frame: &mut Frame, area: Rect, cursor: usize) {
    let widest_title = Theme::ALL
        .iter()
        .map(|theme| theme.title().chars().count())
        .max()
        .unwrap_or(0) as u16;
    // Leave room for the "> " marker and side padding. max-then-min, not
    // clamp(24, area.width): clamp panics when the terminal is narrower than
    // the 24-column minimum.
    let width = (widest_title + 6).max(24).min(area.width);
    // Borders (2) + header + blank + one row per theme + blank + keys hint.
    let height = (Theme::ALL.len() as u16 + 6).min(area.height);

    let [box_area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [box_area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(box_area);

    let buf = frame.buffer_mut();
    Clear.render(box_area, buf);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::accent_bright()));
    let inner = block.inner(box_area);
    block.render(box_area, buf);

    // Header, blank, one row per theme, blank, keys hint.
    let mut constraints = vec![Constraint::Length(1), Constraint::Length(1)];
    for _ in Theme::ALL {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Length(1));
    let rows = Layout::vertical(constraints).split(inner);

    Line::styled(" Theme", theme::selected()).render(rows[0], buf);
    for (i, theme_option) in Theme::ALL.iter().enumerate() {
        let (marker, style) = if i == cursor {
            (">", theme::selected())
        } else {
            (" ", Style::new().fg(theme::fg()))
        };
        Line::styled(format!(" {marker} {}", theme_option.title()), style).render(rows[i + 2], buf);
    }
    Line::styled(" up/down move  enter apply  esc cancel", theme::dim())
        .render(rows[rows.len() - 1], buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn does_not_panic_on_narrow_terminals() {
        for (width, height) in [(10, 5), (1, 1), (0, 0), (24, 3), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for cursor in 0..Theme::ALL.len() {
                terminal
                    .draw(|frame| draw(frame, frame.area(), cursor))
                    .unwrap();
            }
        }
    }
}
