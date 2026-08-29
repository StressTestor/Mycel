//! The full-width bottom status bar from `mycel-tui-mockup.html`.
//!
//! Left: the `▸▸ gate fail-closed <state>` badge plus keybind hints, every one
//! verified against the live reducer bindings (`tui/session.rs`) and the
//! interactive loop's command dispatch. Right: model, substrate counts, and
//! the `/candidates` pointer. Segments drop right-to-left when the width
//! cannot carry them all, the right group first.

use crate::terminal::style::{Color, Span, Style, StyledLine};
use crate::terminal::visible_width;
use crate::tui::theme::Theme;

use super::header::GateDisplay;

/// The live state the status bar renders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusBarData {
    pub gate: GateDisplay,
    pub model: String,
    pub antibodies: u32,
    pub candidates_pending: u32,
}

/// Cells between adjacent left-group segments (the mockup's 18px gap on the
/// ~7.9px cell grid rounds to 2).
const SEGMENT_GAP: usize = 2;

/// Render the status bar as one line of exactly `width` visible cells (padded
/// short, clipped at degenerate widths).
pub fn status_bar(data: &StatusBarData, theme: &Theme, width: usize, truecolor: bool) -> String {
    let secondary = Style::fg(Color::Rgb(theme.secondary));
    let muted = Style::fg(Color::Rgb(theme.muted));
    let accent = Style::fg(Color::Rgb(theme.accent));
    // Same nearest-role mapping as the input box's top rule: the mockup's
    // #8f978c key names map to `stem`.
    let key = Style::fg(Color::Rgb(theme.stem));

    let gate_word = match data.gate {
        GateDisplay::Ok => "ok",
        GateDisplay::Blocked => "blocked",
        GateDisplay::Disarmed => "disarmed",
        GateDisplay::Unknown => "unknown",
    };
    // Every hint is a live binding: enter submit / esc cancel / ctrl+j newline
    // (tui/session.rs `apply_key`), `/` command dispatch (production.rs
    // `process_actions` → `handle_session_command`), ctrl+c clear/cancel/quit
    // (`cancel_or_clear`), ctrl+l / ctrl+r rail toggles (`apply_key`).
    let left_segments: Vec<Vec<Span>> = vec![
        vec![Span::new(
            format!("▸▸ gate fail-closed {gate_word}"),
            secondary,
        )],
        hint("enter", "send", key, muted),
        hint("esc", "cancel", key, muted),
        hint("ctrl+j", "newline", key, muted),
        hint("/", "commands", key, muted),
        hint("ctrl+c", "quit", key, muted),
        hint("ctrl+l", "rail", key, muted),
        hint("ctrl+r", "inspector", key, muted),
    ];
    let right_segments: Vec<Vec<Span>> = vec![
        vec![Span::new(data.model.clone(), muted)],
        vec![Span::new(
            format!(
                "{} antibodies · {} candidate",
                data.antibodies, data.candidates_pending
            ),
            muted,
        )],
        vec![Span::new("/candidates", accent)],
    ];

    let segment_w =
        |segment: &[Span]| -> usize { segment.iter().map(|span| visible_width(&span.text)).sum() };
    let group_w = |segments: &[Vec<Span>], gap: usize| -> usize {
        segments
            .iter()
            .map(|segment| segment_w(segment))
            .sum::<usize>()
            + gap * segments.len().saturating_sub(1)
    };

    // Drop right-group segments right-to-left first, then left hints, keeping
    // at least the gate badge; a width too small even for that clips.
    let mut left = left_segments.len();
    let mut right = right_segments.len();
    let fits = |left: usize, right: usize| {
        let gap_between = if right > 0 { SEGMENT_GAP } else { 0 };
        group_w(&left_segments[..left], SEGMENT_GAP)
            + gap_between
            + group_w(&right_segments[..right], 3)
            <= width
    };
    while right > 0 && !fits(left, right) {
        right -= 1;
    }
    while left > 1 && !fits(left, right) {
        left -= 1;
    }

    let mut spans = Vec::new();
    for (index, segment) in left_segments[..left].iter().enumerate() {
        if index > 0 {
            spans.push(Span::new(" ".repeat(SEGMENT_GAP), Style::default()));
        }
        spans.extend(segment.iter().cloned());
    }
    if right > 0 {
        let mut tail = Vec::new();
        for (index, segment) in right_segments[..right].iter().enumerate() {
            if index > 0 {
                tail.push(Span::new(" · ", Style::fg(Color::Rgb(theme.dimmer))));
            }
            tail.extend(segment.iter().cloned());
        }
        let used: usize = spans
            .iter()
            .chain(tail.iter())
            .map(|span| visible_width(&span.text))
            .sum();
        // Right-align the right group with a flexible pad.
        spans.push(Span::new(
            " ".repeat(width.saturating_sub(used)),
            Style::default(),
        ));
        spans.extend(tail);
    }
    StyledLine(super::fit_spans(spans, width)).render(width, truecolor)
}

/// A `<key> <label>` hint pair: brighter key, muted label.
fn hint(name: &str, label: &str, key: Style, muted: Style) -> Vec<Span> {
    vec![
        Span::new(name.to_owned(), key),
        Span::new(format!(" {label}"), muted),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StatusBarData {
        StatusBarData {
            gate: GateDisplay::Ok,
            model: "claude-sonnet-4.6".to_owned(),
            antibodies: 23,
            candidates_pending: 1,
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

    #[test]
    fn wide_bar_renders_every_verified_hint_and_the_right_group() {
        let line = status_bar(&sample(), &Theme::amanita(), 200, true);
        let text = strip_ansi(&line);
        for needle in [
            "▸▸ gate fail-closed ok",
            "enter send",
            "esc cancel",
            "ctrl+j newline",
            "/ commands",
            "ctrl+c quit",
            "ctrl+l rail",
            "ctrl+r inspector",
            "claude-sonnet-4.6",
            "23 antibodies · 1 candidate",
            "/candidates",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in {text:?}");
        }
        assert_eq!(visible_width(&line), 200);
        assert!(text.trim_end().ends_with("/candidates"), "{text:?}");
        // `/candidates` carries the accent.
        assert!(line.contains("38;2;224;90;30m/candidates"));
    }

    #[test]
    fn gate_states_render_their_words() {
        for (gate, word) in [
            (GateDisplay::Blocked, "fail-closed blocked"),
            (GateDisplay::Disarmed, "fail-closed disarmed"),
            (GateDisplay::Unknown, "fail-closed unknown"),
        ] {
            let mut data = sample();
            data.gate = gate;
            let text = strip_ansi(&status_bar(&data, &Theme::amanita(), 200, true));
            assert!(text.contains(word), "{gate:?}: {text:?}");
        }
    }

    #[test]
    fn narrow_bar_drops_the_right_group_before_the_hints() {
        // 130 cells: the full left group fits (118), the right group does not.
        let text = strip_ansi(&status_bar(&sample(), &Theme::amanita(), 130, true));
        assert!(text.contains("▸▸ gate fail-closed ok"), "{text:?}");
        assert!(text.contains("ctrl+r inspector"), "{text:?}");
        assert!(!text.contains("/candidates"), "{text:?}");
        assert!(!text.contains("antibodies"), "{text:?}");

        // 80 cells: hints drop right-to-left; the tail hints go first.
        let text = strip_ansi(&status_bar(&sample(), &Theme::amanita(), 80, true));
        assert!(text.contains("▸▸ gate fail-closed ok"), "{text:?}");
        assert!(text.contains("/ commands"), "{text:?}");
        assert!(!text.contains("ctrl+c"), "{text:?}");
        assert!(!text.contains("ctrl+r"), "{text:?}");

        // 30 cells: only the gate badge survives.
        let text = strip_ansi(&status_bar(&sample(), &Theme::amanita(), 30, true));
        assert!(text.contains("▸▸ gate fail-closed ok"), "{text:?}");
        assert!(!text.contains("enter send"), "{text:?}");
    }

    #[test]
    fn bar_stays_within_every_width() {
        for width in [0usize, 1, 5, 10, 22, 30, 80, 120, 200] {
            let line = status_bar(&sample(), &Theme::amanita(), width, true);
            assert!(
                visible_width(&line) <= width,
                "width {width}: line is {} cells",
                visible_width(&line)
            );
        }
    }
}
