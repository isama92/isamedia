use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::ui::theme;

/// Centered modal yes/no prompt, drawn over whatever is below it.
pub fn draw_confirm(frame: &mut Frame, area: Rect, question: &str) {
    let width = (question.chars().count() as u16 + 6).clamp(24, area.width);
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
        .border_style(Style::new().fg(theme::ACCENT_BRIGHT));
    let inner = block.inner(box_area);
    block.render(box_area, buf);

    let [question_row, _, keys_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    Line::styled(question.to_string(), Style::new().fg(theme::FG))
        .centered()
        .render(question_row, buf);
    Line::styled("y: yes   n: no", theme::dim())
        .centered()
        .render(keys_row, buf);
}
