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
/// cursor's relative position. A no-op for lists too short to scroll.
pub fn draw_scrollbar(buf: &mut ratatui::buffer::Buffer, area: Rect, cursor: usize, len: usize) {
    if len < 2 {
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
