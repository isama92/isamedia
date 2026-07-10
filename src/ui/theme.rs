//! Colour palette, ported from jfsh.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Rgb(0x92, 0x3F, 0xAD);
pub const ACCENT_BRIGHT: Color = Color::Rgb(0xB2, 0x66, 0xD4);
pub const FG: Color = Color::Rgb(0xdd, 0xdd, 0xdd);
pub const DIM: Color = Color::Rgb(0xA4, 0x9F, 0xA5);
pub const ERROR: Color = Color::Rgb(0xaa, 0x00, 0x00);
pub const TAB_BG: Color = Color::Rgb(0x00, 0x0B, 0x25);

pub fn accent() -> Style {
    Style::new().fg(ACCENT)
}

pub fn selected() -> Style {
    Style::new().fg(ACCENT_BRIGHT).add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::new().fg(DIM)
}

pub fn error() -> Style {
    Style::new().fg(ERROR)
}
