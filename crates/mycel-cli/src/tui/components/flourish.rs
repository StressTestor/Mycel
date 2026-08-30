//! The optional startup flourish: frame content only.
//!
//! A ~1s sequence played before the first differential paint when
//! `tui.toml [startup] flourish = true`: the pixel mushroom appears bottom-up
//! row by row (anchored, so revealed rows never move), then the `mycel`
//! wordmark with the theme's `tag` line, then the live gate line from the
//! substrate summary. This module is pure — it produces the frames; the
//! interactive loop owns the timing and the writes.

use crate::terminal::style::{Color, Span, Style, StyledLine};
use crate::terminal::visible_width;
use crate::tui::theme::Theme;

use super::header::{GateDisplay, SubstrateSummary};
use super::logo::logo_lines;

/// Rows of the final frame's content block: 8 logo rows, a blank, the
/// wordmark, the tag line, a blank, and the gate line.
const BLOCK_ROWS: usize = 13;

/// Build the flourish frame sequence. Each frame is a full-screen snapshot
/// (top `height` rows, lines at most `width` cells): eight bottom-up logo
/// reveals, the wordmark + tag frame, then the gate frame. Every value in the
/// gate line is live from the substrate summary, never sample copy.
pub fn flourish_frames(
    substrate: &SubstrateSummary,
    theme: &Theme,
    width: usize,
    height: usize,
    truecolor: bool,
) -> Vec<Vec<String>> {
    let logo_rows: Vec<String> = logo_lines(theme)
        .into_iter()
        .map(|line| centered(line.0, width).render(width, truecolor))
        .collect();
    let top_pad = height.saturating_sub(BLOCK_ROWS) / 2;

    let mut frames = Vec::with_capacity(logo_rows.len() + 2);
    for revealed in 1..=logo_rows.len() {
        let hidden = logo_rows.len() - revealed;
        let mut frame = vec![String::new(); top_pad + hidden];
        frame.extend(logo_rows[hidden..].iter().cloned());
        frame.truncate(height);
        frames.push(frame);
    }

    // `glow` themes approximate the web glow as bright + bold (spec §4); the
    // rest use the accent, matching the header card's wordmark tab.
    let wordmark = if theme.glow {
        Span::new("mycel", Style::fg(Color::Rgb(theme.bright)).bold())
    } else {
        Span::new("mycel", Style::fg(Color::Rgb(theme.accent)))
    };
    let tag = Span::new(theme.tag, Style::fg(Color::Rgb(theme.muted)));
    let mut named = vec![String::new(); top_pad];
    named.extend(logo_rows);
    named.push(String::new());
    named.push(centered(vec![wordmark], width).render(width, truecolor));
    named.push(centered(vec![tag], width).render(width, truecolor));
    named.truncate(height);
    frames.push(named.clone());

    named.push(String::new());
    named.push(centered(gate_line(substrate, theme), width).render(width, truecolor));
    named.truncate(height);
    frames.push(named);
    frames
}

/// The live gate line: the header card's dot semantics (ok green with its
/// verdict word, blocked/disarmed accent, unknown a wordless muted dot) plus
/// the pluralized substrate counts.
fn gate_line(substrate: &SubstrateSummary, theme: &Theme) -> Vec<Span> {
    let muted = Style::fg(Color::Rgb(theme.muted));
    let (dot_style, gate_word) = match substrate.gate {
        GateDisplay::Ok => (Style::fg(Color::Rgb(theme.ok)), Some("ok")),
        GateDisplay::Blocked => (Style::fg(Color::Rgb(theme.accent)), Some("blocked")),
        GateDisplay::Disarmed => (Style::fg(Color::Rgb(theme.accent)), Some("disarmed")),
        GateDisplay::Unknown => (muted, None),
    };
    let mut trailing = String::from(" gate fail-closed");
    if let Some(word) = gate_word {
        trailing.push(' ');
        trailing.push_str(word);
    }
    trailing.push_str(&format!(
        " · {} · {}",
        crate::util::count_noun(u64::from(substrate.antibodies), "antibody", "antibodies"),
        crate::util::count_noun(
            u64::from(substrate.candidates_pending),
            "candidate",
            "candidates"
        ),
    ));
    vec![Span::new("●", dot_style), Span::new(trailing, muted)]
}

/// Center a run of spans in `width` cells with a leading pad.
fn centered(spans: Vec<Span>, width: usize) -> StyledLine {
    let content: usize = spans.iter().map(|span| visible_width(&span.text)).sum();
    let pad = width.saturating_sub(content) / 2;
    let mut line = vec![Span::new(" ".repeat(pad), Style::default())];
    line.extend(spans);
    StyledLine(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SubstrateSummary {
        SubstrateSummary {
            antibodies: 23,
            candidates_pending: 1,
            gate: GateDisplay::Ok,
        }
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

    fn row_of(frame: &[String], needle: &str) -> Option<usize> {
        frame
            .iter()
            .position(|line| strip_ansi(line).contains(needle))
    }

    #[test]
    fn logo_reveals_bottom_up_and_stays_anchored() {
        let frames = flourish_frames(&sample(), &Theme::amanita(), 80, 24, true);
        assert_eq!(frames.len(), 10, "8 reveals + wordmark + gate");
        // Frame 1: only the root row, no cap yet.
        let root_row = row_of(&frames[0], "╌╌┴").expect("root row in frame 1");
        assert!(row_of(&frames[0], "▄▄▄").is_none(), "cap must appear last");
        // Frame 8: the full logo, with the root row at the same position.
        assert_eq!(row_of(&frames[7], "╌╌┴"), Some(root_row));
        assert!(row_of(&frames[7], "▄▄▄").is_some());
    }

    #[test]
    fn wordmark_frame_carries_the_theme_tag_before_the_gate_frame() {
        let frames = flourish_frames(&sample(), &Theme::amanita(), 120, 24, true);
        let named = &frames[8];
        assert!(row_of(named, "mycel").is_some());
        assert!(row_of(named, Theme::amanita().tag).is_some());
        assert!(row_of(named, "gate fail-closed").is_none());
        let full = &frames[9];
        assert!(
            row_of(full, "● gate fail-closed ok · 23 antibodies · 1 candidate").is_some(),
            "{full:?}"
        );
    }

    #[test]
    fn gate_line_uses_live_values_and_header_dot_semantics() {
        let mut substrate = sample();
        substrate.antibodies = 1;
        substrate.candidates_pending = 0;
        substrate.gate = GateDisplay::Unknown;
        let frames = flourish_frames(&substrate, &Theme::amanita(), 120, 24, true);
        let full = frames.last().expect("gate frame");
        // Wordless unknown dot in muted (#626d61), counts pluralized.
        let gate_row = row_of(full, "gate fail-closed").expect("gate line");
        let line = &full[gate_row];
        assert!(
            strip_ansi(line).contains("● gate fail-closed · 1 antibody · 0 candidates"),
            "{line:?}"
        );
        assert!(line.contains("38;2;98;109;97m●"), "{line:?}");
        assert!(!line.contains("38;2;85;168;104m●"), "{line:?}");
    }

    #[test]
    fn glow_themes_bold_bright_wordmark_and_plain_themes_do_not() {
        // hacker glows: bright #eaf7ff, bold first in the SGR run.
        let hacker = flourish_frames(&sample(), &Theme::hacker(), 120, 24, true);
        let wordmark = &hacker[8][row_of(&hacker[8], "mycel").expect("wordmark")];
        assert!(
            wordmark.contains("\x1b[1;38;2;234;247;255m"),
            "{wordmark:?}"
        );
        // amanita does not glow: accent, no bold.
        let amanita = flourish_frames(&sample(), &Theme::amanita(), 120, 24, true);
        let wordmark = &amanita[8][row_of(&amanita[8], "mycel").expect("wordmark")];
        assert!(wordmark.contains("\x1b[38;2;224;90;30m"), "{wordmark:?}");
        assert!(!wordmark.contains("\x1b[1;"), "{wordmark:?}");
    }

    #[test]
    fn frames_respect_terminal_bounds() {
        for (width, height) in [(0usize, 0usize), (10, 3), (20, 8), (80, 5), (200, 50)] {
            for frame in flourish_frames(&sample(), &Theme::amanita(), width, height, true) {
                assert!(frame.len() <= height, "frame taller than {height}");
                for line in &frame {
                    assert!(
                        visible_width(line) <= width,
                        "({width},{height}): line is {} cells",
                        visible_width(line)
                    );
                }
            }
        }
    }
}
