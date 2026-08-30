//! Pure TUI components. Each produces `Vec<String>` (SGR-coded terminal lines)
//! from a plain data snapshot plus a `Theme`: no I/O, no async, unit-tested
//! against the produced strings so a theme change never churns the tests.

pub mod flourish;
pub mod header;
pub mod input_box;
pub mod inspector;
pub mod logo;
pub mod notification;
pub mod session_rail;
pub mod status_bar;
pub mod transcript;

use crate::terminal::compose::clip_and_pad;
use crate::terminal::style::Span;

/// Fit a run of spans to exactly `target` visible cells: clip any overflow and
/// pad short content with trailing spaces. Delegates to the same core as
/// `compose::push_column` so a component cell aligns the same way the region
/// compositor does.
pub(crate) fn fit_spans(spans: Vec<Span>, target: usize) -> Vec<Span> {
    clip_and_pad(&spans, target)
}
