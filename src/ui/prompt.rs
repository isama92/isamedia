use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::ui::theme;

/// Centered modal yes/no prompt, drawn over whatever is below it.
///
/// Key contract for callers: accept only `y`/`Y` to confirm and `n`/`N` to
/// decline, and ignore everything else — in particular Enter and Esc. These
/// prompts guard actions that are hard or impossible to undo, and Enter is
/// usually the key that opened them, so honouring it would let a double-tap
/// confirm by accident.
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

/// Centered modal option list; the caller owns the selection state and
/// routes arrows/enter/esc itself, this only draws.
pub fn draw_menu(frame: &mut Frame, area: Rect, title: &str, options: &[&str], selected: usize) {
    let footer = "enter: select   esc: close";
    let content_width = options
        .iter()
        .map(|option| option.chars().count())
        .chain([title.chars().count(), footer.chars().count()])
        .max()
        .unwrap_or(0) as u16;
    // max-then-min, not clamp: clamp panics on terminals narrower than the
    // minimum (same guard as draw_confirm).
    let width = (content_width + 6).max(24).min(area.width);
    let height = (options.len() as u16 + 4).min(area.height);
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

    for (i, option) in options.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        let style = if i == selected {
            Style::new()
                .fg(theme::on_accent())
                .bg(theme::accent_color())
        } else {
            Style::new().fg(theme::fg())
        };
        Line::styled(format!(" {option} "), style).render(row, buf);
    }
    if options.len() as u16 + 1 < inner.height {
        let row = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        Line::styled(footer, theme::dim())
            .centered()
            .render(row, buf);
    }
}

/// Centered modal list of checkbox toggles: each option shows `[x]`/`[ ]`
/// from its bool. The caller owns the focus index, the bools, and key routing
/// (space toggles, enter commits, esc cancels); this only draws.
pub fn draw_toggles(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    options: &[(&str, bool)],
    focus: usize,
    footer: &str,
) {
    let rows: Vec<String> = options
        .iter()
        .map(|(label, on)| format!("[{}] {label}", if *on { "x" } else { " " }))
        .collect();
    let content_width = rows
        .iter()
        .map(|row| row.chars().count())
        .chain([title.chars().count(), footer.chars().count()])
        .max()
        .unwrap_or(0) as u16;
    // max-then-min, not clamp: clamp panics on terminals narrower than the
    // minimum (same guard as draw_confirm).
    let width = (content_width + 6).max(24).min(area.width);
    let height = (options.len() as u16 + 4).min(area.height);
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

    for (i, row) in rows.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let area = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        let style = if i == focus {
            Style::new()
                .fg(theme::on_accent())
                .bg(theme::accent_color())
        } else {
            Style::new().fg(theme::fg())
        };
        Line::styled(format!(" {row} "), style).render(area, buf);
    }
    if options.len() as u16 + 1 < inner.height {
        let row = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        Line::styled(footer, theme::dim())
            .centered()
            .render(row, buf);
    }
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

    #[test]
    fn menu_does_not_panic_on_narrow_terminals() {
        let options = ["Name ascending", "Name descending", "Date added"];
        for (width, height) in [(10, 5), (1, 1), (0, 0), (24, 3), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw_menu(frame, frame.area(), "Sort by", &options, 1))
                .unwrap();
        }
    }

    #[test]
    fn toggles_do_not_panic_on_narrow_terminals() {
        let options = [
            ("Remove from download client", true),
            ("Blocklist release", false),
        ];
        for (width, height) in [(10, 5), (1, 1), (0, 0), (24, 3), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    draw_toggles(
                        frame,
                        frame.area(),
                        "Remove 2 downloads",
                        &options,
                        0,
                        "space: toggle   enter: remove   esc: cancel",
                    )
                })
                .unwrap();
        }
    }
}
