//! Colour palette and runtime theme selection.
//!
//! Two light themes ship: Catppuccin Latte (default) and Solarized Light.
//! Colours are read through the accessor functions below, which consult the
//! process-wide `CURRENT` index each call. That lets the theme change at
//! runtime without threading a palette through `MediaApp::draw`. We only ever
//! set foreground colours (and three small `accent`/`tab_bg` fills for chrome);
//! the terminal's own background always shows through, so these light themes
//! are best paired with a light terminal.

use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

/// A full set of foreground colours. There is no background field — we never
/// paint the terminal background. The three background fills we do use (active
/// tab pill, login button, series bar) pair `accent`/`tab_bg` with the two
/// contrasting `on_*` text colours.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Brand hue: secondary accent text and the active pill/button fill.
    pub accent: Color,
    /// The same hue with more emphasis, for the single focused element
    /// (selected title, cursor bar, modal border). On a light background it is
    /// darker than `accent` so a bold selected title out-contrasts the
    /// secondary description beside it.
    pub accent_bright: Color,
    /// Body text.
    pub fg: Color,
    /// De-emphasised text (help, watched rows, rules, scrollbar track).
    pub dim: Color,
    /// Error text.
    pub error: Color,
    /// Fill behind an inactive tab pill.
    pub tab_bg: Color,
    /// Text drawn on an `accent` fill; must contrast with `accent`.
    pub on_accent: Color,
    /// Text drawn on a `tab_bg` fill; must contrast with `tab_bg`.
    pub on_tab: Color,
}

/// Catppuccin Latte (https://catppuccin.com/palette). Rosewater is a pale warm
/// accent, so `on_accent` is dark Text (not near-white) for legible labels on
/// the accent fill, and `accent_bright` is Rosewater darkened so emphasis
/// (selected titles, cursor bar, modal border) stays readable on the light
/// background.
const LATTE: Palette = Palette {
    accent: Color::Rgb(0xdc, 0x8a, 0x78),        // Rosewater
    accent_bright: Color::Rgb(0xb4, 0x5c, 0x42), // Rosewater, darkened
    fg: Color::Rgb(0x4c, 0x4f, 0x69),            // Text
    dim: Color::Rgb(0x6c, 0x6f, 0x85),           // Subtext0
    error: Color::Rgb(0xd2, 0x0f, 0x39),         // Red
    tab_bg: Color::Rgb(0xcc, 0xd0, 0xda),        // Surface0
    on_accent: Color::Rgb(0x4c, 0x4f, 0x69),     // Text
    on_tab: Color::Rgb(0x4c, 0x4f, 0x69),        // Text
};

/// Solarized Light (https://ethanschoonover.com/solarized). `accent_bright` is
/// violet darkened for emphasis; the source ships a single violet.
const SOLARIZED_LIGHT: Palette = Palette {
    accent: Color::Rgb(0x6c, 0x71, 0xc4),        // violet
    accent_bright: Color::Rgb(0x4b, 0x50, 0xa8), // violet, darkened
    fg: Color::Rgb(0x65, 0x7b, 0x83),            // base00
    dim: Color::Rgb(0x93, 0xa1, 0xa1),           // base1
    error: Color::Rgb(0xdc, 0x32, 0x2f),         // red
    tab_bg: Color::Rgb(0xee, 0xe8, 0xd5),        // base2
    on_accent: Color::Rgb(0xfd, 0xf6, 0xe3),     // base3
    on_tab: Color::Rgb(0x58, 0x6e, 0x75),        // base01
};

/// Palettes indexed by `Theme as usize`; the order must match the enum.
static PALETTES: [Palette; 2] = [LATTE, SOLARIZED_LIGHT];

/// Active theme index. Read on every colour access from the render thread and
/// written when the user picks a theme (also the render thread), so `Relaxed`
/// is sufficient: a lone `u8` with no ordering relationship to other memory.
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// A selectable colour theme. The discriminant indexes `PALETTES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    #[default]
    Latte,
    SolarizedLight,
}

impl Theme {
    /// Every theme, in selector order. Kept in lockstep with `PALETTES`.
    pub const ALL: [Theme; 2] = [Theme::Latte, Theme::SolarizedLight];

    /// Human-readable name shown in the selector and the tab-bar label.
    pub fn title(self) -> &'static str {
        match self {
            Theme::Latte => "Catppuccin Latte",
            Theme::SolarizedLight => "Solarized Light",
        }
    }
}

/// The palette for a specific theme. Pure — it does not read the global, so
/// tests can compare palettes without racing on `CURRENT`.
pub fn palette(theme: Theme) -> &'static Palette {
    &PALETTES[theme as usize]
}

fn active() -> &'static Palette {
    palette(active_theme())
}

/// The currently active theme (for the tab-bar label and to seed the picker).
pub fn active_theme() -> Theme {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Theme::SolarizedLight,
        _ => Theme::Latte,
    }
}

/// Switch the active theme. Cheap and lock-free; the next frame renders it.
pub fn set(theme: Theme) {
    CURRENT.store(theme as u8, Ordering::Relaxed);
}

/// Set the initial theme at startup. A named alias for `set` that documents the
/// one-time call in `main`.
pub fn init(theme: Theme) {
    set(theme);
}

// Colour accessors. Named after the palette field, suffixed `_color` only where
// a `Style` helper below already owns the bare name.

pub fn fg() -> Color {
    active().fg
}

pub fn accent_color() -> Color {
    active().accent
}

pub fn accent_bright() -> Color {
    active().accent_bright
}

pub fn dim_color() -> Color {
    active().dim
}

pub fn tab_bg() -> Color {
    active().tab_bg
}

pub fn on_accent() -> Color {
    active().on_accent
}

pub fn on_tab() -> Color {
    active().on_tab
}

// Style helpers: unchanged names and signatures, now reading the active palette.

pub fn accent() -> Style {
    Style::new().fg(active().accent)
}

pub fn selected() -> Style {
    Style::new()
        .fg(active().accent_bright)
        .add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::new().fg(active().dim)
}

pub fn error() -> Style {
    Style::new().fg(active().error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_titles() {
        assert_eq!(Theme::ALL.len(), 2);
        assert_eq!(Theme::Latte.title(), "Catppuccin Latte");
        assert_eq!(Theme::SolarizedLight.title(), "Solarized Light");
    }

    #[test]
    fn default_is_latte() {
        assert_eq!(Theme::default(), Theme::Latte);
        assert_eq!(palette(Theme::Latte).accent, LATTE.accent);
    }

    #[test]
    fn palettes_are_distinct() {
        // Guards that the two arrays are genuinely different and in the right
        // slots. Uses the pure `palette` lookup, so it never touches `CURRENT`.
        assert_ne!(
            palette(Theme::Latte).accent,
            palette(Theme::SolarizedLight).accent
        );
        assert_ne!(
            palette(Theme::Latte).on_tab,
            palette(Theme::SolarizedLight).on_tab
        );
    }

    #[test]
    fn set_switches_active_palette() {
        // The only test that mutates the shared `CURRENT`; keep it a single
        // function and restore the default so parallel tests don't race.
        set(Theme::SolarizedLight);
        assert_eq!(active_theme(), Theme::SolarizedLight);
        assert_eq!(accent_color(), palette(Theme::SolarizedLight).accent);
        set(Theme::Latte);
        assert_eq!(active_theme(), Theme::Latte);
        assert_eq!(accent_color(), palette(Theme::Latte).accent);
    }
}
