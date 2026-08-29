# TUI Rebuild — PR4: Collapsible Rails + Live Substrate Data — Implementation Plan

> **For agentic workers:** execute task-by-task, TDD. Checkbox steps. This is the largest PR of the series; do not start a task until the previous one's gates are green.

**Goal:** The session rail (left) and substrate inspector (right), collapsible and persisted, wired to live substrate/gate data — and the header's substrate line goes live, activating the ok/blocked gate dot that the cleanup deliberately left Unknown.

**Architecture:** Two new pure components (`session_rail`, `inspector`) render from snapshot structs. `interactive_view` restructures into a body band composed via `compose::{Region, join_row/assemble}` — the module PR1 built and the review flagged as dead; **this PR is its consumer-or-delete moment** (see Task 5). Live data: a cheap `EcologyService` summary read at construction + refresh on ecology-affecting events, and a bounded gate-decision ring fed from the existing hook/tool event projection.

**Spec:** `docs/design/tui-implementation-spec.md` §5.1 (session rail), §5.3 (inspector), §7 (interaction), §8 (data wiring). Visual reference: `docs/design/mycel-tui-mockup.html` (both rails, open and collapsed states — read it for exact sections, copy, glyphs, and column widths: rail 300px→~37 cells scaled? NO: use the mockup's CELL PROPORTIONS — rail 300/1680 of a 13px grid ≈ 34 cells open, 3 cells collapsed strip is 26px≈3 cells? The mockup is pixel-based; translate: open rail ≈ 34 cols, collapsed ≈ 3 cols, inspector open ≈ 50 cols, collapsed ≈ 3 cols. Round to taste and record the chosen constants in one place.)

**Foundation:** `terminal/compose.rs` (Region/join_row/assemble + clip_and_pad), `tui/theme.rs`, `tui/components/` pattern, `tui_config::active_theme`, cleanup's cached `theme`/`truecolor`/`header_cache` on `InteractiveLoopState`, `GateDisplay {Ok, Blocked, Unknown}` in header.rs, `util::short_id`.

## Global Constraints

- Zero new dependencies; no Cargo.toml changes.
- Iterate with `cargo test -p mycel-cli` only (`--workspace` is the main loop's job; `mycel-agent-runtime` tests need `--lib`). Final gates all exit 0: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p mycel-cli`.
- Conventional commits, no AI attribution, plain professional comments. Design copy frozen (use the mockup's exact strings).
- Components stay pure (`snapshot + Theme + ctx → Vec<StyledLine>` or `Vec<String>`); every line ≤ width.
- **Real data only.** Where the mockup shows a field today's pipeline doesn't carry (e.g. antibody id on a deny), investigate what the gate/hook payload actually contains, cite file:line in a comment, and render what exists — degrade the missing field honestly (omit or `unknown`), never fabricate. The cleanup's Unknown-not-fabricated gate dot is the precedent.
- Known width trap (from cleanup): at width 120 a Blocked verdict clips the header substrate row by 5 cells — fix the width math while wiring live gate state.
- Known flake: `interactive_pty` fails under load ("provider state 0"); re-run isolated before treating as regression.

## Task 1: Substrate summary read + live header

**Files:** `crates/mycel-cli/src/ecology.rs` (new read fn), `production.rs` (build_header + refresh), header tests.

- Add `EcologyService::summary(&self, now: DateTime<Utc>) -> SubstrateStatus` where `SubstrateStatus { antibodies_active: u32, candidates_pending: u32, gate: GateStatus {Ok, Disarmed, Unknown} }`, built from the existing primitives: `open_existing()` + `list_antibodies()` (count non-expired), the candidates listing (grep how `candidates(now)` counts pending), and `read_gate_wiring(&self.paths.config)` + db presence for gate state (map wired-fail-closed+db → Ok, unwired/unreadable → Disarmed/Unknown; mirror the `/gate` panel's own status logic at ecology.rs:252-…). A missing db is a valid state (0 antibodies, gate depends on wiring) — no error for the summary.
- Call it in `prepare_interactive` (alongside the existing ecology setup), store on `PreparedInteractive`, feed `build_header` — replacing the zeroed TODO(PR4) summary and mapping `GateStatus` → header's `GateDisplay` (activating the kept ok/blocked rendering). Fix the width-120 Blocked clip in the same change (content-size or reflow the substrate line).
- Refresh path: after any ecology command that mutates (`/promote`, `/deny`) and after a gate deny is projected (Task 2 site), re-run `summary` and invalidate `header_cache`. Keep it event-driven — never per-tick.
- TDD: summary unit test against a temp substrate db fixture (grep existing ecology tests for the fixture pattern); header test asserting live counts + Ok dot renders "ok".
- Commit: `feat(tui): live substrate summary in the welcome card`.

## Task 2: Gate decision ring

**Files:** `production.rs` (ring + feed), maybe a small `tui/gate_log.rs`.

- `GateDecision { at_ms: u64, verdict: GateVerdict {Allow, Deny}, tool: String, detail: String }`; `VecDeque<GateDecision>` capped at 32 on `InteractiveLoopState`.
- Feed sites: where tool events and `TranscriptEvent::HookResult { blocked }` are projected (PR3's deny detection documented the chain — reuse it). Allows come from tool completion events; Denies from blocked hook results. Investigate what tool/target detail the events actually carry (tool frames have text like `read crates/...` — cite what you use).
- TDD: push events through the same projection and assert the ring contents + cap behavior.
- Commit: `feat(tui): bounded gate decision ring`.

## Task 3: Session rail component

**Files:** `tui/components/session_rail.rs` (+ mod registration), tests inline.

- Snapshot struct `RailData` (name/title, model, provider, cwd, mode+plan, ctx used/window — `ctx_used` finally gets wired here if context accounting is reachable from the loop state; if it is not cheaply reachable, render the window only and extend the TODO, citing where usage lives), substrate section (reuse `SubstrateStatus`), ecology slash-command grid (mockup's 8 commands + "/ for the full palette"), hyphae (active count, last — from the orchestration status the `/tasks`/hyphae path already reads; cite), footer "promotion is manual. nothing auto-promotes."
- Open (~34 cols) and collapsed (~3 cols: ● gate dot, candidate count, hyphae count, vertical label, expand chevron) renderings per the mockup. Section headers: `secondary` text + trailing dashed rule.
- TDD golden tests: open contains section headers + live values; collapsed contains the glyphs; all lines ≤ width.
- Commit: `feat(tui): session rail component`.

## Task 4: Inspector component

**Files:** `tui/components/inspector.rs`, tests inline.

- Snapshot `InspectorData` from the decision ring + substrate: `gate · last decision` (verdict/tool/target/hook line), `activity` (timestamped ring entries, `short_id` where ids appear), antibody detail box (dashed deny_border box; populate from the substrate db lookup IF a deny can be matched to an antibody with today's data — investigate what the deny path carries; otherwise render the deny detail text and omit unavailable fields, comment citing why), `candidates` (pending count + "learned, not yet trusted" + promotion-is-human-in-the-loop lines).
- Open (~50 cols) / collapsed (~3 cols: ■ last-verdict glyph, candidate count, vertical "inspector", chevron).
- TDD golden tests both states.
- Commit: `feat(tui): substrate inspector component`.

## Task 5: Body-band composition + toggles + persistence

**Files:** `production.rs` (interactive_view restructure), `tui_config.rs` (persisted rail state), reducer keybinds.

- Restructure `interactive_view`: build center lines (header card + transcript + editor, as today), rail lines, inspector lines; compose the body band with `compose::{Region, join_row}` (center is the flexible column). **Cursor math:** the editor cursor's absolute column must offset by the left rail width + border; the existing cursor/viewport logic derives from the same `lines` vec — keep that invariant and add a cursor-column offset test.
- **compose.rs verdict:** if `join_row`/`assemble` fit (uniform dashed border between the three columns — they should), consume them; if a genuine mismatch blocks you, delete compose.rs and use the hand-rolled path, recording why in the commit message. One of the two MUST happen — the review flagged compose.rs as dead code with this PR as its deadline.
- Width guardrails: below ~90 usable cols render no rails at all (center only, today's behavior); collapsed strips from ~90; allow open rails only when the center keeps ≥ 60 cols. Record thresholds as consts.
- Toggles: pick two free keybinds (grep the reducer/`LogicalAction` for taken combos first — cite what's free; suggested ctrl+b rail / ctrl+g inspector, but availability wins), route through the existing input → action path. Persist open/closed in `~/.mycel/tui.toml` (`[rails] session_open = bool, inspector_open = bool` — extend TuiConfig + save/load + doctor validation; default both COLLAPSED, matching the mockup's frozen frame).
- Invalidate the header/render caches appropriately on toggle (width of the center changes).
- TDD: interactive_view-level test (or component-seam tests) for: rails render at wide widths, absent at narrow, toggle flips persisted state, cursor lands right of the rail. Update the `/help` command list if it enumerates keybinds (grep).
- Commit: `feat(tui): body band with collapsible rails`.

## Report

Per-task status, the compose.rs verdict (consumed or deleted + why), the antibody-detail decision (what the deny payload actually carries, file:line), keybinds chosen (and what was taken), ctx_used outcome, gate summary lines proving green, deviations. Stuck >2 cycles → stop and report.
