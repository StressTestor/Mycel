# TUI Rebuild — PR2: Welcome Card + Pixel Logo — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans, task-by-task, TDD. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Render the omp-style welcome card (with the pixel mushroom logo), themed, at the top of the interactive transcript — the first visible change of the rebuild.

**Architecture:** New pure components under `crates/mycel-cli/src/tui/components/` produce `Vec<String>` (SGR-coded) from a plain `HeaderData` snapshot + a `Theme`. `interactive_view` prepends the card to its existing `lines` (it scrolls off via the existing `viewport_start`). Theme resolution turns the config's `ThemeName` into a real `Theme`; truecolor is detected from `COLORTERM`.

**Tech Stack:** Rust, workspace deps only. Zero new dependencies (ADR-0021).

**Spec:** `docs/design/tui-implementation-spec.md` (§4 theme, §5 header card, §6 logo). PR1 foundation: `terminal/style.rs` (`Rgb`/`Color`/`Style`/`Span`/`StyledLine::render(width, truecolor)`), `terminal/compose.rs` (`Region`/`join_row`/`assemble`), `tui/theme.rs` (`Theme`, `Theme::by_name`, `Theme::ALL`, `amanita()`+6 themes, `blend`/`darken`/`lighten`).

## Global Constraints

- Zero new dependencies. No `Cargo.toml` touched.
- Iterate with `cargo test -p mycel-cli` (fast). Do NOT run `cargo test --workspace` — its first-time integration-binary compile takes ~30 min; the main loop runs the workspace gate after review. Final local gates you MUST pass: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p mycel-cli` (all exit 0, with output as evidence).
- Conventional commits, imperative, lowercase, no period, NO AI attribution. Plain professional comments (ships in the product).
- Components are pure `(&data, &Theme, width, truecolor) -> Vec<String>`, no I/O, unit-tested against the produced strings (assert on text + presence of a role's SGR, not raw hex where avoidable).
- Amanita is pixel-exact; every element colors from the active `Theme` so it recolors across all 7 themes.

## Reference: the card (from `docs/design/mycel-tui-mockup.html`)

Dashed border; an overlapping label tab `mycel v0.4.2` (accent + accent_dim). Left block: the pixel logo beside identity — `<model> (Nk context)` (value + muted), `<provider> · gate fail-closed` (secondary), `<cwd>` (muted). A vertical dashed divider. Right block, three sections with `secondary` headers and `muted` bodies: `tips` (`/ commands · ! shell · # note · shift+tab plan · esc cancel`), `substrate` (`N antibodies · N candidate pending · gate fail-closed ● ok` — candidate count in accent when > 0, ● in ok), `recent` (recent session names).

Logo (15×9 pixel map, `o`=cap, `W`=speck, `r`=rim, `s`=stem):
```
.....ooooo.....
...ooooooooo...
..oooWoooooWo..
.oooooooWooooo.
ooWoooooooooWoo
rrrrrrrrrrrrrrr
.....sssss.....
.....sssss.....
....ssssssss...
```
Render with half-block glyphs (`▀▄█`): two vertical pixels per character cell → ~5 rows × 15 cols, each cell colored by its role. A cell whose top and bottom pixels differ uses `▀` with fg=top, bg=bottom (or `▄` inverted); equal pixels use `█`; empty uses a space. The dashed root line `╌╌┴╌╌╌╌┬╌╌╌╌┴╌╌` (border) sits under the stem.

---

## Task 1: Theme resolution + truecolor detection

**Files:** Modify `crates/mycel-cli/src/tui/theme.rs`; modify `crates/mycel-cli/src/terminal/style.rs` (or theme.rs) for truecolor. Tests inline.

**Interfaces produced:**
- `Theme::resolve(config_theme: &ThemeName) -> Theme` — wait: `ThemeName` lives in `tui_config`, `Theme` in `tui::theme`; put the resolver where it avoids a cycle. Put `fn active_theme(name: &ThemeName) -> Theme` in `tui_config.rs` (it already imports `Theme`). `Named(n) → Theme::by_name(n).unwrap_or_else(Theme::amanita)`; `Auto | Dark → Theme::amanita()`; `Light → Theme::amanita()` (the seven designed themes are all dark; a light-palette TUI is out of scope — the card paints its own dark `panel_bg`, so it reads as an intentional dark widget).
- `terminal::style::truecolor_enabled() -> bool` — true iff `std::env::var("COLORTERM")` is `truecolor` or `24bit`.

- [ ] Write failing tests: `active_theme(&ThemeName::Named("hacker".into())).name == "hacker"`; `active_theme(&ThemeName::Auto).name == "amanita"`; a truecolor test that sets/reads the env is racy — instead extract `truecolor_from(value: Option<&str>) -> bool` and test it purely (`Some("truecolor")→true`, `Some("256")→false`, `None→false`); `truecolor_enabled()` calls it with the env var.
- [ ] Run: fails. Implement. Run: passes. Commit `feat(tui): resolve config theme to a Theme + detect truecolor`.

## Task 2: Pixel logo component

**Files:** Create `crates/mycel-cli/src/tui/components/mod.rs` (`pub mod logo; pub mod header;` — add `pub mod components;` to `tui/mod.rs`), `crates/mycel-cli/src/tui/components/logo.rs`. Tests inline.

**Interfaces produced:** `pub fn logo(theme: &Theme, truecolor: bool) -> Vec<String>` — the mushroom as ~5 `StyledLine::render`ed strings.

- [ ] Write failing test: `logo(&Theme::amanita(), true)` returns a non-empty `Vec`, every line non-empty, and the joined output contains amanita's cap SGR (`38;2;224;90;30`); `logo(&Theme::phosphor(), true)` contains phosphor's cap SGR and NOT amanita's (proves theming).
- [ ] Run: fails. Implement: build each half-block row from the pixel map, emit `Span`s per cell colored by role, `StyledLine::render`. Run: passes. Commit `feat(tui): pixel mushroom logo, themed`.

## Task 3: Header card component

**Files:** Create `crates/mycel-cli/src/tui/components/header.rs`. Tests inline.

**Interfaces produced:**
- `pub struct SubstrateSummary { pub antibodies: u32, pub candidates_pending: u32, pub gate_ok: bool }`
- `pub struct HeaderData { pub model: String, pub provider: String, pub cwd: String, pub ctx_used: u64, pub ctx_window: u64, pub substrate: SubstrateSummary, pub recent: Vec<String> }`
- `pub fn header_card(data: &HeaderData, theme: &Theme, width: usize, truecolor: bool) -> Vec<String>`

- [ ] Write failing test: for a sample `HeaderData` (model "claude-sonnet-4.6", provider "anthropic", cwd "~/dev/mycoforge", ctx 41200/200000, substrate {23,1,true}, recent ["cordyceps-patch"]), the joined `header_card` output contains "mycel", "claude-sonnet-4.6", "anthropic", "tips", "substrate", "recent", "23 antibodies", and the accent SGR near "1 candidate"; the border glyph appears; every line's visible width ≤ `width`.
- [ ] Run: fails. Implement the bordered card: label tab, logo (Task 2) beside identity, vertical divider, the three right-hand sections; use `theme` roles throughout; keep each line ≤ width. Run: passes. Commit `feat(tui): omp-style welcome card`.

## Task 4: Wire the card into the interactive loop

**Files:** Modify `crates/mycel-cli/src/production.rs` — add `header: HeaderData` to `InteractiveLoopState` (struct near line 3497); populate it where the state is constructed from `PreparedInteractive` (has `model_alias`, `working_dir`, `session`, `max_completion_tokens`, `ecology`); prepend the card in `interactive_view` (near line 6797, before the transcript-frame loop). Resolve the theme once via `active_theme(&state.tui_config.theme)` and `truecolor_enabled()`.

**Substrate summary source:** snapshot `SubstrateSummary` from `prepared.ecology` at construction (find the read API on `EcologyService` for antibody/candidate counts + gate state). If that read is not cheaply available at construction, populate identity fully and set `substrate` to a zeroed summary with a `// TODO(PR4): live substrate summary` and render the substrate line from whatever is available — do NOT block PR2 on substrate plumbing (PR4 owns "substrate summary queries").

**Provider name:** derive from the same source the seeded status frame uses for `model` (search for where the startup `status:` frame text is built); reuse it.

- [ ] Add the field + populate at construction. Build. 
- [ ] Prepend `header_card` lines in `interactive_view`. Add/extend a unit test if one exists for `interactive_view`; otherwise assert at the component boundary (Task 3 covers rendering).
- [ ] Run `cargo test -p mycel-cli`, `clippy`, `fmt` — all green.
- [ ] Commit `feat(tui): render the welcome card at the top of the session`.

## Self-check before reporting

- No `Cargo.toml` touched (`git diff --stat <base>..HEAD -- '**/Cargo.toml' Cargo.lock` empty).
- `fmt`/`clippy`/`test -p mycel-cli` all exit 0 — paste the summary lines.
- Report: commits (git log --oneline), new files + line counts, how you sourced provider + substrate summary, and any deviation (especially if substrate was deferred to PR4) with its reason. If stuck > ~2 fix cycles on a gate, STOP and report with the error.
