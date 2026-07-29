//! Splitting a poster column off the left of a text area, aspect-correct and
//! self-suppressing when there is no room.
//!
//! Pure geometry. The cell size arrives as a parameter rather than being read
//! from the image subsystem, so the arithmetic every view depends on is testable
//! without a terminal, a network or a decoded image.
//!
//! Nothing here consults the poster cache, and that is the point: because a
//! split depends only on the area and the cell size, the text layout is
//! identical whether artwork is loading, absent or already drawn. Reserving the
//! column up front is what stops a poster landing later from reflowing prose the
//! user is in the middle of reading.
//!
//! Only detail and header views get a poster. Per-row thumbnails were tried and
//! dropped: at the three- and four-row item rhythms the lists use, a 2:3 poster
//! comes out three or four columns wide, which reads as a smudge rather than
//! artwork, and growing the rows to fix that costs more items per screen than the
//! pictures are worth.

use ratatui::layout::Rect;

/// Poster aspect ratio, width:height. Jellyfin `Primary` images and the
/// TMDb/TVDb art Radarr and Sonarr mirror are all 2:3.
const POSTER_W: u32 = 2;
const POSTER_H: u32 = 3;

/// Blank columns to the left of the poster, matching the two-space indent every
/// text line in the app already carries, so artwork sits on the same margin as
/// the prose rather than hard against the terminal edge.
const INSET: u16 = 2;

/// Blank columns between the poster and the prose beside it. Zero, because the
/// text lines bring their own two-space indent: a gap here as well left the right
/// side visibly looser than the left.
const GAP: u16 = 0;

/// The poster takes roughly `1/DIVISOR` of the width, bounded both ways so it
/// stays a poster rather than a stamp on a wide terminal or a wall on a narrow
/// one.
const DIVISOR: u16 = 5;
const MIN_COLS: u16 = 10;
const MAX_COLS: u16 = 24;
const MIN_ROWS: u16 = 6;
/// Prose floor: never squeeze the text column below this to fit artwork.
const MIN_TEXT: u16 = 40;
const MIN_AREA_W: u16 = 60;

/// Split `area` into a left-hand poster slot and the text area beside it.
///
/// `poster` is `None` when the area cannot carry one, in which case `text` is
/// `area` untouched and the view renders exactly as it did before posters
/// existed. `cell` is the terminal's cell size in pixels, width then height.
///
/// `max_rows` is the caller's vertical budget, which is often smaller than the
/// area: a series header has to leave rows for the list beneath it. The width is
/// chosen first, the height follows from the aspect ratio, and if that exceeds
/// `max_rows` the width is *re-derived* from the capped height so the poster stays
/// in proportion instead of stretching.
///
/// Two invariants callers rely on:
///
/// - `text` always ends flush with the right edge of `area`, and always has
///   `area.height`. Every existing `width.saturating_sub(1)` scrollbar
///   reservation and right-aligned column therefore lands on the same physical
///   column as before, so `crate::ui::list::draw_scrollbar` keeps being handed
///   the original `area`.
/// - `poster` is always inside `area`. Callers must still avoid handing in an
///   area flush with the bottom of the terminal: some Sixel implementations
///   scroll the screen when an image touches the final row. The shell's status
///   bar and each app's help row keep the body clear of it.
pub fn split(area: Rect, max_rows: u16, cell: (u16, u16)) -> (Option<Rect>, Rect) {
    let (cell_w, cell_h) = cell;
    if cell_w == 0 || cell_h == 0 {
        return (None, area);
    }
    let max_rows = max_rows.min(area.height);
    if area.width < MIN_AREA_W || max_rows < MIN_ROWS {
        return (None, area);
    }
    let mut cols = (area.width / DIVISOR).clamp(MIN_COLS, MAX_COLS);
    cols = cols.min(area.width.saturating_sub(INSET + GAP + MIN_TEXT));
    let mut rows = rows_for_cols(cols, cell);
    if rows > max_rows {
        // Height-capped: re-derive the width so the poster keeps its proportions
        // rather than being squashed into the shorter slot.
        rows = max_rows;
        cols = cols_for_rows(rows, cell).min(cols);
    }
    if cols < MIN_COLS || rows < MIN_ROWS {
        return (None, area);
    }
    let taken = INSET + cols + GAP;
    (
        Some(Rect::new(area.x + INSET, area.y, cols, rows)),
        Rect::new(area.x + taken, area.y, area.width - taken, area.height),
    )
}

/// Widest column count that keeps a 2:3 poster inside `rows` terminal rows.
fn cols_for_rows(rows: u16, cell: (u16, u16)) -> u16 {
    let pixels_high = u32::from(rows) * u32::from(cell.1);
    let pixels_wide = pixels_high * POSTER_W / POSTER_H;
    clamp_u16(pixels_wide.div_ceil(u32::from(cell.0)))
}

/// Rows a 2:3 poster needs at `cols` columns wide.
fn rows_for_cols(cols: u16, cell: (u16, u16)) -> u16 {
    let pixels_wide = u32::from(cols) * u32::from(cell.0);
    let pixels_high = pixels_wide * POSTER_H / POSTER_W;
    clamp_u16(pixels_high.div_ceil(u32::from(cell.1)))
}

/// Rounding up in both directions is safe: the renderer letterboxes inside the
/// slot it is given, so a one-cell overshoot shows as a blank strip rather than
/// a distorted image.
fn clamp_u16(value: u32) -> u16 {
    value.min(u32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A common 8x16 cell: two vertical pixels per cell, as most fonts give.
    const CELL: (u16, u16) = (8, 16);

    #[test]
    fn text_is_always_flush_right_and_full_height() {
        // The invariant every scrollbar and right-aligned column depends on. If
        // this ever breaks, every view misplaces its scrollbar.
        for width in 0..200u16 {
            for height in 0..40u16 {
                let area = Rect::new(3, 2, width, height);
                for max_rows in [height, 10, 6] {
                    let (poster, text) = split(area, max_rows, CELL);
                    assert_eq!(
                        text.x + text.width,
                        area.x + area.width,
                        "text not flush right for {width}x{height} max {max_rows}"
                    );
                    assert_eq!(text.height, area.height);
                    assert_eq!(text.y, area.y);
                    if let Some(poster) = poster {
                        assert!(
                            poster.x + poster.width <= text.x,
                            "poster overlaps text for {width}x{height}"
                        );
                        assert!(
                            poster.y + poster.height <= area.y + area.height,
                            "poster runs past the area for {width}x{height}"
                        );
                        assert!(poster.x >= area.x);
                    } else {
                        assert_eq!(text, area, "suppressed split must return the area as-is");
                    }
                }
            }
        }
    }

    #[test]
    fn the_poster_is_inset_from_the_left_edge_and_flush_against_the_text() {
        // What the margins should look like: the poster starts on the same
        // two-column margin as every text line, and the text's own indent
        // provides the gap on the other side, so the two sides look even.
        let area = Rect::new(0, 0, 80, 20);
        let (poster, text) = split(area, 20, CELL);
        let poster = poster.expect("80 columns fits a poster");
        assert_eq!(poster.x, 2, "poster should sit on the standard left margin");
        assert_eq!(
            text.x,
            poster.x + poster.width,
            "no extra gap on the right: the text brings its own indent"
        );
    }

    #[test]
    fn sizes_match_the_documented_table() {
        // These are the sizes the design was signed off against; a change here is
        // a visible change to every detail view.
        let cases = [
            (120u16, 34u16, Some((24u16, 18u16))),
            (80, 18, Some((16, 12))),
            (80, 8, Some((11, 8))),
            (60, 18, Some((12, 9))),
        ];
        for (width, height, expected) in cases {
            let area = Rect::new(0, 0, width, height);
            let (poster, _) = split(area, height, CELL);
            assert_eq!(
                poster.map(|rect| (rect.width, rect.height)),
                expected,
                "unexpected poster for {width}x{height}"
            );
        }
    }

    #[test]
    fn a_height_cap_reshrinks_the_width_to_keep_the_aspect() {
        // The series-header case: only 10 rows to spare on an 80-column body.
        // Capping the height alone would stretch the poster, so the width has to
        // come back down with it.
        let area = Rect::new(0, 0, 80, 18);
        assert_eq!(
            split(area, 18, CELL).0.map(|r| (r.width, r.height)),
            Some((16, 12))
        );
        assert_eq!(
            split(area, 10, CELL).0.map(|r| (r.width, r.height)),
            Some((14, 10))
        );
    }

    #[test]
    fn suppression_boundaries() {
        // 60 columns is the documented cut-off, exactly.
        let tall = 30;
        assert!(split(Rect::new(0, 0, 60, tall), tall, CELL).0.is_some());
        assert!(split(Rect::new(0, 0, 59, tall), tall, CELL).0.is_none());
        // A small vertical budget suppresses however wide the area is, because the
        // height cap pulls the width down with it. Note which floor bites first:
        // on an 8x16 cell a 6-row poster is only 8 columns wide, so MIN_COLS
        // rejects it before MIN_ROWS ever would. Both floors are still needed — on
        // a square-cell terminal the row floor is the one that binds.
        let wide = Rect::new(0, 0, 200, 30);
        assert!(split(wide, 7, CELL).0.is_some());
        assert!(split(wide, 6, CELL).0.is_none());
        // A budget larger than the area is clamped to the area, not trusted.
        assert!(split(Rect::new(0, 0, 200, 4), 99, CELL).0.is_none());
    }

    #[test]
    fn square_cells_still_work_just_differently_proportioned() {
        // A 2:3 poster needs far more rows when a cell is square, so the height
        // cap bites and pulls the width down with it.
        let area = Rect::new(0, 0, 120, 30);
        assert_eq!(
            split(area, 30, (8, 8))
                .0
                .map(|rect| (rect.width, rect.height)),
            Some((20, 30))
        );
    }

    #[test]
    fn unreported_cell_size_suppresses_rather_than_dividing_by_zero() {
        // `window_size` reports 0x0 pixels on terminals that do not fill it in.
        let area = Rect::new(0, 0, 120, 30);
        for cell in [(0, 0), (0, 16), (8, 0)] {
            assert_eq!(split(area, 30, cell), (None, area));
        }
    }

    #[test]
    fn degenerate_areas_are_handled_without_panicking() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 0, 30),
            Rect::new(0, 0, 200, 0),
            Rect::new(u16::MAX - 2, u16::MAX - 2, 2, 2),
        ] {
            assert_eq!(split(area, 30, CELL), (None, area));
        }
    }

    #[test]
    fn split_is_deterministic() {
        // Callers may compute the split more than once per frame, so identical
        // inputs must give identical answers.
        let area = Rect::new(0, 0, 97, 23);
        let first = split(area, 11, CELL);
        for _ in 0..5 {
            assert_eq!(split(area, 11, CELL), first);
        }
    }
}
