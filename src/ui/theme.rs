//! Colour palette, ported from jfsh.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Rgb(0x92, 0x3F, 0xAD);
pub const ACCENT_BRIGHT: Color = Color::Rgb(0xB2, 0x66, 0xD4);
/// Body text on the terminal background. `Reset` defers to the terminal's own
/// default foreground, so it stays legible on both light and dark terminals.
/// jfsh got this via lipgloss' `AdaptiveColor{Light: "#1a1a1a", Dark: "#ddd"}`;
/// ratatui has no adaptive colour, and a fixed light grey looked washed out on
/// light terminals, so we defer to the terminal instead.
pub const FG: Color = Color::Reset;
/// Foreground for chrome that always sits on one of our own dark fills (tab
/// bar, login button). Fixed light grey, because the background it pairs with
/// is fixed too — this must not follow the terminal or it vanishes on light
/// terminals.
pub const CHROME_FG: Color = Color::Rgb(0xdd, 0xdd, 0xdd);
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
