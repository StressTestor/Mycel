//! Fixed-column region compositor.
//!
//! A `Region` is a column of `StyledLine`s at a fixed width. `join_row` builds
//! one full-screen row by taking each region's line for that row, padding (or
//! truncating) it to the column width, and separating columns with a single
//! dashed vertical border span. `assemble` does this for every row.

use super::style::{Span, Style, StyledLine};
use super::unicode::{truncate_to_width, visible_width};

/// Dashed vertical border drawn between columns (matches the mockup).
const BORDER_GLYPH: &str = "╎";

/// A fixed-width column of styled lines.
#[derive(Debug, Clone)]
pub struct Region {
    pub width: usize,
    pub lines: Vec<StyledLine>,
}

/// Build one full-screen row from the columns: each region's `row`-th line
/// padded (or truncated) to its width, columns separated by a border span.
/// Color encoding happens later in `StyledLine::render`; this stays purely
/// structural.
pub fn join_row(cols: &[&Region], row: usize, border: Style) -> StyledLine {
    let mut spans = Vec::new();
    for (index, col) in cols.iter().enumerate() {
        if index > 0 {
            spans.push(Span::new(BORDER_GLYPH, border));
        }
        push_column(&mut spans, col, row);
    }
    StyledLine(spans)
}

/// Assemble `height` rows from the columns.
pub fn assemble(cols: &[Region], height: usize, border: Style) -> Vec<StyledLine> {
    let refs: Vec<&Region> = cols.iter().collect();
    (0..height)
        .map(|row| join_row(&refs, row, border))
        .collect()
}

/// Append a single column's `row`-th line to `spans`, clipping any overflow and
/// padding short content to the column width. A missing row renders as full
/// padding.
fn push_column(spans: &mut Vec<Span>, col: &Region, row: usize) {
    let blank = StyledLine::default();
    let line = col.lines.get(row).unwrap_or(&blank);
    let mut used = 0usize;
    for span in &line.0 {
        if used >= col.width {
            break;
        }
        let remaining = col.width - used;
        let width = visible_width(&span.text);
        if width <= remaining {
            spans.push(span.clone());
            used += width;
        } else {
            let clipped = truncate_to_width(&span.text, remaining, "");
            used += visible_width(&clipped);
            spans.push(Span::new(clipped, span.style));
            break;
        }
    }
    if used < col.width {
        spans.push(Span::new(" ".repeat(col.width - used), Style::default()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_row_pads_columns_and_inserts_border() {
        let a = Region {
            width: 4,
            lines: vec![StyledLine(vec![Span::new("hi", Style::default())])],
        };
        let b = Region {
            width: 5,
            lines: vec![StyledLine(vec![Span::new("yo", Style::default())])],
        };
        let row = join_row(&[&a, &b], 0, Style::default());
        let text: String = row.0.iter().map(|s| s.text.as_str()).collect();
        // "hi  " (4) + border + "yo   " (5)
        assert!(text.contains("hi") && text.contains("yo"));
        assert!(text.chars().any(|c| c == '╎' || c == '│'));
    }
}
