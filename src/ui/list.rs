//! Small shared helpers for cursor-driven scrolling lists: the centered
//! window offset and the right-edge scrollbar, used by every list view.

use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::ui::theme;

/// The centered window of a cursor list: which item to draw first.
pub fn window_start(cursor: usize, len: usize, per_page: usize) -> usize {
    let mut first = cursor.saturating_sub(per_page / 2);
    if first > len.saturating_sub(per_page) {
        first = len.saturating_sub(per_page);
    }
    first
}

/// Draw a one-column scrollbar down the right edge of `area`, its thumb at the
/// cursor's relative position. A no-op for lists too short to scroll or a
/// zero-width/zero-height area (which would otherwise underflow the edge and
/// thumb arithmetic below).
pub fn draw_scrollbar(buf: &mut ratatui::buffer::Buffer, area: Rect, cursor: usize, len: usize) {
    if len < 2 || area.height == 0 || area.width == 0 {
        return;
    }
    let height = area.height as usize;
    let x = area.x + area.width - 1;
    let thumb = ((cursor as f64 / (len - 1) as f64) * (height - 1) as f64).round() as usize;
    for i in 0..height {
        let (symbol, style) = if i == thumb {
            ("█", Style::new().fg(theme::accent_color()))
        } else {
            ("│", theme::dim())
        };
        buf[(x, area.y + i as u16)]
            .set_symbol(symbol)
            .set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    #[test]
    fn scrollbar_is_a_no_op_on_degenerate_areas() {
        // A populated list (len >= 2) drawn into a zero-width or zero-height
        // area must not underflow the edge/thumb arithmetic (panic in debug),
        // and must leave the buffer untouched.
        let backing = Rect::new(0, 0, 5, 5);
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 0, 5),
            Rect::new(0, 0, 5, 0),
        ] {
            let mut buf = Buffer::empty(backing);
            draw_scrollbar(&mut buf, area, 3, 10);
            assert_eq!(buf, Buffer::empty(backing));
        }
    }

    #[test]
    fn scrollbar_draws_the_right_edge_column() {
        let area = Rect::new(0, 0, 4, 5);
        let mut buf = Buffer::empty(area);
        draw_scrollbar(&mut buf, area, 0, 10);
        // Thumb at the top for cursor 0; the rightmost column is populated.
        assert_eq!(buf[(3, 0)].symbol(), "█");
        assert_eq!(buf[(3, 4)].symbol(), "│");
    }
}
