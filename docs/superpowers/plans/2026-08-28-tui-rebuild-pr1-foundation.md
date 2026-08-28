# TUI Rebuild — PR1: Compositor + Theme Foundation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the zero-dependency rendering foundation for the TUI rebuild — a styled-span model, a 7-theme system, and a fixed-column region compositor — with no visible UI change yet, fully unit-tested.

**Architecture:** New modules under `crates/mycel-cli/src/` sit above the existing `terminal/driver.rs` + `DifferentialRenderer`. `style.rs` turns `(text, style)` runs into padded, SGR-coded terminal lines. `theme.rs` holds one `Theme` (named color roles) with 7 built-ins. `compose.rs` joins fixed-width regions into full rows. Nothing wires into the interactive loop in this PR.

**Tech Stack:** Rust (edition 2021), the existing workspace deps only. No new crates (ADR-0021).

**Spec:** `docs/design/tui-implementation-spec.md`

## Global Constraints

- Zero new dependencies (ADR-0021). Use only crates already in the workspace.
- Rust gates must pass on every commit: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. MSRV: repo declares no `rust-version`; do not add one.
- Truecolor emitted when supported, else nearest-256 downgrade. Truecolor when `COLORTERM` ∈ {`truecolor`,`24bit`}, else 256.
- Colors are named roles, never raw hex, everywhere except `theme.rs`'s built-in tables. Tests assert roles/text, not hex.
- New modules: `crates/mycel-cli/src/terminal/style.rs`, `crates/mycel-cli/src/terminal/compose.rs`, `crates/mycel-cli/src/tui/theme.rs`. Declared in `terminal/mod.rs` and `tui/mod.rs`.
- Amanita is the default theme and is pixel-exact from `docs/design/mycel-tui-mockup.html`. The other 6 themes' base roles are verbatim from the design project's `mycel-startup-scene.js` `MYCEL_THEMES`; their TUI-only roles are derived (§4 of the spec).

---

## File Structure

- `terminal/style.rs` — `Rgb`, `Color`, `Attrs`, `Style`, `Span`, `StyledLine`; SGR emission + 256 downgrade; line render (pad/truncate to width).
- `tui/theme.rs` — `Role` usage via named fields on `Theme`; `Theme` struct; color math (`darken`/`lighten`/`blend`); 7 built-in themes; `Theme::by_name`.
- `terminal/compose.rs` — `Region`, `join_row`, full-screen assembly from region columns + dashed border.
- `tui_config.rs` (modify) — extend theme selection to the 7 names + `auto`/`dark`(=amanita)/`light`.

---

## Task 1: Rgb + Color + SGR emission

**Files:**
- Create: `crates/mycel-cli/src/terminal/style.rs`
- Modify: `crates/mycel-cli/src/terminal/mod.rs` (add `pub mod style;`)
- Test: inline `#[cfg(test)]` in `style.rs`

**Interfaces:**
- Produces: `Rgb { r: u8, g: u8, b: u8 }`; `Rgb::from_hex(&str) -> Rgb`; `Color::Rgb(Rgb)`, `Color::Reset`; `Color::fg_sgr(self, truecolor: bool) -> String` and `bg_sgr`. Truecolor fg → `38;2;r;g;b`; 256 → `38;5;<idx>` via 6×6×6 cube mapping.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn hex_parses_and_emits_truecolor_and_256() {
    let c = Color::Rgb(Rgb::from_hex("#e05a1e"));
    assert_eq!(c.fg_sgr(true), "38;2;224;90;30");
    // 256 downgrade lands in the 6x6x6 color cube (16..=231)
    let idx: u16 = c.fg_sgr(false).strip_prefix("38;5;").unwrap().parse().unwrap();
    assert!((16..=231).contains(&idx));
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p mycel-cli style::` → FAIL (module/type missing).

- [ ] **Step 3: Implement minimal `Rgb`/`Color`.** `from_hex` parses `#rrggbb`. `fg_sgr(true)` → `format!("38;2;{};{};{}", r,g,b)`. `fg_sgr(false)` maps each channel to `((v as u16 * 5 + 127) / 255)` (0..=5), index = `16 + 36*r6 + 6*g6 + b6`. `bg_sgr` uses `48`.

- [ ] **Step 4: Run to verify it passes.**

- [ ] **Step 5: Commit** — `feat(tui): add Rgb/Color with truecolor + 256 SGR`.

---

## Task 2: Style, Span, StyledLine render

**Files:**
- Modify: `crates/mycel-cli/src/terminal/style.rs`
- Test: inline

**Interfaces:**
- Consumes: `Color` (Task 1), `terminal::unicode::truncate_to_width`.
- Produces: `Attrs { bold, italic, underline }` (Copy, Default); `Style { fg: Color, bg: Option<Color>, attrs: Attrs }` with builders `Style::fg(Color)`, `.bg(Color)`, `.bold()`, `.italic()`; `Span { text: String, style: Style }`; `StyledLine(Vec<Span>)` with `StyledLine::render(&self, width: usize, truecolor: bool) -> String`. Render emits per-span SGR (`\x1b[<codes>m`), the text, resets between spans, pads the visible line to `width` with spaces, truncates overflow via `truncate_to_width`, and ends with `\x1b[0m`. Empty line → `""` (so the differ treats it as blank).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn styled_line_pads_to_width_and_wraps_sgr() {
    let line = StyledLine(vec![Span::new("hi", Style::fg(Color::Rgb(Rgb::from_hex("#55a868"))))]);
    let out = line.render(6, true);
    assert!(out.starts_with("\x1b[38;2;85;168;104m"));
    assert!(out.contains("hi"));
    assert!(out.trim_end_matches("\x1b[0m").ends_with("    ")); // padded 2 -> 6
    assert!(out.ends_with("\x1b[0m"));
}

#[test]
fn styled_line_truncates_to_width() {
    let line = StyledLine(vec![Span::new("abcdef", Style::default())]);
    // 3 visible cells max; ANSI stripped length of the text region == 3
    let out = line.render(3, true);
    assert!(out.contains("abc") && !out.contains("abcd"));
}
```

- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Implement `Attrs`/`Style`/`Span`/`StyledLine::render`.** Build the SGR code list (`1` bold, `3` italic, `4` underline, fg, optional bg) joined by `;`. Track visible width with `unicode::truncate_to_width`; pad remaining with spaces under a default style.
- [ ] **Step 4: Run to verify it passes.**
- [ ] **Step 5: Commit** — `feat(tui): styled spans render to padded SGR lines`.

---

## Task 3: Theme struct + amanita (exact) + resolution

**Files:**
- Create: `crates/mycel-cli/src/tui/theme.rs`
- Modify: `crates/mycel-cli/src/tui/mod.rs` (add `pub mod theme;` — note this is `tui/`, not `terminal/`)
- Test: inline

**Interfaces:**
- Consumes: `Rgb`, `Color` (Task 1).
- Produces: `struct Theme` with `name: &'static str`, `glow: bool`, `tag: &'static str`, and one `Rgb` field per role: base roles `bg, fg, bright, dim, dimmer, cap, rim, speck, stem, thread, thread2, accent, ok, prompt, frame, ground`; TUI roles `panel_bg, border, value, secondary, muted, faint, accent_dim, diff_bg, diff_add, diff_del, deny_border, deny_bg, selection`. `Theme::amanita() -> Theme` with the exact hex from the spec §4. `Theme::color(&self, role) -> Color` optional; direct field access is fine.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn amanita_matches_mockup_exactly() {
    let t = Theme::amanita();
    assert_eq!(t.panel_bg, Rgb::from_hex("#0a0c0a"));
    assert_eq!(t.accent,   Rgb::from_hex("#e05a1e"));
    assert_eq!(t.ok,       Rgb::from_hex("#55a868"));
    assert_eq!(t.border,   Rgb::from_hex("#2c332c"));
    assert_eq!(t.deny_border, Rgb::from_hex("#6b3111"));
    assert_eq!(t.diff_add, Rgb::from_hex("#a8b0a3"));
    assert!(!t.glow);
}
```

- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Implement `Theme` + `Theme::amanita()`** with the spec §4 hex values (base roles from `MYCEL_THEMES.amanita`, TUI roles from the mockup).
- [ ] **Step 4: Run to verify it passes.**
- [ ] **Step 5: Commit** — `feat(tui): Theme struct + exact amanita theme`.

---

## Task 4: Color math + TUI-role derivation

**Files:**
- Modify: `crates/mycel-cli/src/tui/theme.rs`
- Test: inline

**Interfaces:**
- Produces: `fn darken(Rgb, f: f32) -> Rgb`, `fn lighten(Rgb, f: f32) -> Rgb`, `fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb`; `Theme::derive_tui_roles(&mut self)` that fills the 13 TUI roles from the base roles by the spec §4 rules. Amanita does NOT call this (it has explicit overrides).

- [ ] **Step 1: Write the failing test** — the derivation, applied to amanita's base roles, lands within tolerance of amanita's real TUI values (proves the rules are anchored, not arbitrary).

```rust
fn close(a: Rgb, b: Rgb, tol: i16) -> bool {
    (a.r as i16 - b.r as i16).abs() <= tol
        && (a.g as i16 - b.g as i16).abs() <= tol
        && (a.b as i16 - b.b as i16).abs() <= tol
}

#[test]
fn derivation_approximates_amanita_tui_roles() {
    let base = Theme::amanita(); // base roles are ground truth
    let mut d = base.clone();
    // wipe TUI roles then re-derive from base
    d.derive_tui_roles();
    assert!(close(d.border, Rgb::from_hex("#2c332c"), 24));
    assert!(close(d.accent_dim, Rgb::from_hex("#8a3c18"), 24));
    assert!(close(d.deny_border, Rgb::from_hex("#6b3111"), 28));
}
```

- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Implement the color math + `derive_tui_roles`** per spec §4 (`border = blend(fg,bg,0.80)`, `accent_dim = darken(accent,0.45)`, `deny_border = darken(accent,0.55)`, `deny_bg = darken(accent,0.88)`, `value = blend(fg,bright,0.5)`, `secondary = prompt`, `muted = dim`, `faint = blend(dim,bg,0.5)`, `diff_add = blend(fg,bright,0.4)`, `diff_del = dim`, `panel_bg = lighten(bg,0.02)`, `diff_bg = blend(bg,accent,0.06)`, `selection = lighten(bg,0.06)`). Tune constants until the amanita test passes within tolerance.
- [ ] **Step 4: Run to verify it passes.**
- [ ] **Step 5: Commit** — `feat(tui): color math + derived TUI theme roles`.

---

## Task 5: The other 6 themes + full resolution

**Files:**
- Modify: `crates/mycel-cli/src/tui/theme.rs`
- Test: inline

**Interfaces:**
- Produces: `Theme::hacker/foxfire/cordyceps/phosphor/amber/synthwave()` (base roles verbatim from `MYCEL_THEMES`, TUI roles via `derive_tui_roles`); `Theme::by_name(&str) -> Option<Theme>`; `Theme::ALL: [&'static str; 7]`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn every_theme_resolves_and_is_distinct_bg() {
    let mut seen = std::collections::HashSet::new();
    for name in Theme::ALL {
        let t = Theme::by_name(name).unwrap();
        assert_eq!(t.name, name);
        // accent + bg are set (non-zero struct, all roles present by construction)
        seen.insert((t.bg.r, t.bg.g, t.bg.b));
    }
    assert_eq!(seen.len(), 7, "themes must have distinct backgrounds");
    assert!(Theme::by_name("nope").is_none());
}
```

- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Implement the 6 constructors** with base roles copied verbatim from `mycel-startup-scene.js` `MYCEL_THEMES` (bg/fg/bright/dim/dimmer/cap/rim/speck/stem/thread/thread2/accent/ok/prompt/frame/ground/glow/tag), each calling `derive_tui_roles()`. Add `by_name` + `ALL`.
- [ ] **Step 4: Run to verify it passes.**
- [ ] **Step 5: Commit** — `feat(tui): add hacker/foxfire/cordyceps/phosphor/amber/synthwave themes`.

---

## Task 6: Region compositor

**Files:**
- Create: `crates/mycel-cli/src/terminal/compose.rs`
- Modify: `crates/mycel-cli/src/terminal/mod.rs` (`pub mod compose;`)
- Test: inline

**Interfaces:**
- Consumes: `StyledLine`, `Span`, `Style`, `Color` (Tasks 1–2).
- Produces: `struct Region { width: usize, lines: Vec<StyledLine> }`; `fn join_row(cols: &[&Region], row: usize, border: Style, truecolor: bool) -> StyledLine` — for each column, take its `row`-th line (or blank), pad to the column width, separate columns with a single dashed vertical border span (`│`/`╎` per mockup); `fn assemble(cols: &[Region], height: usize, border: Style, truecolor: bool) -> Vec<StyledLine>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn join_row_pads_columns_and_inserts_border() {
    let a = Region { width: 4, lines: vec![StyledLine(vec![Span::new("hi", Style::default())])] };
    let b = Region { width: 5, lines: vec![StyledLine(vec![Span::new("yo", Style::default())])] };
    let row = join_row(&[&a, &b], 0, Style::default(), true);
    let text: String = row.0.iter().map(|s| s.text.as_str()).collect();
    // "hi  " (4) + border + "yo   " (5)
    assert!(text.contains("hi") && text.contains("yo"));
    assert!(text.chars().any(|c| c == '╎' || c == '│'));
}
```

- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Implement `Region`/`join_row`/`assemble`.** Pad each column line's visible width to `Region.width`; push a border span between columns; blank rows beyond a region's lines render as full-width padding.
- [ ] **Step 4: Run to verify it passes.**
- [ ] **Step 5: Commit** — `feat(tui): fixed-column region compositor`.

---

## Task 7: Wire theme selection into tui.toml

**Files:**
- Modify: `crates/mycel-cli/src/tui_config.rs` (the `ThemeName` enum + parse)
- Modify: `crates/mycel-cli/src/production.rs` (`resolved_theme` maps a `ThemeName` to a `Theme`)
- Test: inline in `tui_config.rs`

**Interfaces:**
- Consumes: `Theme::by_name`, `Theme::ALL` (Task 5).
- Produces: `ThemeName` accepts `auto`, `dark`, `light`, and the 7 theme names; `ThemeName::parse` returns an error listing valid names; `fn theme_for(name: ThemeName, terminal_bg_is_light: bool) -> Theme` (`auto`/`dark` → amanita, `light` → existing light palette as a `Theme`, a named theme → that theme).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn theme_names_parse() {
    assert!(ThemeName::parse("hacker").is_ok());
    assert!(ThemeName::parse("amanita").is_ok());
    assert!(ThemeName::parse("dark").is_ok());
    assert!(ThemeName::parse("bogus").is_err());
}
```

- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Extend `ThemeName`** to carry a named variant (e.g. `ThemeName::Named(String)` validated against `Theme::ALL`) alongside `Auto`/`Dark`/`Light`; update `parse`, `as_str`, and the tui.toml serializer. Keep `doctor`'s validation passing.
- [ ] **Step 4: Run gates** — `cargo test -p mycel-cli`, `clippy`, `fmt`.
- [ ] **Step 5: Commit** — `feat(tui): select any of the 7 themes via tui.toml`.

---

## Self-Review

- **Spec coverage:** §3 (style/compose/theme modules) → Tasks 1–2, 6, 3–5; §4 (theme model, 7 themes, derivation, truecolor downgrade) → Tasks 1, 3–5, 7; §4 glow-approximation and per-component theming land in PRs 2–5, not here. Layout (§5), logo (§6), interaction (§7), data wiring (§8) are PRs 2–5.
- **Placeholder scan:** none — every code step has real code or exact rules.
- **Type consistency:** `Rgb`/`Color`/`Style`/`Span`/`StyledLine`/`Theme`/`Region` names are used identically across tasks; `from_hex`, `render(width, truecolor)`, `derive_tui_roles`, `by_name`, `join_row` signatures are stable.

## Roadmap — PRs 2–5

Each gets its own plan authored against PR1's landed interfaces:
- **PR2** welcome card + pixel logo (`tui/components/header.rs`, `logo.rs`) — consumes `StyledLine`, `Theme`.
- **PR3** transcript frames (`tui/components/transcript.rs`) — frame renderers, diffs, gate allow/deny box, spinner.
- **PR4** collapsible rails (`session_rail.rs`, `inspector.rs`) + toggle keybinds/persistence + substrate summary queries + gate decision ring.
- **PR5** input box + status bar + notification (`input.rs`, `status.rs`) + optional startup flourish behind a flag.

## Execution Handoff

Execute with subagent-driven-development (fresh subagent per task, review between) or inline via executing-plans.
