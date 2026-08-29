//! The collapsible substrate inspector (right) from `mycel-tui-mockup.html`.
//!
//! Open: `gate · last decision`, the timestamped `activity` log, the antibody
//! detail box for the last matched denial, and the `candidates` section.
//! Collapsed: a 3-cell strip with the last-verdict glyph, the candidate
//! count, a vertical label, and the expand chevron. Pure: renders an
//! `InspectorData` snapshot with a `Theme`; no I/O — the antibody lookup
//! happens at deny time in the interactive loop, never here.

use crate::terminal::style::{Color, Span, Style, StyledLine};
use crate::terminal::{visible_width, wrap_text};
use crate::tui::theme::Theme;
use crate::tui::{GateDecision, GateVerdict};

use super::fit_spans;
use super::transcript::gutter_text;

/// Substrate record for the antibody behind the last denial, snapshotted by
/// the loop when the deny's `(source: antibody:<id>)` pointer resolves.
///
/// The mockup also shows a `name`, a last-hit date, and a per-decision
/// three-step trace; none of those exist in today's data. `Antibody` has no
/// name field and tracks only a hit count (crates/mycel-core/src/lib.rs:73-85),
/// and the gate reports a single reason string per decision, not which floor
/// or db step fired (crates/mycel-gate/src/main.rs:205-214). They are omitted
/// rather than invented.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AntibodyDetail {
    pub id: String,
    pub source: String,
    pub scope: String,
    pub severity: String,
    pub confidence: String,
    pub refusal: String,
    pub hits: u32,
    /// Present signature fields as `(field, pattern)` rows.
    pub signature: Vec<(String, String)>,
    pub remediation: String,
}

/// The plain data the inspector renders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorData {
    /// Observed gate decisions, oldest first (the ring's order).
    pub activity: Vec<GateDecision>,
    pub antibody: Option<AntibodyDetail>,
    pub candidates_pending: u32,
}

/// Cells for the label column of the key/value grids.
const LABEL_W: usize = 11;
/// Most recent activity rows shown in the open inspector.
const ACTIVITY_ROWS: usize = 8;

/// Render the open inspector: exactly `height` lines, each at most `width`
/// cells.
pub fn inspector(
    data: &InspectorData,
    theme: &Theme,
    width: usize,
    height: usize,
    truecolor: bool,
) -> Vec<String> {
    let muted = Style::fg(Color::Rgb(theme.muted));
    let value = Style::fg(Color::Rgb(theme.value));
    let accent = Style::fg(Color::Rgb(theme.accent));
    let faint = Style::fg(Color::Rgb(theme.faint));
    let secondary = Style::fg(Color::Rgb(theme.secondary));

    let mut lines = vec![section_header("gate · last decision", theme, width)];
    match data.activity.last() {
        Some(decision) => {
            let verdict = match decision.verdict {
                GateVerdict::Deny => Span::new("DENY · fail-closed", accent.bold()),
                GateVerdict::Allow => Span::new("allow", muted),
            };
            lines.push(grid_row("verdict", verdict, theme));
            if !decision.tool.is_empty() {
                lines.push(grid_row(
                    "tool",
                    Span::new(decision.tool.clone(), value),
                    theme,
                ));
            }
            if !decision.target.is_empty() {
                lines.push(grid_row(
                    "target",
                    Span::new(decision.target.clone(), value),
                    theme,
                ));
            }
            if decision.verdict == GateVerdict::Deny {
                // Denies arrive as blocked PreToolUse hook results; that is
                // the whole hook fact the event carries (no exit code —
                // see tui/gate_log.rs).
                lines.push(grid_row(
                    "hook",
                    Span::new("PreToolUse · decision emitted".to_owned(), muted),
                    theme,
                ));
            }
        }
        None => lines.push(StyledLine(vec![Span::new("no decisions yet", muted)])),
    }
    lines.push(StyledLine::default());

    lines.push(section_header("activity", theme, width));
    if data.activity.is_empty() {
        lines.push(StyledLine(vec![Span::new("nothing observed yet", muted)]));
    }
    let skip = data.activity.len().saturating_sub(ACTIVITY_ROWS);
    for decision in data.activity.iter().skip(skip) {
        let (verdict_span, line_style) = match decision.verdict {
            GateVerdict::Deny => (Span::new("DENY ", accent), accent),
            GateVerdict::Allow => (Span::new("allow", muted), muted),
        };
        let what = match (decision.tool.is_empty(), decision.target.is_empty()) {
            (false, false) => format!("{} · {}", decision.tool, decision.target),
            (false, true) => decision.tool.clone(),
            _ => decision.detail.clone(),
        };
        lines.push(StyledLine(vec![
            Span::new(format!("{}  ", gutter_text(decision.at_ms)), faint),
            verdict_span,
            Span::new(format!("  {what}"), line_style),
        ]));
    }
    lines.push(StyledLine::default());

    if let Some(antibody) = &data.antibody {
        lines.extend(antibody_box(antibody, theme, width));
        lines.push(StyledLine::default());
    }

    lines.push(section_header("candidates", theme, width));
    lines.push(StyledLine(vec![
        Span::new(
            format!("{} pending", data.candidates_pending),
            if data.candidates_pending > 0 {
                accent
            } else {
                muted
            },
        ),
        Span::new(" · learned, not yet trusted", muted),
    ]));
    lines.push(StyledLine(vec![Span::new(
        "promotion is human-in-the-loop:",
        muted,
    )]));
    // The mockup's one-liner is 59 cells; at the 50-cell open width it splits
    // at its ` · ` boundary instead of clipping the tail.
    lines.push(StyledLine(vec![
        Span::new("/candidates", secondary),
        Span::new(" review → ", muted),
        Span::new("/promote", secondary),
        Span::new(" trust", muted),
    ]));
    lines.push(StyledLine(vec![Span::new("nothing auto-promotes", muted)]));

    lines.truncate(height);
    while lines.len() < height {
        lines.push(StyledLine::default());
    }
    lines
        .into_iter()
        .map(|line| line.render(width, truecolor))
        .collect()
}

/// Render the collapsed 3-cell strip: last-verdict glyph, candidate count,
/// vertical `inspector` label, expand chevron. Exactly `height` lines.
pub fn inspector_collapsed(
    data: &InspectorData,
    theme: &Theme,
    width: usize,
    height: usize,
    truecolor: bool,
) -> Vec<String> {
    let glyph_style = match data.activity.last().map(|decision| decision.verdict) {
        Some(GateVerdict::Deny) => Style::fg(Color::Rgb(theme.accent)),
        Some(GateVerdict::Allow) => Style::fg(Color::Rgb(theme.ok)),
        None => Style::fg(Color::Rgb(theme.muted)),
    };
    let candidate_style = if data.candidates_pending > 0 {
        Style::fg(Color::Rgb(theme.accent))
    } else {
        Style::fg(Color::Rgb(theme.muted))
    };
    let dimmer = Style::fg(Color::Rgb(theme.dimmer));
    let muted = Style::fg(Color::Rgb(theme.muted));

    let mut lines = vec![
        StyledLine(vec![Span::new(" ■", glyph_style)]),
        StyledLine(vec![Span::new(
            format!(" {}", data.candidates_pending),
            candidate_style,
        )]),
        StyledLine::default(),
    ];
    for character in "inspector".chars() {
        lines.push(StyledLine(vec![Span::new(format!(" {character}"), dimmer)]));
    }
    lines.push(StyledLine::default());
    lines.push(StyledLine(vec![Span::new(" ‹", muted)]));
    lines.truncate(height);
    while lines.len() < height {
        lines.push(StyledLine::default());
    }
    lines
        .into_iter()
        .map(|line| line.render(width, truecolor))
        .collect()
}

/// The antibody detail box: dashed `deny_border` rules and `╎` sides with
/// content on `deny_bg`, mirroring the transcript's deny framing.
fn antibody_box(antibody: &AntibodyDetail, theme: &Theme, width: usize) -> Vec<StyledLine> {
    let border = Style::fg(Color::Rgb(theme.deny_border));
    let deny_bg = Color::Rgb(theme.deny_bg);
    let accent = Style::fg(Color::Rgb(theme.accent)).bg(deny_bg);
    let muted = Style::fg(Color::Rgb(theme.muted)).bg(deny_bg);
    let value = Style::fg(Color::Rgb(theme.value)).bg(deny_bg);
    let secondary = Style::fg(Color::Rgb(theme.secondary)).bg(deny_bg);
    // `╎ ` + content + ` ╎`
    let inner_w = width.saturating_sub(4).max(1);

    let mut lines = vec![StyledLine(vec![Span::new("╌".repeat(width), border)])];
    let boxed = |content: Vec<Span>| {
        let used: usize = content.iter().map(|span| visible_width(&span.text)).sum();
        let mut spans = vec![Span::new("╎ ", border)];
        spans.extend(content);
        spans.push(Span::new(
            " ".repeat(inner_w.saturating_sub(used)),
            Style::default().bg(deny_bg),
        ));
        spans.push(Span::new(" ╎", border));
        StyledLine(spans)
    };

    lines.push(boxed(vec![Span::new(
        format!("antibody {}", antibody.id),
        accent,
    )]));
    for (label, text) in [
        ("source", &antibody.source),
        ("scope", &antibody.scope),
        ("severity", &antibody.severity),
        ("confidence", &antibody.confidence),
        ("refusal", &antibody.refusal),
    ] {
        let mut row = fit_spans(vec![Span::new(label, muted)], LABEL_W);
        row.push(Span::new(" ", Style::default().bg(deny_bg)));
        row.push(Span::new(text.clone(), value));
        lines.push(boxed(row));
    }
    let mut hits = fit_spans(vec![Span::new("hits", muted)], LABEL_W);
    hits.push(Span::new(" ", Style::default().bg(deny_bg)));
    hits.push(Span::new(antibody.hits.to_string(), value));
    lines.push(boxed(hits));

    if !antibody.signature.is_empty() {
        lines.push(boxed(vec![Span::new("signature", muted)]));
        for (field, pattern) in &antibody.signature {
            let mut row = vec![Span::new("  ", Style::default().bg(deny_bg))];
            row.extend(fit_spans(
                vec![Span::new(
                    field.clone(),
                    Style::fg(Color::Rgb(theme.faint)).bg(deny_bg),
                )],
                LABEL_W + 2,
            ));
            row.push(Span::new(" ", Style::default().bg(deny_bg)));
            row.push(Span::new(pattern.clone(), secondary));
            lines.push(boxed(row));
        }
    }
    if !antibody.remediation.is_empty() {
        lines.push(boxed(vec![Span::new("remediation", muted)]));
        for wrapped in wrap_text(&antibody.remediation, inner_w.saturating_sub(2)) {
            lines.push(boxed(vec![Span::new(
                format!("  {wrapped}"),
                Style::fg(Color::Rgb(theme.stem)).bg(deny_bg),
            )]));
        }
    }
    lines.push(StyledLine(vec![Span::new("╌".repeat(width), border)]));
    lines
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

fn grid_row(label: &str, value: Span, theme: &Theme) -> StyledLine {
    let mut spans = fit_spans(
        vec![Span::new(label, Style::fg(Color::Rgb(theme.muted)))],
        LABEL_W,
    );
    spans.push(Span::new(" ", Style::default()));
    spans.push(value);
    StyledLine(spans)
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

    fn decision(at_ms: u64, verdict: GateVerdict, tool: &str, target: &str) -> GateDecision {
        GateDecision {
            at_ms,
            verdict,
            tool: tool.to_owned(),
            target: target.to_owned(),
            detail: match verdict {
                GateVerdict::Deny => "denied (source: antibody:abcdef12)".to_owned(),
                GateVerdict::Allow => String::new(),
            },
        }
    }

    fn sample() -> InspectorData {
        InspectorData {
            activity: vec![
                decision(1_000, GateVerdict::Allow, "read", "config.rs"),
                decision(2_000, GateVerdict::Allow, "shell", "cargo test"),
                decision(3_000, GateVerdict::Deny, "write", "~/.mycel/config.toml"),
            ],
            antibody: Some(AntibodyDetail {
                id: "abcdef12".to_owned(),
                source: "manual".to_owned(),
                scope: "project".to_owned(),
                severity: "refuse".to_owned(),
                confidence: "solid".to_owned(),
                refusal: "hard".to_owned(),
                hits: 14,
                signature: vec![
                    ("tool_pattern".to_owned(), "write".to_owned()),
                    ("file_pattern".to_owned(), "~/.mycel/**".to_owned()),
                ],
                remediation: "stage the change in-repo".to_owned(),
            }),
            candidates_pending: 1,
        }
    }

    #[test]
    fn open_inspector_renders_last_decision_activity_antibody_and_candidates() {
        let width = 50;
        let height = 40;
        let lines = inspector(&sample(), &Theme::amanita(), width, height, true);
        assert_eq!(lines.len(), height);
        for line in &lines {
            assert!(visible_width(line) <= width, "line too wide: {line:?}");
        }
        let joined = lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "gate · last decision ╌",
            "DENY · fail-closed",
            "write",
            "~/.mycel/config.toml",
            "PreToolUse · decision emitted",
            "activity ╌",
            "allow",
            "read · config.rs",
            "shell · cargo test",
            "antibody abcdef12",
            "severity",
            "refuse",
            "tool_pattern",
            "~/.mycel/**",
            "remediation",
            "stage the change in-repo",
            "candidates ╌",
            "1 pending · learned, not yet trusted",
            "promotion is human-in-the-loop:",
            "/candidates review → /promote trust",
            "nothing auto-promotes",
        ] {
            assert!(joined.contains(needle), "missing {needle:?} in {joined}");
        }
        // Deny rows carry the accent, the antibody box its dashed frame.
        let colored = lines.join("\n");
        assert!(colored.contains("38;2;224;90;30"));
        assert!(joined.contains("╎ "));
    }

    #[test]
    fn empty_inspector_degrades_without_inventing_data() {
        let data = InspectorData::default();
        let lines = inspector(&data, &Theme::amanita(), 50, 30, true);
        let joined = lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("no decisions yet"));
        assert!(joined.contains("nothing observed yet"));
        assert!(joined.contains("0 pending"));
        assert!(!joined.contains("antibody "), "no box without a match");
    }

    #[test]
    fn unattributed_deny_falls_back_to_its_reason_text() {
        let data = InspectorData {
            activity: vec![GateDecision {
                at_ms: 1_000,
                verdict: GateVerdict::Deny,
                tool: String::new(),
                target: String::new(),
                detail: "refusing a write-class tool call".to_owned(),
            }],
            antibody: None,
            candidates_pending: 0,
        };
        let joined = inspector(&data, &Theme::amanita(), 50, 30, true)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("DENY · fail-closed"));
        assert!(joined.contains("refusing a write-class tool call"));
    }

    #[test]
    fn collapsed_inspector_stacks_glyph_count_label_and_chevron() {
        let width = 3;
        let height = 20;
        let lines = inspector_collapsed(&sample(), &Theme::amanita(), width, height, true);
        assert_eq!(lines.len(), height);
        for line in &lines {
            assert!(visible_width(line) <= width, "line too wide: {line:?}");
        }
        let stripped: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
        assert_eq!(stripped[0].trim(), "■");
        assert_eq!(stripped[1].trim(), "1");
        let column: String = stripped
            .iter()
            .map(|line| line.trim().to_owned())
            .collect::<Vec<_>>()
            .join("");
        assert!(column.contains("inspector"));
        assert!(column.ends_with('‹'));
        // Last verdict was a deny: the glyph carries the accent.
        assert!(lines[0].contains("38;2;224;90;30m"));
    }

    #[test]
    fn short_heights_clip_without_panicking() {
        for height in [0usize, 1, 5, 12] {
            assert_eq!(
                inspector(&sample(), &Theme::amanita(), 50, height, true).len(),
                height
            );
            assert_eq!(
                inspector_collapsed(&sample(), &Theme::amanita(), 3, height, true).len(),
                height
            );
        }
    }
}
