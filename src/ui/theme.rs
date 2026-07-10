//! Colour palette and runtime theme selection.
//!
//! Two light themes ship: Catppuccin Latte (default) and Solarized Light.
//! Colour is split into two axes: the theme supplies the base
//! (`fg`/`dim`/`error`/`tab_bg`/`on_tab`), and — for themes that offer a choice
//! of accents — a separately selected accent supplies
//! `accent`/`accent_bright`/`on_accent`. Both axes live in process-wide atomics
//! read by the accessor functions each call, so the palette changes at runtime
//! without threading anything through `MediaApp::draw`. We only ever set
//! foreground colours (plus three small `accent`/`tab_bg` chrome fills); the
//! terminal's own background always shows through, so these light themes are
//! best paired with a light terminal.

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
    /// (selected title, cursor bar, modal border).
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

/// The three accent-dependent colours; the base theme supplies the rest.
#[derive(Debug, Clone, Copy)]
pub struct AccentColors {
    pub accent: Color,
    pub accent_bright: Color,
    pub on_accent: Color,
}

/// Catppuccin Latte (https://catppuccin.com/palette). The accent triple here is
/// the default (Rosewater) so `palette(Latte)` is coherent on its own; the
/// active accent overrides it (see `active`). The base fields are accent-independent.
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

/// Solarized Light (https://ethanschoonover.com/solarized). It offers no accent
/// choice, so its violet accent stands as-is. `accent_bright` is violet darkened.
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

/// Active accent index (into `Accent`), same reasoning as `CURRENT`.
static CURRENT_ACCENT: AtomicU8 = AtomicU8::new(0);

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

    /// Human-readable name shown in the Settings tab.
    pub fn title(self) -> &'static str {
        match self {
            Theme::Latte => "Catppuccin Latte",
            Theme::SolarizedLight => "Solarized Light",
        }
    }

    /// The accents this theme offers, in selector order. Empty for a theme with
    /// a single fixed accent (Solarized Light), which hides the Settings row.
    pub fn accents(self) -> &'static [Accent] {
        match self {
            Theme::Latte => &Accent::ALL,
            Theme::SolarizedLight => &[],
        }
    }
}

/// A selectable accent colour. Only applied for themes whose `accents()` is
/// non-empty; the discriminant indexes into `active_accent`'s mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Accent {
    #[default]
    Rosewater,
    Mauve,
    Green,
    Sky,
    Lavender,
}

impl Accent {
    /// Every accent, in selector order.
    pub const ALL: [Accent; 5] = [
        Accent::Rosewater,
        Accent::Mauve,
        Accent::Green,
        Accent::Sky,
        Accent::Lavender,
    ];

    /// Human-readable name shown in the Settings tab.
    pub fn title(self) -> &'static str {
        match self {
            Accent::Rosewater => "Rosewater",
            Accent::Mauve => "Mauve",
            Accent::Green => "Green",
            Accent::Sky => "Sky",
            Accent::Lavender => "Lavender",
        }
    }
}

/// The colours for a Catppuccin Latte accent. `accent_bright` is the swatch
/// hand-darkened for emphasis on the light background; `on_accent` is dark only
/// for the pale Rosewater and light for the more saturated hues.
pub fn accent_colors(accent: Accent) -> AccentColors {
    match accent {
        Accent::Rosewater => AccentColors {
            accent: Color::Rgb(0xdc, 0x8a, 0x78),
            accent_bright: Color::Rgb(0xb4, 0x5c, 0x42),
            on_accent: Color::Rgb(0x4c, 0x4f, 0x69),
        },
        Accent::Mauve => AccentColors {
            accent: Color::Rgb(0x88, 0x39, 0xef),
            accent_bright: Color::Rgb(0x6d, 0x1f, 0xc9),
            on_accent: Color::Rgb(0xef, 0xf1, 0xf5),
        },
        Accent::Green => AccentColors {
            accent: Color::Rgb(0x40, 0xa0, 0x2b),
            accent_bright: Color::Rgb(0x2f, 0x7a, 0x1f),
            on_accent: Color::Rgb(0xef, 0xf1, 0xf5),
        },
        Accent::Sky => AccentColors {
            accent: Color::Rgb(0x04, 0xa5, 0xe5),
            accent_bright: Color::Rgb(0x0b, 0x7e, 0xa8),
            on_accent: Color::Rgb(0xef, 0xf1, 0xf5),
        },
        Accent::Lavender => AccentColors {
            accent: Color::Rgb(0x72, 0x87, 0xfd),
            accent_bright: Color::Rgb(0x4a, 0x57, 0xc9),
            on_accent: Color::Rgb(0xef, 0xf1, 0xf5),
        },
    }
}

/// The base palette for a specific theme. Pure — it does not read the globals,
/// so tests can compare palettes without racing on `CURRENT`.
pub fn palette(theme: Theme) -> &'static Palette {
    &PALETTES[theme as usize]
}

/// The resolved palette: the active theme's base with the active accent mixed
/// in for themes that offer accents. Returned by value (`Palette` is `Copy`);
/// accessors just read one field off it, so this is effectively free.
fn active() -> Palette {
    let theme = active_theme();
    let mut p = *palette(theme);
    if !theme.accents().is_empty() {
        let a = accent_colors(active_accent());
        p.accent = a.accent;
        p.accent_bright = a.accent_bright;
        p.on_accent = a.on_accent;
    }
    p
}

/// The currently active theme (for the Settings tab and to seed its picker).
pub fn active_theme() -> Theme {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Theme::SolarizedLight,
        _ => Theme::Latte,
    }
}

/// The currently active accent.
pub fn active_accent() -> Accent {
    match CURRENT_ACCENT.load(Ordering::Relaxed) {
        1 => Accent::Mauve,
        2 => Accent::Green,
        3 => Accent::Sky,
        4 => Accent::Lavender,
        _ => Accent::Rosewater,
    }
}

/// Switch the active theme. Cheap and lock-free; the next frame renders it.
pub fn set(theme: Theme) {
    CURRENT.store(theme as u8, Ordering::Relaxed);
}

/// Switch the active accent. Cheap and lock-free; the next frame renders it.
pub fn set_accent(accent: Accent) {
    CURRENT_ACCENT.store(accent as u8, Ordering::Relaxed);
}

/// Set the initial theme at startup. A named alias for `set` that documents the
/// one-time call in `main`.
pub fn init(theme: Theme) {
    set(theme);
}

/// Set the initial accent at startup.
pub fn init_accent(accent: Accent) {
    set_accent(accent);
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

// Style helpers: unchanged names and signatures, reading the active palette.

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
    fn defaults_and_accent_lists() {
        assert_eq!(Theme::default(), Theme::Latte);
        assert_eq!(Accent::default(), Accent::Rosewater);
        assert_eq!(Accent::ALL.len(), 5);
        assert_eq!(Theme::Latte.accents().len(), 5);
        assert!(Theme::SolarizedLight.accents().is_empty());
    }

    #[test]
    fn palettes_and_accents_are_distinct() {
        // Pure lookups, so these never touch the globals.
        assert_ne!(palette(Theme::Latte).fg, palette(Theme::SolarizedLight).fg);
        assert_ne!(
            accent_colors(Accent::Rosewater).accent,
            accent_colors(Accent::Mauve).accent
        );
        assert_ne!(
            accent_colors(Accent::Green).accent,
            accent_colors(Accent::Sky).accent
        );
    }

    #[test]
    fn set_switches_active_theme_and_accent() {
        // The only test that mutates the shared globals; keep it a single
        // function and restore defaults so parallel tests don't race on them.
        set(Theme::SolarizedLight);
        assert_eq!(active_theme(), Theme::SolarizedLight);
        // Solarized has no accents, so its built-in accent stands.
        assert_eq!(accent_color(), palette(Theme::SolarizedLight).accent);

        set(Theme::Latte);
        set_accent(Accent::Mauve);
        assert_eq!(active_accent(), Accent::Mauve);
        // Latte overrides its accent with the selected one.
        assert_eq!(accent_color(), accent_colors(Accent::Mauve).accent);

        set(Theme::Latte);
        set_accent(Accent::Rosewater);
    }
}
