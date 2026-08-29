//! Pure TUI components. Each produces `Vec<String>` (SGR-coded terminal lines)
//! from a plain data snapshot plus a `Theme`: no I/O, no async, unit-tested
//! against the produced strings so a theme change never churns the tests.

pub mod header;
pub mod logo;
pub mod transcript;

use crate::terminal::style::{Span, Style};
use crate::terminal::{truncate_to_width, visible_width};

/// Fit a run of spans to exactly `target` visible cells: clip any overflow and
/// pad short content with trailing spaces. Mirrors `compose::push_column` so a
/// component cell aligns the same way the region compositor does.
pub(crate) fn fit_spans(spans: Vec<Span>, target: usize) -> Vec<Span> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= target {
            break;
        }
        let width = visible_width(&span.text);
        if used + width <= target {
            used += width;
            out.push(span);
        } else {
            let clipped = truncate_to_width(&span.text, target - used, "");
            used += visible_width(&clipped);
            out.push(Span::new(clipped, span.style));
            break;
        }
    }
    if used < target {
        out.push(Span::new(" ".repeat(target - used), Style::default()));
    }
    out
}
