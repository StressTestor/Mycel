//! The static pixel mushroom, transcribed from `mycel-tui-mockup.html`.
//!
//! The mockup renders the shared 15x9 pixel map as eight hand-tuned rows of
//! half-block glyphs (`▄`, `█`, `▀`) plus a dashed root line. We reproduce those
//! exact rows and color each run from a theme role (`o` cap, `W` speck, `r` rim,
//! `s` stem, and the root line's border), so the logo recolors with the active
//! theme: amanita's orange cap, phosphor's green cap, and so on.

use crate::terminal::style::{Color, Span, Style, StyledLine};
use crate::tui::theme::Theme;

use super::fit_spans;

/// Visible width of the logo block in character cells.
pub const LOGO_WIDTH: usize = 16;

/// Which theme role paints a run of logo glyphs.
#[derive(Clone, Copy)]
enum Ink {
    Cap,
    Speck,
    Rim,
    Stem,
    Border,
}

/// The mushroom, row by row; each row is a list of `(role, glyphs)` runs copied
/// verbatim from the mockup.
const ROWS: &[&[(Ink, &str)]] = &[
    &[(Ink::Cap, "   ▄▄▄▄▄▄▄▄▄")],
    &[
        (Ink::Cap, " ███"),
        (Ink::Speck, "█"),
        (Ink::Cap, "█████"),
        (Ink::Speck, "█"),
        (Ink::Cap, "███"),
    ],
    &[
        (Ink::Cap, "██"),
        (Ink::Speck, "█"),
        (Ink::Cap, "█████"),
        (Ink::Speck, "█"),
        (Ink::Cap, "███"),
        (Ink::Speck, "█"),
        (Ink::Cap, "██"),
    ],
    &[(Ink::Rim, "▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀")],
    &[(Ink::Stem, "     █████")],
    &[(Ink::Stem, "     █████")],
    &[(Ink::Stem, "    ▄██████▄")],
    &[(Ink::Border, " ╌╌┴╌╌╌╌┬╌╌╌╌┴╌╌")],
];

fn ink_color(theme: &Theme, ink: Ink) -> Color {
    Color::Rgb(match ink {
        Ink::Cap => theme.cap,
        Ink::Speck => theme.speck,
        Ink::Rim => theme.rim,
        Ink::Stem => theme.stem,
        Ink::Border => theme.border,
    })
}

/// The logo as styled lines (spans only, each padded to `LOGO_WIDTH` cells).
/// Kept internal so the header card can place the spans beside the identity
/// block; the public `logo` renders them to strings.
pub(crate) fn logo_lines(theme: &Theme) -> Vec<StyledLine> {
    ROWS.iter()
        .map(|row| {
            let spans = row
                .iter()
                .map(|(ink, text)| Span::new(*text, Style::fg(ink_color(theme, *ink))))
                .collect::<Vec<_>>();
            StyledLine(fit_spans(spans, LOGO_WIDTH))
        })
        .collect()
}

/// The mushroom as rendered terminal lines, each `LOGO_WIDTH` cells wide.
/// Test-only: the header card is the only production consumer and it reaches
/// the spans through `logo_lines`, so a crate-visible wrapper would be dead
/// code under `-D warnings`.
#[cfg(test)]
fn logo(theme: &Theme, truecolor: bool) -> Vec<String> {
    logo_lines(theme)
        .iter()
        .map(|line| line.render(LOGO_WIDTH, truecolor))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_is_nonempty_and_themed() {
        let lines = logo(&Theme::amanita(), true);
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|line| !line.is_empty()));
        let joined = lines.join("\n");
        // amanita's cap is #e05a1e; the logo must carry its role color.
        assert!(joined.contains("38;2;224;90;30"));

        // Phosphor recolors the same glyphs to its green cap (#33ff66) and drops
        // amanita's orange entirely, proving the logo themes rather than hardcodes.
        let phosphor = logo(&Theme::phosphor(), true).join("\n");
        assert!(phosphor.contains("38;2;51;255;102"));
        assert!(!phosphor.contains("38;2;224;90;30"));
    }
}
