//! The candidate notification strip from `mycel-tui-mockup.html`.
//!
//! A single line between the welcome card and the transcript: an accent `▲`
//! marker plus an `accent_dim` call to action pointing at `/candidates`. It
//! renders only while candidates are pending, so a quiet substrate keeps the
//! center column unchanged.

use crate::terminal::style::{Color, Span, Style, StyledLine};
use crate::tui::theme::Theme;

/// Render the pending-candidate strip, or nothing when no candidates are
/// pending. Every returned line is at most `width` visible cells
/// (`StyledLine::render` clips overflow).
pub fn notification_strip(
    candidates_pending: u32,
    theme: &Theme,
    width: usize,
    truecolor: bool,
) -> Vec<String> {
    if candidates_pending == 0 {
        return Vec::new();
    }
    let line = StyledLine(vec![
        Span::new("▲ ", Style::fg(Color::Rgb(theme.accent))),
        Span::new(
            format!(
                "{} pending review · run /candidates",
                crate::util::count_noun(u64::from(candidates_pending), "candidate", "candidates")
            ),
            Style::fg(Color::Rgb(theme.accent_dim)),
        ),
    ]);
    vec![line.render(width, truecolor)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::visible_width;

    #[test]
    fn pending_candidates_render_the_accent_strip() {
        let lines = notification_strip(1, &Theme::amanita(), 120, true);
        assert_eq!(lines.len(), 1);
        // Accent (#e05a1e) marker, accent_dim (#8a3c18) copy from the mockup.
        assert!(lines[0].contains("38;2;224;90;30m▲"));
        assert!(lines[0].contains("38;2;138;60;24m1 candidate pending review · run /candidates"));
        // The noun agrees with larger counts.
        let plural = notification_strip(3, &Theme::amanita(), 120, true);
        assert!(plural[0].contains("3 candidates pending review"));
    }

    #[test]
    fn no_pending_candidates_render_nothing() {
        assert!(notification_strip(0, &Theme::amanita(), 120, true).is_empty());
    }

    #[test]
    fn strip_stays_within_narrow_widths() {
        for width in [0usize, 1, 5, 10, 20, 40] {
            for line in notification_strip(3, &Theme::amanita(), width, true) {
                assert!(
                    visible_width(&line) <= width,
                    "width {width}: line is {} cells",
                    visible_width(&line)
                );
            }
        }
    }
}
