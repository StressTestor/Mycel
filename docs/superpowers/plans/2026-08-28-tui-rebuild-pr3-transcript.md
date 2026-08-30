# TUI Rebuild — PR3: Rich Transcript Frames — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or superpowers:executing-plans, task-by-task, TDD. Checkbox (`- [ ]`) steps.

**Goal:** Replace the flat `label: text` transcript rendering with the mockup's rich frames — timestamp gutter, per-kind markers, tool rows with status dot / spinner / right-aligned gate status, diff blocks, the gate DENY box, italic thinking — all themed.

**Architecture:** A new pure component `tui/components/transcript.rs` renders one `TranscriptFrame` to styled lines; `production.rs::frame_lines` delegates to it. `TranscriptFrame` gains a timestamp (the reducer already receives `now_ms` on every push). No layout-engine changes; frames stay full-width lines in the existing scroll model.

**Tech Stack:** Rust, workspace deps only (chrono is already a mycel-cli dep for time formatting). Zero new dependencies (ADR-0021).

**Spec:** `docs/design/tui-implementation-spec.md` §5.4 (transcript). Visual reference: `docs/design/mycel-tui-mockup.html` transcript section. Landed foundation: `terminal/style.rs`, `tui/theme.rs`, `tui_config::active_theme`, `terminal/style::truecolor_enabled`, PR2's `tui/components/` pattern (pure `(&data, &Theme, width, truecolor) -> Vec<String>`, `fit_spans`).

## Global Constraints

- Zero new dependencies; no Cargo.toml changes.
- Iterate with `cargo test -p mycel-cli`; do NOT run `cargo test --workspace` (30-min first compile — main loop runs it). Final gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p mycel-cli`, all exit 0 with evidence.
- Conventional commits, no AI attribution. Plain professional comments.
- Colors only via `Theme` roles. Golden-line tests assert text content + the role's SGR presence, per frame kind.
- Every rendered line ≤ width visible cells (use `StyledLine::render`).
- The existing reducer behavior (coalescing, streaming, tick/finish) must not change semantically — additive field only.

## Reference: frame anatomy (from the mockup)

Every frame row: `HH:MM:SS` gutter (theme.faint, fixed 10 cells: 8 time + 2 gap) + marker + content. Wrapped continuation lines indent past the gutter with blank gutter space.

| kind | marker | content style |
|---|---|---|
| User | `❯` (secondary) | bright text |
| Thinking | `∴` (dimmer, italic content in stem-gray `dim`) | italic, muted |
| Assistant | `·` (muted) | value text |
| Tool | `  ⎿` tree glyph (dimmer) + status dot | see tool rows below |
| Status/Hook/Mcp/Compaction/BackgroundTask/Goal/Subagent | `·` (muted) | muted text (Goal/Subagent content in `prompt` color) |

Tool rows: dot is `●` ok-green when Completed, accent when Failed, braille spinner frame (`⣾⣽⣻⢿⡿⣟⣯⣷`, indexed by a `spinner_phase: usize` param) when Running. First line: `<first-line-of-text>` (value for the verb-ish head, secondary for a path-like tail if the text splits on first space — keep this simple: render the whole first line in `value`) with right-aligned status text (`muted`): `gate allow · done` / `gate allow · running` / `failed` derived from `tool_status` (`Completed`→`done`, `Running`→`running`, `Failed`→`failed`; prefix `gate allow · ` only when the frame is NOT a deny — see below). Subsequent text lines: `faint`.

Diff lines: inside any Tool/Hook frame, a text line starting with `+` renders `diff_add` on `diff_bg`, starting with `-` renders `diff_del` on `diff_bg` (only when the line is not `+++`/`---`... keep it simple: exactly one leading `+`/`-` followed by a space or content).

DENY box: a frame that represents a gate denial renders inside a dashed box: top/bottom rules and `╎` sides in `deny_border`, content on `deny_bg`, first line lead `■` in accent, a `DENY` badge (accent background, `deny_bg`-dark foreground, bold) where the text contains it, remaining lines `secondary`. **Detection must key off real data:** grep how Hook frames are pushed (`TranscriptEvent` hook paths in `transcript.rs` + where production.rs emits hook/gate text) and determine what a gate deny actually looks like in `frame.text`/`frame.state` today (e.g. text containing a deny marker from the gate hook output). Implement detection from that evidence and document it in a comment. If today's deny text is a plain string with no structure, match on the real substring the gate emits (cite file:line in the comment). Do NOT invent a data shape; PR4 adds the structured gate-decision ring for the inspector.

## Task 1: Timestamp on TranscriptFrame

**Files:** Modify `crates/mycel-cli/src/tui/transcript.rs`. Tests inline (extend existing reducer tests).

**Interfaces produced:** `TranscriptFrame.at_ms: u64` — the `now_ms` the frame was first created at (coalesced/streamed appends keep the original `at_ms`).

- [ ] Failing test: push a `UserMessage` at `now_ms = 9_000_000`; assert `frames()[0].at_ms == 9_000_000`. Push a streaming assistant delta at t1 then more at t2; assert the frame keeps t1.
- [ ] Run → fails (field missing). Implement: add the field, set at frame creation everywhere a `TranscriptFrame` is constructed; update every existing test literal (compiler drives this).
- [ ] Run `cargo test -p mycel-cli` → passes. Commit `feat(tui): stamp transcript frames with creation time`.

## Task 2: Transcript frame component — non-tool kinds

**Files:** Create `crates/mycel-cli/src/tui/components/transcript.rs`; register in `components/mod.rs`. Tests inline.

**Interfaces produced:**
- `pub struct FrameCtx { pub width: usize, pub truecolor: bool, pub spinner_phase: usize }`
- `pub fn transcript_frame_lines(frame: &TranscriptFrame, theme: &Theme, ctx: &FrameCtx) -> Vec<String>`
- Gutter: format `at_ms` as local wall-clock `HH:MM:SS` via chrono (`Local.timestamp_millis_opt`), rendered in `theme.faint`; continuation lines get 10 blank cells.

- [ ] Failing golden tests (amanita, width 100): User frame contains `❯`, the text, and the secondary SGR; Thinking contains `∴` and italic SGR (`\x1b[3;` or `;3;` code present); Assistant contains `·` and value SGR; Status renders muted; a long text wraps and the second line starts with 10 spaces after any SGR prefix (assert via a stripped-ANSI helper in the test). All lines ≤ width.
- [ ] Run → fails. Implement. Run → passes. Commit `feat(tui): rich transcript frames for user/thinking/assistant/status`.

## Task 3: Tool rows — dot, spinner, right-aligned status, diffs

**Files:** Modify `components/transcript.rs`. Tests inline.

- [ ] Failing tests: Completed tool frame shows `⎿`, ok-SGR `●`, right-aligned `gate allow · done` (assert the status text appears and the line's visible width ≤ width); Running frame with `spinner_phase: 3` shows `⢿` and `running`; Failed shows accent dot and `failed`; a tool frame whose text has `- old` / `+ new` lines renders them with diff_del/diff_add SGR and diff_bg SGR (48;2 code present).
- [ ] Run → fails. Implement: split `frame.text` into lines; first line = head + right-aligned status via width math (pad between head and status; drop status when width too small); remaining = faint or diff-styled.
- [ ] Run → passes. Commit `feat(tui): tool rows with status dot, spinner, and diff styling`.

## Task 4: The gate DENY box

**Files:** Modify `components/transcript.rs`. Tests inline.

- [ ] FIRST: investigate how a gate denial reaches the transcript today. Grep `transcript.rs` `TranscriptEvent` hook variants + production.rs hook/gate emission (search `hook`, `deny`, `DENY`, `refus`, `gate`). Write down (in a comment on the detection fn) the exact shape with file:line citations.
- [ ] Failing test: construct a frame matching that real shape; assert the output contains the deny_border SGR, deny_bg SGR, `■`, and a `DENY` badge with accent-background SGR; box lines ≤ width.
- [ ] Implement `is_gate_deny(frame) -> bool` + the boxed rendering. Non-deny hook frames keep the plain muted row.
- [ ] Run → passes. Commit `feat(tui): gate denial frames render as the deny box`.

## Task 5: Wire in — replace the flat renderer

**Files:** Modify `crates/mycel-cli/src/production.rs`. Tests: update existing.

- [ ] In `interactive_view`, replace `frame_lines(frame, width, resolved_theme(...))` calls with `transcript_frame_lines(frame, &active_theme(&state.tui_config.theme), &FrameCtx { width, truecolor: truecolor_enabled(), spinner_phase })` — spinner_phase from a new counter on `InteractiveLoopState` advanced in the existing tick/poll path (find where the loop already ticks, cite it; if no natural tick advances during Running tool frames, advance the phase in `render_interactive` per call — deterministic enough for the 25ms poll).
- [ ] Delete the old `frame_lines` + `resolved_theme` + `ResolvedTheme` if now unused (compiler/clippy will say; `ThemeName::Light/Auto` handling lives in `active_theme` already). Update the tests that referenced them (e.g. `builtin_themes_color_frames_without_changing_terminal_width`) to go through the new component.
- [ ] Run `cargo test -p mycel-cli`, `clippy`, `fmt` → all green. Commit `feat(tui): interactive transcript uses the themed frame renderer`.

## Self-check before reporting

- No Cargo.toml/lock changes; all three local gates exit 0 (paste summary lines).
- Report: commits, the deny-detection evidence (what shape you found, file:line), spinner wiring choice, any existing tests you had to update and why, deviations. Stuck >2 cycles on a gate → stop and report.
