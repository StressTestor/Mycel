//! Rich transcript frame renderer from `mycel-tui-mockup.html`.
//!
//! Renders one `TranscriptFrame` to SGR-coded terminal lines: an `HH:MM:SS`
//! gutter, a per-kind marker column, then themed content. Wrapped continuation
//! lines indent past the gutter. Pure: no I/O, colors only through `Theme`.

use crate::terminal::style::{Color, Span, Style, StyledLine};
use crate::terminal::{truncate_to_width, visible_width, wrap_text};
use crate::tui::theme::Theme;
use crate::tui::transcript::{FrameKind, ToolFrameStatus, TranscriptFrame};

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
/// Tool row lead cells past the gutter: indent + tree glyph + gap + dot + gap.
const TOOL_LEAD_W: usize = 6;
/// Braille spinner frames for running tool rows, indexed by `spinner_phase`.
const SPINNER: [char; 8] = ['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];

/// Render one transcript frame to terminal lines, each at most `ctx.width`
/// visible cells.
pub fn transcript_frame_lines(
    frame: &TranscriptFrame,
    theme: &Theme,
    ctx: &FrameCtx,
) -> Vec<String> {
    match frame.kind {
        FrameKind::Tool => tool_lines(frame, theme, ctx),
        _ => plain_lines(frame, theme, ctx),
    }
}

/// A tool row: `⎿` tree glyph, status dot or spinner, the first text line as
/// the head with a right-aligned gate status, then faint or diff-styled
/// subtext lines.
fn tool_lines(frame: &TranscriptFrame, theme: &Theme, ctx: &FrameCtx) -> Vec<String> {
    let dimmer = Style::fg(Color::Rgb(theme.dimmer));
    let muted = Style::fg(Color::Rgb(theme.muted));
    let value = Style::fg(Color::Rgb(theme.value));
    let (dot, dot_style) = match frame.tool_status {
        Some(ToolFrameStatus::Running) => (
            SPINNER[ctx.spinner_phase % SPINNER.len()].to_string(),
            Style::fg(Color::Rgb(theme.secondary)),
        ),
        Some(ToolFrameStatus::Failed) => ("●".to_owned(), Style::fg(Color::Rgb(theme.accent))),
        Some(ToolFrameStatus::Completed) | None => {
            ("●".to_owned(), Style::fg(Color::Rgb(theme.ok)))
        }
    };
    let status = match frame.tool_status {
        Some(ToolFrameStatus::Running) => "gate allow · running",
        Some(ToolFrameStatus::Failed) => "failed",
        Some(ToolFrameStatus::Completed) | None => "gate allow · done",
    };

    let content_w = ctx.width.saturating_sub(GUTTER_W + TOOL_LEAD_W).max(1);
    let mut text_lines = frame.text.lines();
    let head = text_lines.next().unwrap_or_default();

    let mut spans = vec![
        gutter_span(frame.at_ms, theme),
        Span::new("  ⎿ ", dimmer),
        Span::new(dot, dot_style),
        Span::new(" ", Style::default()),
    ];
    // Right-align the status past the head; drop it entirely when the row is
    // too narrow to hold both.
    let status_w = visible_width(status);
    if content_w > status_w + 2 {
        let head_clipped = truncate_to_width(head, content_w - status_w - 2, "");
        let pad = content_w - status_w - visible_width(&head_clipped);
        spans.push(Span::new(head_clipped, value));
        spans.push(Span::new(" ".repeat(pad), Style::default()));
        spans.push(Span::new(status, muted));
    } else {
        spans.push(Span::new(head, value));
    }
    let mut lines = vec![StyledLine(spans).render(ctx.width, ctx.truecolor)];

    for text in text_lines {
        let style = subtext_style(text, theme);
        for wrapped in wrap_text(text, content_w) {
            lines.push(
                StyledLine(vec![
                    Span::new(" ".repeat(GUTTER_W + TOOL_LEAD_W), Style::default()),
                    Span::new(wrapped, style),
                ])
                .render(ctx.width, ctx.truecolor),
            );
        }
    }
    lines
}

/// Faint for plain subtext; `diff_add`/`diff_del` on `diff_bg` for lines with
/// exactly one leading `+`/`-` (so `+++`/`---` file headers stay plain).
fn subtext_style(text: &str, theme: &Theme) -> Style {
    let diff_bg = Color::Rgb(theme.diff_bg);
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some('+'), second) if second != Some('+') => {
            Style::fg(Color::Rgb(theme.diff_add)).bg(diff_bg)
        }
        (Some('-'), second) if second != Some('-') => {
            Style::fg(Color::Rgb(theme.diff_del)).bg(diff_bg)
        }
        _ => Style::fg(Color::Rgb(theme.faint)),
    }
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

    fn tool(text: &str, status: ToolFrameStatus) -> TranscriptFrame {
        let mut frame = frame(FrameKind::Tool, text);
        frame.tool_id = Some("tool-1".to_owned());
        frame.tool_status = Some(status);
        frame
    }

    #[test]
    fn completed_tool_row_has_ok_dot_and_right_aligned_status() {
        let lines = transcript_frame_lines(
            &tool(
                "read crates/mycel-gate/src/config.rs\n231 lines",
                ToolFrameStatus::Completed,
            ),
            &Theme::amanita(),
            &ctx(),
        );
        let first = &lines[0];
        assert!(first.contains('⎿'));
        // ok #55a868 immediately styles the dot
        assert!(first.contains("38;2;85;168;104m●"));
        let stripped = strip_ansi(first);
        assert!(stripped.contains("gate allow · done"), "{stripped:?}");
        assert!(
            stripped.trim_end().ends_with("gate allow · done"),
            "status must be right-aligned: {stripped:?}"
        );
        // subtext renders faint #4a544a
        assert!(lines[1].contains("38;2;74;84;74"));
        assert_within_width(&lines, 100);
    }

    #[test]
    fn running_tool_row_spins_and_says_running() {
        let mut context = ctx();
        context.spinner_phase = 3;
        let lines = transcript_frame_lines(
            &tool("shell cargo test -p mycel-gate", ToolFrameStatus::Running),
            &Theme::amanita(),
            &context,
        );
        let stripped = strip_ansi(&lines[0]);
        assert!(stripped.contains('⢿'), "spinner frame 3: {stripped:?}");
        assert!(stripped.contains("gate allow · running"));
        assert_within_width(&lines, 100);
    }

    #[test]
    fn failed_tool_row_has_accent_dot_and_failed_status() {
        let lines = transcript_frame_lines(
            &tool("shell cargo test", ToolFrameStatus::Failed),
            &Theme::amanita(),
            &ctx(),
        );
        // accent #e05a1e styles the dot
        assert!(lines[0].contains("38;2;224;90;30m●"));
        let stripped = strip_ansi(&lines[0]);
        assert!(stripped.contains("failed"));
        assert!(!stripped.contains("gate allow"));
        assert_within_width(&lines, 100);
    }

    #[test]
    fn tool_diff_lines_style_add_and_del_on_the_diff_background() {
        let lines = transcript_frame_lines(
            &tool(
                "edit config.rs\n- const T: u64 = 500;\n+ const T: u64 = 800;",
                ToolFrameStatus::Completed,
            ),
            &Theme::amanita(),
            &ctx(),
        );
        let joined = lines.join("\n");
        // diff_del #7d8579 and diff_add #a8b0a3 foregrounds
        assert!(joined.contains("38;2;125;133;121"));
        assert!(joined.contains("38;2;168;176;163"));
        // diff_bg #0d0f0c background
        assert!(joined.contains("48;2;13;15;12"));
        assert_within_width(&lines, 100);
    }

    #[test]
    fn narrow_tool_row_drops_the_status_instead_of_overflowing() {
        let mut context = ctx();
        context.width = 24;
        let lines = transcript_frame_lines(
            &tool(
                "read crates/mycel-gate/src/config.rs",
                ToolFrameStatus::Completed,
            ),
            &Theme::amanita(),
            &context,
        );
        assert!(!strip_ansi(&lines[0]).contains("gate allow"));
        assert_within_width(&lines, 24);
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
