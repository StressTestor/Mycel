//! The omp-style welcome card from `mycel-tui-mockup.html`.
//!
//! A dashed border box with an overlapping `mycel <version>` label tab. The left
//! block places the pixel logo beside the session identity (model + context,
//! provider + gate, cwd); a dashed divider separates it from the right block's
//! `tips`, `substrate`, and `recent` sections. Everything colors from the active
//! `Theme`, so the whole card recolors across all seven themes.

use crate::terminal::style::{Color, Span, Style, StyledLine};
use crate::terminal::visible_width;
use crate::tui::theme::Theme;

use super::fit_spans;
use super::logo::{logo_lines, LOGO_WIDTH};

/// A one-line snapshot of the substrate for the card's `substrate` section.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubstrateSummary {
    pub antibodies: u32,
    pub candidates_pending: u32,
    pub gate_ok: bool,
}

/// The plain data the welcome card renders. No handles, no I/O — a snapshot the
/// interactive loop fills in once at construction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderData {
    pub model: String,
    pub provider: String,
    pub cwd: String,
    // TODO(PR4): wire live context usage (rail data)
    pub ctx_used: u64,
    pub ctx_window: u64,
    pub substrate: SubstrateSummary,
    pub recent: Vec<String>,
}

/// Cells between the logo and the identity block.
const GAP: usize = 2;
/// Width reserved for the identity block (fits `<model> (NNNk context)`).
const IDENT_W: usize = 34;
/// Grid rows the identity block (3 lines) and right block (6 lines) start at, so
/// each is vertically centered against the 8-row logo.
const IDENT_OFFSET: usize = 2;
const RIGHT_OFFSET: usize = 1;

/// Render the welcome card to terminal lines. Every returned line is at most
/// `width` visible cells (`StyledLine::render` clips overflow), and the layout
/// degrades left-to-right as `width` shrinks: the right column collapses first,
/// then the identity block, and the logo survives longest.
pub fn header_card(data: &HeaderData, theme: &Theme, width: usize, truecolor: bool) -> Vec<String> {
    let border = Style::fg(Color::Rgb(theme.border));
    let gap = Style::default();

    let logo = logo_lines(theme);
    let identity = identity_lines(data, theme);
    let right = right_lines(data, theme);

    // left border(2) + logo + gap + identity + divider(3) + right border(2).
    let reserved = 2 + LOGO_WIDTH + GAP + IDENT_W + 3 + 2;
    let right_w = width.saturating_sub(reserved);

    let mut lines = Vec::with_capacity(logo.len() + 2);
    lines.push(top_border(theme, width).render(width, truecolor));
    for (row, logo_line) in logo.iter().enumerate() {
        let mut spans = vec![Span::new("╎ ", border)];
        spans.extend(fit_spans(logo_line.0.clone(), LOGO_WIDTH));
        spans.push(Span::new(" ".repeat(GAP), gap));
        spans.extend(fit_spans(cell_at(&identity, IDENT_OFFSET, row), IDENT_W));
        spans.push(Span::new(" ╎ ", border));
        spans.extend(fit_spans(cell_at(&right, RIGHT_OFFSET, row), right_w));
        spans.push(Span::new(" ╎", border));
        lines.push(StyledLine(spans).render(width, truecolor));
    }
    lines.push(bottom_border(theme, width).render(width, truecolor));
    lines
}

/// The dashed top rule carrying the overlapping `mycel <version>` label tab.
fn top_border(theme: &Theme, width: usize) -> StyledLine {
    let border = Style::fg(Color::Rgb(theme.border));
    let name = Style::fg(Color::Rgb(theme.accent)).bold();
    let version_style = Style::fg(Color::Rgb(theme.accent_dim));
    let version = concat!("v", env!("CARGO_PKG_VERSION"));
    // "╭╌╌ "(4) + "mycel"(5) + " "(1) + version + " "(1) + fill + "╮"(1).
    let used = 4 + 5 + 1 + visible_width(version) + 1 + 1;
    let fill = width.saturating_sub(used);
    StyledLine(vec![
        Span::new("╭╌╌ ", border),
        Span::new("mycel", name),
        Span::new(" ", Style::default()),
        Span::new(version, version_style),
        Span::new(" ", Style::default()),
        Span::new("╌".repeat(fill), border),
        Span::new("╮", border),
    ])
}

/// The dashed bottom rule closing the card.
fn bottom_border(theme: &Theme, width: usize) -> StyledLine {
    let border = Style::fg(Color::Rgb(theme.border));
    let dashes = "╌".repeat(width.saturating_sub(2));
    StyledLine(vec![Span::new(format!("╰{dashes}╯"), border)])
}

/// The three identity lines: model + context window, provider + gate, cwd.
fn identity_lines(data: &HeaderData, theme: &Theme) -> Vec<StyledLine> {
    let value = Style::fg(Color::Rgb(theme.value));
    let muted = Style::fg(Color::Rgb(theme.muted));
    let secondary = Style::fg(Color::Rgb(theme.secondary));
    vec![
        StyledLine(vec![
            Span::new(data.model.clone(), value),
            Span::new(format!(" ({} context)", format_k(data.ctx_window)), muted),
        ]),
        StyledLine(vec![Span::new(
            format!("{} · gate fail-closed", data.provider),
            secondary,
        )]),
        StyledLine(vec![Span::new(data.cwd.clone(), muted)]),
    ]
}

/// The right block: `tips`, `substrate`, `recent`, with `secondary` headers and
/// `muted` bodies. The candidate count is `accent` when pending, and the gate
/// dot is `ok` when the gate is healthy.
fn right_lines(data: &HeaderData, theme: &Theme) -> Vec<StyledLine> {
    let secondary = Style::fg(Color::Rgb(theme.secondary));
    let muted = Style::fg(Color::Rgb(theme.muted));
    let accent = Style::fg(Color::Rgb(theme.accent));
    let ok = Style::fg(Color::Rgb(theme.ok));

    let candidate_style = if data.substrate.candidates_pending > 0 {
        accent
    } else {
        muted
    };
    let (dot_style, gate_word) = if data.substrate.gate_ok {
        (ok, "ok")
    } else {
        (accent, "blocked")
    };
    let recent = if data.recent.is_empty() {
        "no recent sessions".to_owned()
    } else {
        data.recent.join(" · ")
    };

    vec![
        StyledLine(vec![Span::new("tips", secondary)]),
        StyledLine(vec![Span::new(
            "/ commands · ! shell · # note · shift+tab plan · esc cancel",
            muted,
        )]),
        StyledLine(vec![Span::new("substrate", secondary)]),
        StyledLine(vec![
            Span::new(
                format!("{} antibodies · ", data.substrate.antibodies),
                muted,
            ),
            Span::new(
                format!("{} candidate pending", data.substrate.candidates_pending),
                candidate_style,
            ),
            Span::new(" · gate fail-closed ", muted),
            Span::new("●", dot_style),
            Span::new(format!(" {gate_word}"), muted),
        ]),
        StyledLine(vec![Span::new("recent", secondary)]),
        StyledLine(vec![Span::new(recent, muted)]),
    ]
}

/// The `row`-th line of a block whose first line sits at grid row `offset`, or an
/// empty run when this grid row falls outside the block.
fn cell_at(lines: &[StyledLine], offset: usize, row: usize) -> Vec<Span> {
    row.checked_sub(offset)
        .and_then(|index| lines.get(index))
        .map(|line| line.0.clone())
        .unwrap_or_default()
}

/// Format a token count as a compact `NNNk` string (200000 -> "200k").
fn format_k(tokens: u64) -> String {
    format!("{}k", tokens / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HeaderData {
        HeaderData {
            model: "claude-sonnet-4.6".to_owned(),
            provider: "anthropic".to_owned(),
            cwd: "~/dev/mycoforge".to_owned(),
            ctx_used: 41_200,
            ctx_window: 200_000,
            substrate: SubstrateSummary {
                antibodies: 23,
                candidates_pending: 1,
                gate_ok: true,
            },
            recent: vec!["cordyceps-patch".to_owned()],
        }
    }

    #[test]
    fn header_card_renders_identity_substrate_and_tips() {
        let width = 120;
        let lines = header_card(&sample(), &Theme::amanita(), width, true);
        let joined = lines.join("\n");

        for needle in [
            "mycel",
            "claude-sonnet-4.6",
            "200k context",
            "anthropic",
            "tips",
            "substrate",
            "recent",
            "23 antibodies",
        ] {
            assert!(joined.contains(needle), "missing {needle:?}");
        }
        // The candidate count carries amanita's accent (#e05a1e) directly.
        assert!(joined.contains("38;2;224;90;30m1 candidate"));
        // The dashed border frame is present.
        assert!(joined.contains('╎') && joined.contains('╭') && joined.contains('╰'));
        // Every rendered line stays within the target width.
        for line in &lines {
            assert!(
                visible_width(line) <= width,
                "line exceeds width {width}: {}",
                visible_width(line)
            );
        }
    }

    #[test]
    fn header_card_recolors_with_the_theme() {
        let phosphor = header_card(&sample(), &Theme::phosphor(), 120, true).join("\n");
        // phosphor accent #33ff66, and none of amanita's orange.
        assert!(phosphor.contains("38;2;51;255;102"));
        assert!(!phosphor.contains("38;2;224;90;30"));
    }

    #[test]
    fn header_card_survives_narrow_widths() {
        for width in [0usize, 1, 5, 10, 20, 30, 40, 59, 60] {
            let lines = header_card(&sample(), &Theme::amanita(), width, true);
            for line in &lines {
                assert!(
                    visible_width(line) <= width,
                    "width {width}: line is {} cells",
                    visible_width(line)
                );
            }
        }
    }

    #[test]
    fn candidate_count_is_muted_when_none_pending() {
        let mut data = sample();
        data.substrate.candidates_pending = 0;
        let joined = header_card(&data, &Theme::amanita(), 120, true).join("\n");
        // muted #626d61 immediately before the candidate count, not accent.
        assert!(joined.contains("38;2;98;109;97m0 candidate"));
    }
}
