# TUI Rebuild — PR5: Input Box, Status Bar, Notification, Flourish — Implementation Plan

> **For agentic workers:** execute task-by-task, TDD. Checkbox steps. This is the final build PR of the series.

**Goal:** The remaining chrome from the mockup — the drawn input box with its inline status strip, the bottom status bar, the candidate notification strip — plus the optional ~1s startup flourish (which finally consumes `Theme.tag`/`glow`), count pluralization, and the ARCHITECTURE.md sync for PR4+PR5.

**Spec:** `docs/design/tui-implementation-spec.md` §5.2 items 2 and 5, §5.4 status bar, §6/§9 flourish. Visual reference `docs/design/mycel-tui-mockup.html` (input box + status bar + notification markup — exact glyphs `+╌╌`, `╎`, `▸▸`, `▮▯`, `❯`).

**Foundation:** everything through PR4 — components pattern, `compose.rs`, cached theme/truecolor, `SubstrateStatus` live summary, `GateLog`, `interactive_center_view`/body-band, `tui_config` rails persistence, `util::short_id`.

## Global Constraints

Same as PR4: zero deps; iterate `cargo test -p mycel-cli` only; final gates fmt/clippy(workspace, -D warnings)/test all 0; conventional commits, no AI attribution, plain comments; pure components; real data only with cited degradation; no I/O on the render tick; design copy frozen EXCEPT Task 4's pluralization (grammar agreement is a bug fix, not a copy change); `interactive_pty` flake protocol applies.

## Task 1: Notification strip

- `tui/components/notification.rs`: `▲ N candidate(s) pending review · run /candidates` (accent ▲, accent_dim text per mockup), rendered between the header card and the transcript in `interactive_center_view` when `candidates_pending > 0`; refreshes with the existing substrate-summary refresh (no new reads).
- TDD: golden test (pending > 0 renders, 0 renders nothing, width-safe). Commit `feat(tui): candidate notification strip`.

## Task 2: Drawn input box

- `tui/components/input_box.rs` replacing the bare `> ` prompt lines in `interactive_center_view`:
  - Top rule: `+╌╌ mycel ❯ [M] <model> ╌ [gate] <state> ╌ [N running] ╌ <cwd> ╌╌╌ ... ╌+` — segments from live state (model alias, gate state word from `SubstrateStatus`, running = active turn/hyphae count from what the loop already tracks — cite the source; drop segments right-to-left when width is short). The mockup's `ctx ▮▮▯▯ 21%` meter is OMITTED: context occupancy is not derivable (PR4 evidence, production.rs build_header comment) — cite that comment.
  - Input line: `╎ ❯ <buffer>▌ ╎` with the existing editor buffer/cursor; shell mode renders `!` instead of `❯` (preserve the existing InputMode distinction); the ghost hint (`— <hint>` in faint) only when the buffer is empty and a hint exists (mockup shows a slash-command hint; wire to whatever hint source exists today, else omit — do not invent a hint engine).
  - Bottom rule `+╌ ... ╌+`.
  - CURSOR: the cursor row/column math in `interactive_center_view` must land inside the drawn box (offset by the `╎ ❯ ` lead and the box's rows); extend the existing cursor tests. Multi-line input wraps inside the box.
- TDD: golden tests (segments, degradation, shell mode, cursor position). Commit `feat(tui): drawn input box with inline status strip`.

## Task 3: Status bar

- `tui/components/status_bar.rs`: full-width bottom line — left `▸▸ gate fail-closed <state>` + keybind hints (`enter send · esc cancel · ctrl+j newline · / commands · ctrl+c quit` — verify each against the real reducer bindings and render only true ones; add `ctrl+l rail · ctrl+r inspector`), right `<model> · N antibodies · N candidate(s) · /candidates`. Reserved as the last row of the body band (compose height - 1); drops segments right-to-left when narrow.
- TDD golden tests. Commit `feat(tui): bottom status bar`.

## Task 4: Count pluralization

- One helper (e.g. `util::count_noun(n, "candidate", "candidates")`); apply to candidate/antibody/session counts across header, rails, inspector, notification, status bar. "0 candidates", "1 candidate", "51 candidates".
- TDD: unit + update goldens. Commit `fix(tui): pluralize live counts`.

## Task 5: Startup flourish (flagged, default off)

- `tui.toml` `[startup] flourish = false` (extend TuiConfig + doctor). When true: before the first differential paint, a ~1s sequence on the alternate screen — mushroom logo rows appear bottom-up (~120ms/row using the existing logo component), then `mycel` + the theme's `tag` line (finally consuming `Theme.tag`), then the real gate line from `SubstrateStatus` (`● gate fail-closed ok · N antibodies · N candidates` — live values, not the video's sample copy), then clear to the normal view. `glow: true` themes render the wordmark bold+bright (spec §4's approximation). Implementation: a pre-loop render pass writing frames via the existing terminal session + a `std::thread::sleep` cadence is acceptable (startup only, before the event loop); ctrl+c during flourish must still exit cleanly (the signal handling already installed — verify and cite).
- TDD: frame-content golden tests for the sequence steps (pure fn producing the frame lines; the timing loop stays thin). Commit `feat(tui): optional startup flourish`.

## Task 6: Docs

- ARCHITECTURE.md: add PR4's modules (`tui/gate_log.rs`, `components/session_rail.rs`, `components/inspector.rs`) and PR5's (`notification.rs`, `input_box.rs`, `status_bar.rs`), the rails keybinds/persistence, and the flourish flag. Update the `/settings` output if it enumerates config (grep). Commit `docs: sync ARCHITECTURE.md with rails and chrome`.

## Report

Per-task status; the hint-source and running-count sources found (file:line); cursor-test evidence; flourish flag default confirmed off; gate summary lines; commits; anything missed. Stuck >2 cycles → stop and report.
