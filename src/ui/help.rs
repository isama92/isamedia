//! The expanded (`?`) help panel, shared by every browse app so the three
//! near-identical grids stay in step. An app describes its help as a list of
//! labelled [`Section`]s (one column each); the compact one-line help stays in
//! each app, since that is a single context-aware string built from the same
//! `help_entries` the `Actions` section reuses.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::ui::theme;

/// One `(key, description)` row of the help.
pub type Entry = (&'static str, &'static str);

/// A labelled group of entries, rendered as one column of the expanded help.
pub struct Section {
    pub title: &'static str,
    pub entries: Vec<Entry>,
}

/// The list-navigation keys every browse view and overview/info scroller
/// shares. Apps append their own context rows (for example Jellyfin's tab
/// switch) after cloning this with `.to_vec()`.
pub const NAV: [Entry; 5] = [
    ("↑/↓", "move"),
    ("←/→", "prev/next page"),
    ("pgup/pgdn", "page up/down"),
    ("g/home", "go to start"),
    ("G/end", "go to end"),
];

/// Rows the expanded help occupies: one header row plus the longest section's
/// entries. Each app sizes its help region from this so the layout tracks the
/// context-dependent entry count instead of a hard-coded height.
pub fn rows(sections: &[Section]) -> u16 {
    let longest = sections
        .iter()
        .map(|section| section.entries.len())
        .max()
        .unwrap_or(0);
    longest as u16 + 1
}

/// Render the expanded help: one column per section, a bold title row above its
/// `key  desc` rows. Columns are width-fitted to their content and clipped
/// (never panic) when the terminal is too small, mirroring the per-row guard
/// the old static grids used.
pub fn draw(frame: &mut Frame, area: Rect, sections: &[Section]) {
    // Fixed key field, matching the previous grid; every key we emit is at most
    // this wide, so left-padding aligns the descriptions without truncating.
    let key_w = 10usize;
    let buf = frame.buffer_mut();

    let constraints: Vec<Constraint> = sections
        .iter()
        .map(|section| {
            let desc_w = section
                .entries
                .iter()
                .map(|(_, desc)| desc.width())
                .chain(std::iter::once(section.title.width()))
                .max()
                .unwrap_or(0);
            // 2 indent + key field + 1 space + description + 2 gutter.
            Constraint::Length((2 + key_w + 1 + desc_w + 2) as u16)
        })
        .collect();
    let areas = Layout::horizontal(constraints).split(area);

    for (section, col) in sections.iter().zip(areas.iter()) {
        if col.height == 0 {
            continue;
        }
        Line::from(Span::styled(
            format!("  {}", section.title),
            Style::new()
                .fg(theme::dim_color())
                .add_modifier(Modifier::BOLD),
        ))
        .render(Rect::new(col.x, col.y, col.width, 1), buf);

        for (row, (key, desc)) in section.entries.iter().enumerate() {
            let y = row as u16 + 1; // +1 for the title row
            if y < col.height {
                Line::from(vec![
                    Span::styled(
                        format!("  {key:<key_w$}"),
                        Style::new().fg(theme::dim_color()),
                    ),
                    Span::styled(format!(" {desc}"), theme::dim()),
                ])
                .render(Rect::new(col.x, col.y + y, col.width, 1), buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_is_header_plus_longest_section() {
        assert_eq!(rows(&[]), 1);
        let sections = [
            Section {
                title: "Move",
                entries: NAV.to_vec(),
            },
            Section {
                title: "Actions",
                entries: vec![("q", "quit")],
            },
        ];
        // 1 header row + the 5-entry NAV section.
        assert_eq!(rows(&sections), 6);
    }
}
