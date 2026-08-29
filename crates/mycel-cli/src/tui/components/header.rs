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

/// Design copy shared by the identity line and the substrate row; the wording
/// is frozen from the mockup.
const GATE_TAGLINE: &str = "gate fail-closed";

/// Cells between the logo and the identity block.
const GAP: usize = 2;
/// Bounds for the identity column, which is sized from its content: wide
/// enough for a provider-qualified alias, never so wide it starves the right
/// block.
const IDENT_MIN_W: usize = 24;
const IDENT_MAX_W: usize = 48;
/// Cells the box frame spends on its side borders: `╎ ` and ` ╎`.
const BORDERS_W: usize = 4;
/// Cells of the dashed divider between the identity and right blocks: ` ╎ `.
const DIVIDER_W: usize = 3;
/// A column narrower than this is dropped rather than squeezed to a sliver.
const COLLAPSE_W: usize = 8;
/// Grid rows the identity block (3 lines) and right block (6 lines) start at, so
/// each is vertically centered against the 8-row logo.
const IDENT_OFFSET: usize = 2;
const RIGHT_OFFSET: usize = 1;

/// Render the welcome card to terminal lines. Every returned line is at most
/// `width` visible cells (`StyledLine::render` clips overflow), and the layout
/// degrades right-to-left as `width` shrinks while staying a closed box: the
/// right column (and its divider) collapses first, then the identity block
/// shrinks and drops, and the logo with both border glyphs survives down to the
/// logo-only floor (`BORDERS_W + LOGO_WIDTH` cells); only below that do rows
/// ragged-clip.
pub fn header_card(data: &HeaderData, theme: &Theme, width: usize, truecolor: bool) -> Vec<String> {
    let border = Style::fg(Color::Rgb(theme.border));
    let gap = Style::default();

    let logo = logo_lines(theme);
    let identity = identity_lines(data, theme);
    let right = right_lines(data, theme);

    // Size the identity column from its widest line so a long qualified alias
    // is not clipped by a sample-string constant.
    let ident_w = identity
        .iter()
        .map(|line| line.0.iter().map(|span| visible_width(&span.text)).sum())
        .max()
        .unwrap_or(0)
        .clamp(IDENT_MIN_W, IDENT_MAX_W);
    let (ident_col, right_col) = collapse_columns(width, ident_w);
    let box_w = BORDERS_W
        + LOGO_WIDTH
        + ident_col.map_or(0, |ident_w| GAP + ident_w)
        + right_col.map_or(0, |right_w| DIVIDER_W + right_w);

    let mut lines = Vec::with_capacity(logo.len() + 2);
    lines.push(top_border(theme, box_w).render(width, truecolor));
    for (row, logo_line) in logo.iter().enumerate() {
        let mut spans = vec![Span::new("╎ ", border)];
        spans.extend(fit_spans(logo_line.0.clone(), LOGO_WIDTH));
        if let Some(ident_w) = ident_col {
            spans.push(Span::new(" ".repeat(GAP), gap));
            spans.extend(fit_spans(cell_at(&identity, IDENT_OFFSET, row), ident_w));
        }
        if let Some(right_w) = right_col {
            spans.push(Span::new(" ╎ ", border));
            spans.extend(fit_spans(cell_at(&right, RIGHT_OFFSET, row), right_w));
        }
        spans.push(Span::new(" ╎", border));
        lines.push(StyledLine(spans).render(width, truecolor));
    }
    lines.push(bottom_border(theme, box_w).render(width, truecolor));
    lines
}

/// The staged collapse: widths (if any) of the identity and right columns at
/// this terminal width. The right column goes first, then the identity column
/// shrinks below `ident_w` and drops, leaving the logo-only box.
fn collapse_columns(width: usize, ident_w: usize) -> (Option<usize>, Option<usize>) {
    let full_reserved = BORDERS_W + LOGO_WIDTH + GAP + ident_w + DIVIDER_W;
    let right_w = width.saturating_sub(full_reserved);
    if right_w >= COLLAPSE_W {
        return (Some(ident_w), Some(right_w));
    }
    let ident_avail = width.saturating_sub(BORDERS_W + LOGO_WIDTH + GAP);
    if ident_avail >= COLLAPSE_W {
        return (Some(ident_w.min(ident_avail)), None);
    }
    (None, None)
}

/// The dashed top rule carrying the overlapping `mycel <version>` label tab,
/// sized to the (possibly collapsed) box width.
fn top_border(theme: &Theme, box_w: usize) -> StyledLine {
    let border = Style::fg(Color::Rgb(theme.border));
    let name = Style::fg(Color::Rgb(theme.accent)).bold();
    let version_style = Style::fg(Color::Rgb(theme.accent_dim));
    let version = concat!("v", env!("CARGO_PKG_VERSION"));
    // "╭╌╌ "(4) + "mycel"(5) + " "(1) + version + " "(1) + fill + "╮"(1).
    let used = 4 + 5 + 1 + visible_width(version) + 1 + 1;
    let fill = box_w.saturating_sub(used);
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

/// The dashed bottom rule closing the card, sized to the box width.
fn bottom_border(theme: &Theme, box_w: usize) -> StyledLine {
    let border = Style::fg(Color::Rgb(theme.border));
    let dashes = "╌".repeat(box_w.saturating_sub(2));
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
            format!("{} · {GATE_TAGLINE}", data.provider),
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
            Span::new(format!(" · {GATE_TAGLINE} "), muted),
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

    fn strip_ansi(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(character) = chars.next() {
            if character == '\x1b' {
                for control in chars.by_ref() {
                    if control == 'm' {
                        break;
                    }
                }
            } else {
                out.push(character);
            }
        }
        out
    }

    #[test]
    fn header_card_survives_narrow_widths() {
        for width in [0usize, 1, 5, 10, 20, 25, 30, 40, 55, 59, 60, 80] {
            let lines = header_card(&sample(), &Theme::amanita(), width, true);
            for line in &lines {
                assert!(
                    visible_width(line) <= width,
                    "width {width}: line is {} cells",
                    visible_width(line)
                );
            }
            // Wherever the box renders at all (>= the logo-only floor), the
            // collapse must stay a closed box: every interior row ends with the
            // right border glyph instead of being ragged-clipped.
            if width >= 20 {
                for line in &lines[1..lines.len() - 1] {
                    let stripped = strip_ansi(line);
                    assert!(
                        stripped.trim_end().ends_with('╎'),
                        "width {width}: open-sided row: {stripped:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn qualified_model_alias_renders_fully_at_comfortable_widths() {
        let mut data = sample();
        // A provider-qualified collision alias (see provider_commands.rs), 37
        // chars: wider than any fixed sample-string column.
        data.model = "anthropic:claude-sonnet-4-6-20250929".to_owned();
        let joined = header_card(&data, &Theme::amanita(), 120, true)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("anthropic:claude-sonnet-4-6-20250929"),
            "alias must not be clipped: {joined}"
        );
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
