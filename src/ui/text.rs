//! Text-shaping helpers shared by every app's list rendering.

/// Truncate to a terminal column budget with a trailing ellipsis. Measures
/// display width, not chars: CJK characters occupy two columns each.
pub fn truncate(text: &str, max_width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    if text.width() <= max_width {
        return text.to_string();
    }
    let budget = max_width.saturating_sub(1); // leave a column for the ellipsis
    let mut used = 0;
    let mut truncated = String::new();
    for c in text.chars() {
        let char_width = c.width().unwrap_or(0);
        if used + char_width > budget {
            break;
        }
        used += char_width;
        truncated.push(c);
    }
    truncated.push('…');
    truncated
}

/// Greedy word-wrap to a column budget, measuring display width like
/// `truncate`. Words wider than the budget are hard-split so no line ever
/// overflows. Paragraph breaks are not preserved; input is treated as one
/// stream of words.
pub fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    let max_width = max_width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split_whitespace() {
        let word_width = word.width();
        if current_width > 0 && current_width + 1 + word_width <= max_width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
            continue;
        }
        if current_width > 0 {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if word_width <= max_width {
            current = word.to_string();
            current_width = word_width;
            continue;
        }
        for c in word.chars() {
            let char_width = c.width().unwrap_or(0);
            if current_width + char_width > max_width && current_width > 0 {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(c);
            current_width += char_width;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// A faint connector exactly `width` columns wide: a horizontal rule padded
/// with a space at each end so it never touches the text it bridges. Used to
/// link a left label to a right-aligned value across the empty middle of a
/// list row. Widths below 2 collapse to plain spaces, since the padding alone
/// already fills them.
pub fn leader_line(width: usize) -> String {
    match width {
        0 | 1 => " ".repeat(width),
        _ => format!(" {} ", "─".repeat(width - 2)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_handles_multibyte() {
        assert_eq!(truncate("héllo wörld", 20), "héllo wörld");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn truncate_counts_display_width() {
        use unicode_width::UnicodeWidthStr;

        // 5 chars but 10 columns: fits a 10-column budget untouched...
        assert_eq!(truncate("こんにちは", 10), "こんにちは");
        // ...but a 6-column budget fits only 2 double-width chars + ellipsis.
        assert_eq!(truncate("こんにちは", 6), "こん…");
        // A double-width char never straddles the boundary.
        assert_eq!(truncate("こんにちは", 5), "こん…");
        assert!(truncate("攻殻機動隊 S01E01 (1995)", 12).width() <= 12);
    }

    #[test]
    fn wrap_fills_lines_greedily() {
        assert_eq!(
            wrap_text("Asta and Yuno were abandoned together", 12),
            vec!["Asta and", "Yuno were", "abandoned", "together"]
        );
        assert_eq!(wrap_text("", 10), Vec::<String>::new());
        assert_eq!(wrap_text("short", 10), vec!["short"]);
    }

    #[test]
    fn wrap_hard_splits_overlong_words() {
        use unicode_width::UnicodeWidthStr;

        let lines = wrap_text("a Supercalifragilistic word", 8);
        assert!(lines.iter().all(|line| line.width() <= 8), "{lines:?}");
        assert_eq!(lines.concat().replace(' ', ""), "aSupercalifragilisticword");
        // Double-width chars never straddle the boundary.
        let lines = wrap_text("こんにちはこんにちは", 5);
        assert!(lines.iter().all(|line| line.width() <= 5), "{lines:?}");
    }

    #[test]
    fn leader_line_fills_exact_width() {
        use unicode_width::UnicodeWidthStr;

        for width in 0..=40 {
            assert_eq!(leader_line(width).width(), width, "width {width}");
        }
    }

    #[test]
    fn leader_line_pads_and_collapses() {
        // Below 2 columns there is only room for the padding spaces.
        assert_eq!(leader_line(0), "");
        assert_eq!(leader_line(1), " ");
        // A rule never touches the text on either side.
        let line = leader_line(6);
        assert!(line.starts_with(' ') && line.ends_with(' '), "{line:?}");
        assert_eq!(line, " ──── ");
    }
}
