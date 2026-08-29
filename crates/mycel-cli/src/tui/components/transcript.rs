//! Rich transcript frame renderer from `mycel-tui-mockup.html`.
//!
//! Renders one `TranscriptFrame` to SGR-coded terminal lines: an `HH:MM:SS`
//! gutter, a per-kind marker column, then themed content. Wrapped continuation
//! lines indent past the gutter. Pure: no I/O, colors only through `Theme`.

use crate::terminal::style::{Color, Span, Style, StyledLine};
use crate::terminal::wrap_text;
use crate::tui::theme::Theme;
use crate::tui::transcript::{FrameKind, TranscriptFrame};

/// Per-render context: target width, truecolor support, and the spinner frame
/// index for running tool rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCtx {
    pub width: usize,
    pub truecolor: bool,
    pub spinner_phase: usize,
}

/// Gutter cells: 8 for `HH:MM:SS` plus a 2-cell gap.
const GUTTER_W: usize = 10;
/// Marker column cells: the marker glyph plus a 1-cell gap.
const MARKER_W: usize = 2;

/// Render one transcript frame to terminal lines, each at most `ctx.width`
/// visible cells.
pub fn transcript_frame_lines(
    frame: &TranscriptFrame,
    theme: &Theme,
    ctx: &FrameCtx,
) -> Vec<String> {
    plain_lines(frame, theme, ctx)
}

/// Marker glyph, marker style, and content style for the non-tool frame kinds.
fn marker_and_styles(kind: FrameKind, theme: &Theme) -> (&'static str, Style, Style) {
    let secondary = Style::fg(Color::Rgb(theme.secondary));
    let muted = Style::fg(Color::Rgb(theme.muted));
    match kind {
        FrameKind::User => ("❯", secondary, Style::fg(Color::Rgb(theme.bright))),
        FrameKind::Thinking => (
            "∴",
            Style::fg(Color::Rgb(theme.dimmer)),
            Style::fg(Color::Rgb(theme.dim)).italic(),
        ),
        FrameKind::Assistant => ("·", muted, Style::fg(Color::Rgb(theme.value))),
        FrameKind::Goal | FrameKind::Subagent => ("·", muted, Style::fg(Color::Rgb(theme.prompt))),
        _ => ("·", muted, muted),
    }
}

/// The gutter + marker frame layout shared by every non-tool kind.
fn plain_lines(frame: &TranscriptFrame, theme: &Theme, ctx: &FrameCtx) -> Vec<String> {
    let (marker, marker_style, content_style) = marker_and_styles(frame.kind, theme);
    let content_w = ctx.width.saturating_sub(GUTTER_W + MARKER_W).max(1);
    wrap_text(&frame.text, content_w)
        .into_iter()
        .enumerate()
        .map(|(row, text)| {
            let mut spans = Vec::with_capacity(3);
            if row == 0 {
                spans.push(gutter_span(frame.at_ms, theme));
                spans.push(Span::new(format!("{marker} "), marker_style));
            } else {
                spans.push(Span::new(" ".repeat(GUTTER_W + MARKER_W), Style::default()));
            }
            spans.push(Span::new(text, content_style));
            StyledLine(spans).render(ctx.width, ctx.truecolor)
        })
        .collect()
}

/// The 10-cell timestamp gutter: `HH:MM:SS` in `faint` plus a 2-cell gap.
fn gutter_span(at_ms: u64, theme: &Theme) -> Span {
    Span::new(
        format!("{}  ", gutter_text(at_ms)),
        Style::fg(Color::Rgb(theme.faint)),
    )
}

/// The frame's creation time as local wall-clock `HH:MM:SS`. `at_ms` is unix
/// epoch milliseconds; a value chrono cannot map (out of range, DST gap)
/// renders as a placeholder rather than panicking.
fn gutter_text(at_ms: u64) -> String {
    use chrono::TimeZone;
    i64::try_from(at_ms)
        .ok()
        .and_then(|ms| chrono::Local.timestamp_millis_opt(ms).single())
        .map(|at| at.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::visible_width;
    use crate::tui::transcript::ToolFrameStatus;

    fn frame(kind: FrameKind, text: &str) -> TranscriptFrame {
        TranscriptFrame {
            kind,
            text: text.to_owned(),
            streaming: false,
            tool_id: None,
            tool_status: None,
            entity_id: None,
            state: None,
            at_ms: 45_296_000,
        }
    }

    fn ctx() -> FrameCtx {
        FrameCtx {
            width: 100,
            truecolor: true,
            spinner_phase: 0,
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

    fn assert_within_width(lines: &[String], width: usize) {
        for line in lines {
            assert!(
                visible_width(line) <= width,
                "line exceeds width {width}: {} cells: {line:?}",
                visible_width(line)
            );
        }
    }

    #[test]
    fn user_frame_has_prompt_marker_and_bright_text() {
        let lines =
            transcript_frame_lines(&frame(FrameKind::User, "bump the timeout"), &Theme::amanita(), &ctx());
        let joined = lines.join("\n");
        assert!(joined.contains('❯'));
        assert!(joined.contains("bump the timeout"));
        // marker in secondary #8ba18c
        assert!(joined.contains("38;2;139;161;140"));
        // content in bright #dde3d8
        assert!(joined.contains("38;2;221;227;216"));
        assert_within_width(&lines, 100);
    }

    #[test]
    fn thinking_frame_is_italic_and_muted() {
        let lines = transcript_frame_lines(
            &frame(FrameKind::Thinking, "the loader lives in config.rs"),
            &Theme::amanita(),
            &ctx(),
        );
        let joined = lines.join("\n");
        assert!(joined.contains('∴'));
        // italic SGR: attribute 3 leads the content prefix
        assert!(joined.contains("\x1b[3;"));
        assert_within_width(&lines, 100);
    }

    #[test]
    fn assistant_frame_has_dot_marker_and_value_text() {
        let lines = transcript_frame_lines(
            &frame(FrameKind::Assistant, "Raising the gate timeout"),
            &Theme::amanita(),
            &ctx(),
        );
        let joined = lines.join("\n");
        assert!(joined.contains('·'));
        assert!(joined.contains("Raising the gate timeout"));
        // content in value #c3cabe
        assert!(joined.contains("38;2;195;202;190"));
        assert_within_width(&lines, 100);
    }

    #[test]
    fn status_frame_renders_muted() {
        let lines = transcript_frame_lines(
            &frame(FrameKind::Status, "session resumed"),
            &Theme::amanita(),
            &ctx(),
        );
        let joined = lines.join("\n");
        assert!(joined.contains("session resumed"));
        // muted #626d61
        assert!(joined.contains("38;2;98;109;97"));
        assert_within_width(&lines, 100);
    }

    #[test]
    fn gutter_is_a_faint_wall_clock_timestamp() {
        let lines = transcript_frame_lines(
            &frame(FrameKind::Assistant, "hello"),
            &Theme::amanita(),
            &ctx(),
        );
        let stripped = strip_ansi(&lines[0]);
        let gutter: Vec<char> = stripped.chars().take(8).collect();
        for (index, character) in gutter.iter().enumerate() {
            if index == 2 || index == 5 {
                assert_eq!(*character, ':', "gutter shape: {stripped:?}");
            } else {
                assert!(character.is_ascii_digit(), "gutter shape: {stripped:?}");
            }
        }
        // faint #4a544a
        assert!(lines[0].contains("38;2;74;84;74"));
    }

    #[test]
    fn long_text_wraps_with_a_blank_gutter_indent() {
        let text = "word ".repeat(40);
        let lines = transcript_frame_lines(
            &frame(FrameKind::Assistant, text.trim()),
            &Theme::amanita(),
            &ctx(),
        );
        assert!(lines.len() >= 2, "expected wrapping, got {lines:?}");
        let second = strip_ansi(&lines[1]);
        assert!(
            second.starts_with(&" ".repeat(10)),
            "continuation must indent past the gutter: {second:?}"
        );
        assert_within_width(&lines, 100);
    }

    #[test]
    fn goal_frame_content_uses_prompt_color() {
        let lines = transcript_frame_lines(
            &frame(FrameKind::Goal, "ship the release"),
            &Theme::amanita(),
            &ctx(),
        );
        // prompt #8ba18c styles the content
        assert!(lines.join("\n").contains("38;2;139;161;140"));
        assert_within_width(&lines, 100);
    }
}
