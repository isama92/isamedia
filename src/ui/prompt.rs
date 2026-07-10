use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::ui::theme;

/// Centered modal yes/no prompt, drawn over whatever is below it.
pub fn draw_confirm(frame: &mut Frame, area: Rect, question: &str) {
    // max-then-min, not clamp(24, area.width): clamp panics when the
    // terminal is narrower than the 24-column minimum.
    let width = (question.chars().count() as u16 + 6)
        .max(24)
        .min(area.width);
    let [box_area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [box_area] = Layout::vertical([Constraint::Length(5)])
        .flex(Flex::Center)
        .areas(box_area);

    let buf = frame.buffer_mut();
    Clear.render(box_area, buf);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::accent_bright()));
    let inner = block.inner(box_area);
    block.render(box_area, buf);

    let [question_row, _, keys_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    Line::styled(question.to_string(), Style::new().fg(theme::fg()))
        .centered()
        .render(question_row, buf);
    Line::styled("y: yes   n: no", theme::dim())
        .centered()
        .render(keys_row, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn does_not_panic_on_narrow_terminals() {
        for (width, height) in [(10, 5), (1, 1), (0, 0), (24, 3), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw_confirm(frame, frame.area(), "Replace current playback?"))
                .unwrap();
        }
    }
}
