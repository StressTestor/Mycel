//! The collapsible session rail (left) from `mycel-tui-mockup.html`.
//!
//! Open: `session`, `substrate`, `ecology`, and `hyphae` sections with
//! secondary headers and trailing dashed rules, and the promotion footer
//! pinned to the bottom row. Collapsed: a 3-cell strip of status glyphs, a
//! vertical label, and the expand chevron. Pure: renders a `RailData`
//! snapshot with a `Theme`; no I/O.

use crate::ecology::{GateStatus, SubstrateStatus};
use crate::terminal::style::{Color, Span, Style, StyledLine};
use crate::terminal::visible_width;
use crate::tui::theme::Theme;

use super::fit_spans;

/// The plain data the rail renders, snapshotted by the interactive loop.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RailData {
    /// Session title when one is set, else the short session id.
    pub name: String,
    pub model: String,
    pub provider: String,
    pub cwd: String,
    /// The editor's input mode: shell (`!`) or prompt.
    pub shell_mode: bool,
    pub plan: bool,
    /// Context occupancy is not derivable from the loop's event stream (see
    /// `build_header`); `None` renders the window alone, never a made-up fill.
    pub ctx_used: Option<u64>,
    pub ctx_window: u64,
    pub substrate: SubstrateStatus,
    /// Live subagent count and the most recent subagent line, both derived
    /// from the transcript's Subagent frames — the only hyphae state the loop
    /// already holds without an async orchestration read.
    pub hyphae_active: usize,
    pub hyphae_last: Option<String>,
}

/// Cells for the label column of the key/value grids (fits `candidates`).
const LABEL_W: usize = 11;
/// The ecology palette, rendered as the mockup's two-column grid.
const ECOLOGY_COMMANDS: [(&str, &str); 4] = [
    ("/immunity", "/gate"),
    ("/substrate", "/candidates"),
    ("/promote", "/deny"),
    ("/delegate", "/hyphae"),
];

/// Render the open rail: exactly `height` lines, each at most `width` cells.
/// The footer sits on the bottom rows; content beyond the height clips from
/// the bottom (the sections are ordered by importance).
pub fn session_rail(
    data: &RailData,
    theme: &Theme,
    width: usize,
    height: usize,
    truecolor: bool,
) -> Vec<String> {
    let mut lines = vec![section_header("session", theme, width)];
    lines.push(grid_row("name", &data.name, theme));
    lines.push(grid_row("model", &data.model, theme));
    lines.push(grid_row("provider", &data.provider, theme));
    lines.push(grid_row("cwd", &data.cwd, theme));
    let secondary = Style::fg(Color::Rgb(theme.secondary));
    lines.push(grid_row_styled(
        "mode",
        Span::new(
            format!(
                "{} · plan {}",
                if data.shell_mode { "shell" } else { "prompt" },
                if data.plan { "on" } else { "off" }
            ),
            secondary,
        ),
        theme,
    ));
    lines.push(grid_row_styled(
        "ctx",
        Span::new(
            format!(
                "{} / {}",
                data.ctx_used.map_or("-".to_owned(), format_tokens),
                format_tokens(data.ctx_window)
            ),
            secondary,
        ),
        theme,
    ));
    lines.push(StyledLine::default());

    lines.push(section_header("substrate", theme, width));
    lines.extend(substrate_rows(&data.substrate, theme));
    lines.push(StyledLine::default());

    lines.push(section_header("ecology", theme, width));
    for (left, right) in ECOLOGY_COMMANDS {
        let style = |command: &str| {
            // The pending-review entry point lights up when there is
            // something to review, matching the mockup's highlighted
            // `/candidates`.
            if command == "/candidates" && data.substrate.candidates_pending > 0 {
                Style::fg(Color::Rgb(theme.value))
            } else {
                secondary
            }
        };
        let pad = " ".repeat(LABEL_W.saturating_sub(visible_width(left)) + 1);
        lines.push(StyledLine(vec![
            Span::new(left, style(left)),
            Span::new(pad, Style::default()),
            Span::new(right, style(right)),
        ]));
    }
    let faint = Style::fg(Color::Rgb(theme.faint));
    lines.push(StyledLine(vec![Span::new("/ for the full palette", faint)]));
    lines.push(StyledLine::default());

    lines.push(section_header("hyphae", theme, width));
    lines.push(grid_row("active", &data.hyphae_active.to_string(), theme));
    if let Some(last) = &data.hyphae_last {
        lines.push(grid_row("last", last, theme));
    }
    lines.push(StyledLine(vec![Span::new(
        "/delegate spawns scoped sub-agents",
        faint,
    )]));

    // Pin the two footer lines to the bottom of the rail when there is room.
    let dimmer = Style::fg(Color::Rgb(theme.dimmer));
    let footer = [
        StyledLine(vec![Span::new("promotion is manual.", dimmer)]),
        StyledLine(vec![Span::new("nothing auto-promotes.", dimmer)]),
    ];
    while lines.len() + footer.len() < height {
        lines.push(StyledLine::default());
    }
    lines.extend(footer);
    lines.truncate(height);
    lines
        .into_iter()
        .map(|line| line.render(width, truecolor))
        .collect()
}

/// Render the collapsed 3-cell strip: gate dot, pending-candidate count,
/// hyphae count, the vertical `session · substrate` label, and the expand
/// chevron. Exactly `height` lines.
pub fn session_rail_collapsed(
    data: &RailData,
    theme: &Theme,
    width: usize,
    height: usize,
    truecolor: bool,
) -> Vec<String> {
    let (dot_style, _) = gate_dot(data.substrate.gate, theme);
    let candidate_style = if data.substrate.candidates_pending > 0 {
        Style::fg(Color::Rgb(theme.accent))
    } else {
        Style::fg(Color::Rgb(theme.muted))
    };
    let dimmer = Style::fg(Color::Rgb(theme.dimmer));
    let muted = Style::fg(Color::Rgb(theme.muted));

    let mut lines = vec![
        StyledLine(vec![Span::new(" ●", dot_style)]),
        StyledLine(vec![Span::new(
            format!(" {}", data.substrate.candidates_pending),
            candidate_style,
        )]),
        StyledLine(vec![Span::new(format!(" {}", data.hyphae_active), dimmer)]),
        StyledLine::default(),
    ];
    for character in "session · substrate".chars().filter(|c| *c != ' ') {
        lines.push(StyledLine(vec![Span::new(format!(" {character}"), dimmer)]));
    }
    lines.push(StyledLine::default());
    lines.push(StyledLine(vec![Span::new(" ›", muted)]));
    lines.truncate(height);
    while lines.len() < height {
        lines.push(StyledLine::default());
    }
    lines
        .into_iter()
        .map(|line| line.render(width, truecolor))
        .collect()
}

/// The substrate key/value rows shared with the mockup: antibody count,
/// candidate count (accent when pending), the gate state line, and the hook
/// wiring row. The mockup's `hook … 3ms`, `mcp`, and `decay` rows carry data
/// today's pipeline does not: hook executions have no duration field
/// (crates/mycel-agent-runtime/src/hooks.rs `HookExecution`), no substrate MCP
/// server is part of the CLI's own wiring, and the decay pass lives behind a
/// full audit-log scan (`last_maintenance`, crates/mycel-cli/src/ecology.rs).
/// They are omitted rather than invented; the hook row keeps only the wiring
/// fact the gate status already proves.
fn substrate_rows(substrate: &SubstrateStatus, theme: &Theme) -> Vec<StyledLine> {
    let accent = Style::fg(Color::Rgb(theme.accent));
    let muted = Style::fg(Color::Rgb(theme.muted));
    let value = Style::fg(Color::Rgb(theme.value));
    let mut rows = vec![
        grid_row(
            "antibodies",
            &format!("{} active", substrate.antibodies_active),
            theme,
        ),
        grid_row_styled(
            "candidates",
            Span::new(
                format!("{} pending", substrate.candidates_pending),
                if substrate.candidates_pending > 0 {
                    accent
                } else {
                    muted
                },
            ),
            theme,
        ),
    ];
    let (dot_style, state) = gate_dot(substrate.gate, theme);
    rows.push(grid_row_spans(
        "gate",
        vec![
            Span::new("●", dot_style),
            Span::new(format!(" {state}"), value),
        ],
        theme,
    ));
    if matches!(substrate.gate, GateStatus::Ok | GateStatus::Tripwire) {
        // Wired fail-closed is exactly what Ok/Tripwire mean (see
        // `EcologyService::summary`); other states cannot honestly claim a
        // PreToolUse hook, so the row is dropped.
        rows.push(grid_row_styled(
            "hook",
            Span::new("PreToolUse".to_owned(), muted),
            theme,
        ));
    }
    rows
}

/// The gate dot style and state text for a `GateStatus`.
fn gate_dot(gate: GateStatus, theme: &Theme) -> (Style, &'static str) {
    match gate {
        GateStatus::Ok => (Style::fg(Color::Rgb(theme.ok)), "fail-closed · ok"),
        GateStatus::Tripwire => (Style::fg(Color::Rgb(theme.accent)), "fail-closed · blocked"),
        GateStatus::Disarmed => (Style::fg(Color::Rgb(theme.accent)), "disarmed"),
        GateStatus::Unknown => (Style::fg(Color::Rgb(theme.muted)), "unknown"),
    }
}

/// `name ╌╌╌…` — secondary section name with a trailing dashed rule.
fn section_header(name: &str, theme: &Theme, width: usize) -> StyledLine {
    let rule_w = width.saturating_sub(visible_width(name) + 1);
    StyledLine(vec![
        Span::new(name, Style::fg(Color::Rgb(theme.secondary))),
        Span::new(" ", Style::default()),
        Span::new("╌".repeat(rule_w), Style::fg(Color::Rgb(theme.border))),
    ])
}

fn grid_row(label: &str, value: &str, theme: &Theme) -> StyledLine {
    grid_row_styled(
        label,
        Span::new(value.to_owned(), Style::fg(Color::Rgb(theme.value))),
        theme,
    )
}

fn grid_row_styled(label: &str, value: Span, theme: &Theme) -> StyledLine {
    grid_row_spans(label, vec![value], theme)
}

fn grid_row_spans(label: &str, value: Vec<Span>, theme: &Theme) -> StyledLine {
    let mut spans = fit_spans(
        vec![Span::new(label, Style::fg(Color::Rgb(theme.muted)))],
        LABEL_W,
    );
    spans.push(Span::new(" ", Style::default()));
    spans.extend(value);
    StyledLine(spans)
}

/// Compact token count: whole `k` above 100k (`200k`), one decimal below
/// (`41.2k`), plain digits under 1k.
fn format_tokens(tokens: u64) -> String {
    if tokens >= 100_000 {
        format!("{}k", tokens / 1000)
    } else if tokens >= 1000 {
        let tenths = tokens / 100;
        format!("{}.{}k", tenths / 10, tenths % 10)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn sample() -> RailData {
        RailData {
            name: "cordyceps-patch".to_owned(),
            model: "claude-sonnet-4.6".to_owned(),
            provider: "anthropic".to_owned(),
            cwd: "~/dev/mycoforge".to_owned(),
            shell_mode: false,
            plan: false,
            ctx_used: None,
            ctx_window: 200_000,
            substrate: SubstrateStatus {
                antibodies_active: 23,
                candidates_pending: 1,
                gate: GateStatus::Ok,
            },
            hyphae_active: 0,
            hyphae_last: Some("test-runner · exited".to_owned()),
        }
    }

    #[test]
    fn open_rail_renders_all_sections_with_live_values() {
        let width = 38;
        let height = 40;
        let lines = session_rail(&sample(), &Theme::amanita(), width, height, true);
        assert_eq!(lines.len(), height);
        let stripped: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
        let joined = stripped.join("\n");
        for needle in [
            "session ╌",
            "cordyceps-patch",
            "claude-sonnet-4.6",
            "anthropic",
            "~/dev/mycoforge",
            "prompt · plan off",
            "- / 200k",
            "substrate ╌",
            "23 active",
            "1 pending",
            "● fail-closed · ok",
            "PreToolUse",
            "ecology ╌",
            "/immunity",
            "/candidates",
            "/hyphae",
            "/ for the full palette",
            "hyphae ╌",
            "test-runner · exited",
            "/delegate spawns scoped sub-agents",
        ] {
            assert!(joined.contains(needle), "missing {needle:?} in {joined}");
        }
        // Footer pinned to the bottom rows.
        assert!(stripped[height - 2].starts_with("promotion is manual."));
        assert!(stripped[height - 1].starts_with("nothing auto-promotes."));
        for line in &lines {
            assert!(visible_width(line) <= width, "line too wide: {line:?}");
        }
    }

    #[test]
    fn open_rail_colors_pending_candidates_accent_and_ok_gate_green() {
        let joined = session_rail(&sample(), &Theme::amanita(), 38, 40, true).join("\n");
        // amanita accent #e05a1e on the pending count, ok #55a868 on the dot.
        assert!(joined.contains("38;2;224;90;30m1 pending"));
        assert!(joined.contains("38;2;85;168;104m●"));
    }

    #[test]
    fn degraded_gate_states_render_their_words_and_drop_the_hook_row() {
        let mut data = sample();
        data.substrate.gate = GateStatus::Disarmed;
        let disarmed = session_rail(&data, &Theme::amanita(), 38, 40, true)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(disarmed.contains("● disarmed"));
        assert!(
            !disarmed.contains("PreToolUse"),
            "a disarmed gate cannot claim a wired hook"
        );
        data.substrate.gate = GateStatus::Tripwire;
        let tripwire = session_rail(&data, &Theme::amanita(), 38, 40, true)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(tripwire.contains("● fail-closed · blocked"));
        assert!(tripwire.contains("PreToolUse"));
    }

    #[test]
    fn collapsed_rail_stacks_glyphs_label_and_chevron() {
        let width = 3;
        let height = 30;
        let lines = session_rail_collapsed(&sample(), &Theme::amanita(), width, height, true);
        assert_eq!(lines.len(), height);
        for line in &lines {
            assert!(visible_width(line) <= width, "line too wide: {line:?}");
        }
        let stripped: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
        assert_eq!(stripped[0].trim(), "●");
        assert_eq!(stripped[1].trim(), "1");
        assert_eq!(stripped[2].trim(), "0");
        let column: String = stripped
            .iter()
            .map(|line| line.trim().to_owned())
            .collect::<Vec<_>>()
            .join("");
        assert!(column.contains("session·substrate"));
        assert!(column.ends_with('›'));
        // The gate dot keeps its live color in the strip.
        assert!(lines[0].contains("38;2;85;168;104m"));
    }

    #[test]
    fn short_heights_clip_without_panicking() {
        for height in [0usize, 1, 5, 10, 20] {
            let open = session_rail(&sample(), &Theme::amanita(), 38, height, true);
            assert_eq!(open.len(), height);
            let collapsed = session_rail_collapsed(&sample(), &Theme::amanita(), 3, height, true);
            assert_eq!(collapsed.len(), height);
        }
    }

    #[test]
    fn token_counts_format_compactly() {
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(41_200), "41.2k");
        assert_eq!(format_tokens(999), "999");
    }
}
