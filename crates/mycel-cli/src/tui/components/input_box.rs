//! The drawn input box from `mycel-tui-mockup.html`.
//!
//! A three-part frame replacing the bare prompt: a `+╌╌ mycel ❯ …` top rule
//! carrying an inline status strip (model, gate state, running count, cwd), the
//! `╎ ❯ <buffer> ╎` input rows, and a `+╌ … ╌+` bottom rule. The mockup's
//! `ctx ▮▮▯▯ 21%` meter is omitted: context occupancy is not derivable from the
//! interactive loop's event stream (see `build_header` in production.rs), and a
//! made-up fill would be a lie. The mockup's ghost hint is likewise omitted: no
//! hint source exists in the interactive loop today, and inventing one is out
//! of scope.

use crate::terminal::style::{Color, Span, Style, StyledLine};
use crate::terminal::{visible_width, wrap_text};
use crate::tui::theme::Theme;

use super::fit_spans;
use super::header::GateDisplay;

/// The live state the input box renders. `running` counts the in-flight main
/// turn plus streaming hyphae, `cursor` is a byte offset into `text` (always a
/// char boundary, from the editor).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputBoxData {
    pub model: String,
    pub gate: GateDisplay,
    pub running: usize,
    pub cwd: String,
    pub shell_mode: bool,
    pub text: String,
    pub cursor: usize,
}

/// The rendered box plus where the terminal cursor lands inside it:
/// `cursor_row` is the 0-based row within `lines`, `cursor_column` the 1-based
/// visible column (already offset by the `╎ ❯ ` lead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedInputBox {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_column: usize,
}

/// Cells of the input row's trail ` ╎`.
const INPUT_TRAIL_W: usize = 2;

/// The gate state word for the top rule's `[gate]` segment. Unlike the header
/// card's wordless unknown dot there is no color channel here, so the honest
/// rendering of an unchecked state is the word itself.
fn gate_word(gate: GateDisplay) -> &'static str {
    match gate {
        GateDisplay::Ok => "ok",
        GateDisplay::Blocked => "blocked",
        GateDisplay::Disarmed => "disarmed",
        GateDisplay::Unknown => "unknown",
    }
}

/// Render the input box. Rows are exactly `width` cells wide (down to the
/// degenerate widths where `StyledLine::render` clips), the buffer wraps
/// inside the frame, and the top rule's segments drop right-to-left when the
/// width cannot carry them all.
pub fn input_box(
    data: &InputBoxData,
    theme: &Theme,
    width: usize,
    truecolor: bool,
) -> RenderedInputBox {
    let faint = Style::fg(Color::Rgb(theme.faint));
    let prompt = if data.shell_mode {
        Span::new("! ", Style::fg(Color::Rgb(theme.accent)))
    } else {
        Span::new("❯ ", Style::fg(Color::Rgb(theme.secondary)))
    };
    // The lead is measured, not assumed: this codebase's width model counts
    // `❯` (U+276F) as 2 cells (`terminal/unicode.rs` `is_wide`), so the
    // prompt-mode lead is 5 cells while shell mode's `!` lead is 4.
    let lead_w = 2 + visible_width(&prompt.text);
    let inner = width.saturating_sub(lead_w + INPUT_TRAIL_W).max(1);

    let mut lines = vec![top_rule(data, theme, width).render(width, truecolor)];

    let bright = Style::fg(Color::Rgb(theme.bright));
    let editor_lines = wrap_text(&data.text, inner);
    for (row, editor_line) in editor_lines.iter().enumerate() {
        let lead = if row == 0 {
            vec![Span::new("╎ ", faint), prompt.clone()]
        } else {
            vec![Span::new(format!("╎{}", " ".repeat(lead_w - 1)), faint)]
        };
        let mut spans = lead;
        spans.extend(fit_spans(
            vec![Span::new(editor_line.clone(), bright)],
            inner,
        ));
        spans.push(Span::new(" ╎", faint));
        lines.push(StyledLine(spans).render(width, truecolor));
    }
    lines.push(
        StyledLine(vec![Span::new(
            format!("+{}+", "╌".repeat(width.saturating_sub(2))),
            faint,
        )])
        .render(width, truecolor),
    );

    // `wrap_text` is a hard grapheme wrap, so wrapping the before-cursor
    // prefix at the same inner width reproduces the exact cell the cursor
    // occupies in the wrapped buffer above.
    let cursor_lines = wrap_text(&data.text[..data.cursor], inner);
    RenderedInputBox {
        lines,
        cursor_row: cursor_lines.len(),
        cursor_column: lead_w
            + visible_width(cursor_lines.last().map(String::as_str).unwrap_or(""))
            + 1,
    }
}

/// The top rule with its inline status strip. Segments drop right-to-left
/// (cwd, then running, then gate, then model) until the rule fits; below the
/// width of the bare `+╌╌ mycel ❯` lead it degrades to a plain dashed rule.
fn top_rule(data: &InputBoxData, theme: &Theme, width: usize) -> StyledLine {
    let faint = Style::fg(Color::Rgb(theme.faint));
    let secondary = Style::fg(Color::Rgb(theme.secondary));
    let value = Style::fg(Color::Rgb(theme.value));
    // Nearest theme role to the mockup's #8f978c strip labels (amanita stem is
    // #9aa79a); the status bar's key names map the same color to the same role.
    let label = Style::fg(Color::Rgb(theme.stem));

    let segments: Vec<Vec<Span>> = vec![
        vec![Span::new(format!(" [M] {} ", data.model), value)],
        vec![Span::new(
            format!(" [gate] {} ", gate_word(data.gate)),
            label,
        )],
        vec![Span::new(format!(" [{} running] ", data.running), label)],
        vec![Span::new(format!(" {} ", data.cwd), secondary)],
    ];

    let lead = vec![
        Span::new("+╌╌", faint),
        Span::new(" mycel ", secondary),
        Span::new("❯", faint),
    ];
    let lead_w: usize = lead.iter().map(|span| visible_width(&span.text)).sum();
    // The rule must keep at least one trailing dash and the closing `+`.
    let tail_min = 2;
    if width < lead_w + tail_min {
        return StyledLine(vec![Span::new(
            format!("+{}+", "╌".repeat(width.saturating_sub(2))),
            faint,
        )]);
    }

    let mut included = segments.len();
    let fits = |count: usize| {
        let segment_w: usize = segments[..count]
            .iter()
            .flatten()
            .map(|span| visible_width(&span.text))
            .sum();
        // One `╌` joiner between adjacent segments.
        lead_w + segment_w + count.saturating_sub(1) + tail_min <= width
    };
    while included > 0 && !fits(included) {
        included -= 1;
    }

    let mut spans = lead;
    for (index, segment) in segments[..included].iter().enumerate() {
        if index > 0 {
            spans.push(Span::new("╌", faint));
        }
        spans.extend(segment.iter().cloned());
    }
    let used: usize = spans.iter().map(|span| visible_width(&span.text)).sum();
    spans.push(Span::new(
        format!("{}+", "╌".repeat(width.saturating_sub(used + 1))),
        faint,
    ));
    StyledLine(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> InputBoxData {
        InputBoxData {
            model: "claude-sonnet-4.6".to_owned(),
            gate: GateDisplay::Ok,
            running: 1,
            cwd: "~/dev/mycoforge".to_owned(),
            shell_mode: false,
            text: String::new(),
            cursor: 0,
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
    fn top_rule_carries_the_live_status_segments() {
        let rendered = input_box(&sample(), &Theme::amanita(), 120, true);
        let top = strip_ansi(&rendered.lines[0]);
        for needle in [
            "+╌╌ mycel ❯",
            "[M] claude-sonnet-4.6",
            "[gate] ok",
            "[1 running]",
            "~/dev/mycoforge",
        ] {
            assert!(top.contains(needle), "missing {needle:?} in {top:?}");
        }
        assert!(top.ends_with('+'), "{top:?}");
        assert_eq!(visible_width(&rendered.lines[0]), 120);
        // The omitted mockup extras stay omitted: no ctx meter, no ghost hint.
        let all = rendered
            .lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<String>();
        assert!(!all.contains("ctx"), "{all}");
        assert!(!all.contains('▮'), "{all}");
    }

    #[test]
    fn segments_drop_right_to_left_when_narrow() {
        // 46 cells: lead(11) + model(23) + tail fits, nothing after it does.
        let rendered = input_box(&sample(), &Theme::amanita(), 46, true);
        let top = strip_ansi(&rendered.lines[0]);
        assert!(top.contains("[M] claude-sonnet-4.6"), "{top:?}");
        assert!(!top.contains("~/dev/mycoforge"), "{top:?}");
        assert!(!top.contains("running"), "{top:?}");
        assert!(!top.contains("[gate]"), "{top:?}");

        // Too narrow even for the wordmark: a plain closed rule.
        let tiny = input_box(&sample(), &Theme::amanita(), 8, true);
        assert_eq!(strip_ansi(&tiny.lines[0]), "+╌╌╌╌╌╌+");
    }

    #[test]
    fn gate_states_render_their_words() {
        for (gate, word) in [
            (GateDisplay::Ok, "[gate] ok"),
            (GateDisplay::Blocked, "[gate] blocked"),
            (GateDisplay::Disarmed, "[gate] disarmed"),
            (GateDisplay::Unknown, "[gate] unknown"),
        ] {
            let mut data = sample();
            data.gate = gate;
            let top = strip_ansi(&input_box(&data, &Theme::amanita(), 120, true).lines[0]);
            assert!(top.contains(word), "{gate:?}: {top:?}");
        }
    }

    #[test]
    fn empty_buffer_renders_one_input_row_with_the_cursor_after_the_lead() {
        let rendered = input_box(&sample(), &Theme::amanita(), 80, true);
        assert_eq!(rendered.lines.len(), 3, "rule + one row + rule");
        let row = strip_ansi(&rendered.lines[1]);
        assert!(row.starts_with("╎ ❯ "), "{row:?}");
        assert!(row.trim_end().ends_with('╎'), "{row:?}");
        assert_eq!(rendered.cursor_row, 1);
        // Lead `╎ ❯ ` measures 5 cells (`❯` is 2 in this width model).
        assert_eq!(rendered.cursor_column, 6);
    }

    #[test]
    fn shell_mode_swaps_the_prompt_glyph_at_the_same_offset() {
        let mut data = sample();
        data.shell_mode = true;
        data.text = "ls".to_owned();
        data.cursor = 2;
        let rendered = input_box(&data, &Theme::amanita(), 80, true);
        let row = strip_ansi(&rendered.lines[1]);
        assert!(row.starts_with("╎ ! ls"), "{row:?}");
        assert_eq!(rendered.cursor_row, 1);
        // Shell's `!` lead measures 4 cells (`!` is narrow, `❯` is not).
        assert_eq!(rendered.cursor_column, 4 + 2 + 1);
    }

    #[test]
    fn long_buffers_wrap_inside_the_frame_and_carry_the_cursor() {
        // Width 20 leaves inner 13 (5-cell lead + 2-cell trail). 30 chars
        // wrap onto 3 rows (13+13+4).
        let mut data = sample();
        data.text = "abcdefghijklmnopqrstuvwxyz0123".to_owned();
        data.cursor = data.text.len();
        let rendered = input_box(&data, &Theme::amanita(), 20, true);
        let rows: Vec<String> = rendered.lines.iter().map(|line| strip_ansi(line)).collect();
        assert_eq!(rows.len(), 5, "rule + three rows + rule: {rows:?}");
        assert!(rows[1].starts_with("╎ ❯ abcdefghijklm"), "{rows:?}");
        assert!(rows[2].starts_with("╎    nopqrstuvwxyz"), "{rows:?}");
        assert!(rows[3].starts_with("╎    0123"), "{rows:?}");
        for row in &rows[1..4] {
            assert!(row.trim_end().ends_with('╎'), "open-sided row: {row:?}");
            assert_eq!(visible_width(row), 20);
        }
        assert_eq!(rendered.cursor_row, 3);
        assert_eq!(rendered.cursor_column, 5 + 4 + 1);

        // Cursor mid-buffer lands on the row its grapheme wrapped onto.
        data.cursor = 20; // 'u', 8th char of the second row
        let rendered = input_box(&data, &Theme::amanita(), 20, true);
        assert_eq!(rendered.cursor_row, 2);
        assert_eq!(rendered.cursor_column, 5 + 7 + 1);
    }

    #[test]
    fn cursor_at_an_exact_wrap_boundary_stays_on_the_filled_row() {
        // Before-cursor exactly fills the inner width: the cursor reports the
        // cell after the row's last grapheme (column width-1), matching the
        // pre-box behavior of wrapping the prefix independently.
        let mut data = sample();
        data.text = "abcdefghijklmnopqrst".to_owned();
        data.cursor = 13; // == inner at width 20
        let rendered = input_box(&data, &Theme::amanita(), 20, true);
        assert_eq!(rendered.cursor_row, 1);
        assert_eq!(rendered.cursor_column, 5 + 13 + 1);
    }

    #[test]
    fn every_row_stays_within_narrow_widths() {
        let mut data = sample();
        data.text = "wrap me across several rows please".to_owned();
        data.cursor = 10;
        for width in [0usize, 1, 3, 7, 8, 12, 20, 46, 80] {
            let rendered = input_box(&data, &Theme::amanita(), width, true);
            for line in &rendered.lines {
                assert!(
                    visible_width(line) <= width,
                    "width {width}: line is {} cells",
                    visible_width(line)
                );
            }
        }
    }

    #[test]
    fn box_recolors_with_the_theme() {
        let mut data = sample();
        data.text = "hi".to_owned();
        data.cursor = 2;
        let phosphor = input_box(&data, &Theme::phosphor(), 120, true)
            .lines
            .join("\n");
        // phosphor secondary/prompt #33ff66; none of amanita's orange accent.
        assert!(phosphor.contains("38;2;51;255;102"));
        assert!(!phosphor.contains("38;2;224;90;30"));
    }
}
