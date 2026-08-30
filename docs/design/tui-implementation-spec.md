# TUI rebuild — implementation spec

Status: approved (2026-08-28). Implements the design project "Mockup review for
antibody system" (`docs/design/mycel-tui-mockup.html`, `mycel-startup-animation.html`,
and the 7-theme model in the design project's `mycel-startup-scene.js`).

Source of truth for the visuals is the design project. This spec is the source of
truth for how the Rust TUI reproduces it.

## 1. Goal

v0.3.0's rust-only cutover (ADR-0019) replaced the Kimi TypeScript harness — which
carried the branded, multi-panel UI — with a minimal renderer: a status line, a
`subagent:` line, and a `>` prompt. This spec rebuilds the TUI to match the design
mockups: an omp-style welcome card, two collapsible rails (session, substrate
inspector), a richly-styled transcript with gate allow/deny framing, a drawn input
box, a status bar, a themeable palette (7 themes), and a static pixel logo.

Non-goal: the 13s startup cinematic. It is a 1920×1080 canvas animation (camera
zoom, squash-and-stretch, sub-pixel easing, CRT scanlines, RGB-split glitch) built
for video export, not terminal playback. The TUI reuses only its pixel map (static
logo) and its theme model. An optional ~1s in-terminal boot flourish is deferred to
PR5 behind a config flag.

## 2. Decision: hand-rolled compositor, no TUI framework

The TUI is built by extending the existing hand-rolled terminal layer, not by
adopting ratatui/crossterm.

Measured cost of ratatui against the current 220-crate tree (2026-08-28,
`cargo add` resolve):

| Config | Net-new crates |
|---|---|
| ratatui, default features | +107 (termwiz, 6× wezterm, termina, terminfo, palette, pest×5, time) |
| ratatui, crossterm backend only | +57 (crossterm+winapi×3, palette×3, time×5, darling×3, strum×2, kasuari, signal-hook×2) |
| hand-rolled extension | +0 |

Beyond crate count: ratatui expects to own the terminal backend (raw mode, input,
alt-screen), which `crates/mycel-cli/src/terminal/driver.rs` already implements and
which is part of the gate-audited surface. Adopting it means either replacing that
tested layer with crossterm or writing a custom `Backend` seam — a foundation swap,
not a shortcut. The mockup's layout is a transcript plus two fixed side columns, not
something that needs a constraint solver. A +57-crate dependency to avoid a few
hundred lines of compositor code cuts against the lean direction of the cutover.
(Recorded as ADR-0021.)

## 3. Architecture

Kept as-is:
- `terminal/driver.rs` — raw mode, kitty keyboard, bracketed paste, signals, resize,
  alt-screen enter/leave.
- `terminal/render.rs` `DifferentialRenderer` — line-diff, full-clear on first paint.
- `terminal/unicode.rs` — width and truncation.

Added, above the driver, below `production.rs`'s interactive loop:

- **`terminal/style.rs` — styled spans.**
  `Style { fg: Color, bg: Option<Color>, attrs: Attrs (bold/italic/underline) }`,
  `Span { text: String, style: Style }`, `StyledLine = Vec<Span>`. `Color` is a
  24-bit RGB with a 256-color downgrade (`to_sgr(truecolor: bool)`). A `StyledLine`
  renders to one terminal line: emit each span's SGR, pad/truncate the whole line to
  the target width via `unicode::truncate_to_width`, reset at end. The
  `DifferentialRenderer` keeps diffing final strings — one styled line renders to one
  string, so the diff layer is unchanged.

- **`terminal/compose.rs` — region compositor.**
  Assembles a full screen row-by-row from regions at fixed column widths:
  `[left rail | center | right inspector]` for the body band, plus full-width bands
  for the header card, notification strip, input box, and status bar. Row assembly:
  take each region's `StyledLine` for that row, pad to the region width, join with the
  dashed vertical border span. Handles collapsed rails (26-col rail vs full width).

- **`tui/theme.rs` — theme model.** See §4.

- **`tui/view.rs` — view→lines.** Pure function: `(AppView, width, height, theme)
  → Vec<StyledLine>` (exactly `height` lines). `AppView` is a plain snapshot of app
  state (session meta, substrate summary, transcript frames, input buffer + cursor,
  rail/inspector open flags, gate last-decision, spinner phase, running timers). No
  terminal I/O, no async — fully unit-testable against golden lines.

- **`tui/components/` — the pieces** (each a fn `AppView → Vec<StyledLine>` for its
  region): `header_card` (+ `logo`), `transcript` (frame renderers), `session_rail`,
  `inspector`, `input_box`, `status_bar`, `notification`.

Render loop change in `production.rs`: replace the current 3-line composition with
`view::render(app_view, w, h, theme)` → `DifferentialRenderer`. Existing input,
session, and event plumbing are unchanged; only the composition step is replaced.

## 4. Theme model

One `Theme` = a set of named color roles. 7 built-in themes, selected by
`~/.mycel/tui.toml` `theme = "<name>"` (extends the current auto/dark/light enum;
`dark` aliases `amanita`, `light` is a warned alias of `amanita` until a light
palette is designed (a startup warning says so), `auto` → `amanita`). Exposed
for `/theme` cycling.

Themes (from the design project): **amanita** (default), **hacker**, **foxfire**,
**cordyceps**, **phosphor**, **amber**, **synthwave**.

Base roles (defined for all 7 in the design's startup model):
`bg, fg, bright, dim, dimmer, cap, rim, speck, stem, thread, thread2, accent, ok,
prompt, frame, ground`, plus `glow: bool` and `tag: String`.

TUI-only roles (the mockup uses colors the startup set doesn't name):
`panel_bg, border, value, secondary, muted, faint, accent_dim, diff_bg, diff_add,
diff_del, deny_border, deny_bg, selection`.

Amanita's TUI roles are taken **pixel-exact** from `mycel-tui-mockup.html`:
`panel_bg #0a0c0a, border #2c332c, value #c3cabe, secondary #8ba18c, muted #626d61,
faint #4a544a, accent_dim #8a3c18, diff_bg #0d0f0c, diff_add #a8b0a3, diff_del
#7d8579, deny_border #6b3111, deny_bg #140a04, selection #2a332a` (base bg #050705).

**Derivation for the other 6 themes** (see §11, the one open assumption): the design
defines those 6 only for the startup video, so their TUI-only roles are derived from
their own base roles by fixed rules, anchored so the same rules reproduce amanita's
exact values within tolerance:
`panel_bg = lighten(bg, +0.02)`, `border = blend(fg, bg, 0.80)`,
`value = blend(fg, bright, 0.5)`, `secondary = prompt`, `muted = dim`,
`faint = blend(dim, bg, 0.5)`, `accent_dim = darken(accent, 0.45)`,
`diff_bg = darken(bg toward accent, small)`, `diff_add = blend(fg, bright, 0.4)`,
`diff_del = dim`, `deny_border = darken(accent, 0.55)`, `deny_bg = darken(accent,
0.88)`, `selection = lighten(bg, +0.06)`. Amanita ships explicit overrides so it is
never approximate.

`glow` themes: the web effects (box-shadow blur, text-shadow, RGB-split glitch) have
no terminal equivalent. Approximation: `glow` maps to using `bright` (not `fg`) for
accents and applying `bold` to the wordmark/status dot; no blur, no glitch. The
per-theme `tag` line is used verbatim (e.g. hacker: "trust nothing, verify
everything").

Truecolor is emitted when the terminal supports it (Ghostty does); otherwise each
`Color` downgrades to nearest 256. Truecolor detection: `COLORTERM=truecolor|24bit`,
else assume 256.

## 5. Layout (from `mycel-tui-mockup.html`)

Full screen, top to bottom: **body band** (rails + center) then **status bar**.

Body band = `[session rail] [center] [inspector]`, each column separated by a dashed
vertical border.

### Session rail (left) — 300 cols open / 26 collapsed
Open: sections `session` (name, model, provider, cwd, mode, ctx), `substrate`
(antibodies, candidates [accent when pending], gate ● state, hook, mcp, decay),
`ecology` (2-col slash-command grid + "/ for the full palette"), `hyphae` (active,
last), footer "promotion is manual. nothing auto-promotes." Section headers are
`secondary` text with a trailing dashed rule.
Collapsed (26 cols): stacked glyphs — ● (ok), candidate count (accent), hyphae count
— vertical label "session · substrate", and an expand chevron `›`. Toggle persists
(see §7).

### Center — flex
1. **Welcome card** — dashed border with an overlapping label tab `mycel v0.4.2`
   (accent + accent_dim). Left: the pixel logo (§6) beside identity (model + `(NNNk
   context)`, `provider · gate fail-closed`, cwd). Vertical dashed divider. Right:
   `tips`, `substrate`, `recent` blocks (secondary headers, muted bodies).
2. **Notification** — `▲ N candidate pending review · run /candidates` (accent /
   accent_dim), shown when candidates pending > 0.
3. Dashed rule.
4. **Transcript** — scrollable. Each frame: a 66-col timestamp gutter (`faint`), a
   marker column, then content. Markers: user `❯` (secondary), thinking `∴`
   (italic, `stem`-ish muted, behind a show-thinking flag), assistant `·` (muted
   dot + `value` text), tool rows `⎿` tree glyph + `●` (ok) status dot. Tool row:
   `<verb> <target>` left, right-aligned `gate allow · <detail>` (`muted`); optional
   subtext (`faint`); optional diff block (`diff_bg` bg, `diff_add`/`diff_del` fg);
   running rows show a braille spinner (`⣾⣽⣻⢿⡿⣟⣯⣷`, ~90ms) and `· running · Ns`.
   Gate DENY frame: a dashed `deny_border` box on `deny_bg` — `■` (accent), the
   attempted write + right `gate · fail-closed` (accent_dim), a diff, a `DENY` badge
   (accent bg, dark fg) + `<antibody-id> <name> · severity <s> · refusal <r>`, the
   plain-language reason (`secondary`), and `captured → candidate <id> · review with
   /candidates`.
5. **Input box** — drawn frame: a `+╌╌ … ╌+` top rule carrying an inline status strip
   (`mycel ❯ [M] <model> ╌ [gate] fail-closed ╌ [N running] ╌ <cwd> … ctx ▮▮▯▯▯▯ NN%`),
   the input line (`╎ ❯ <buffer>▌ <ghost hint> ╎`), and a `+ … +` bottom rule.

### Inspector (right) — 452 cols open / 26 collapsed
Open: `gate · last decision` (verdict, tool, target, hook), `activity` (timestamped
allow/DENY log), an `antibody <id>` box (name, source, scope, severity, confidence,
refusal, hits; `signature` tool/file patterns; `decision trace` 3-step fail-closed
order; `remediation`) on a dashed `deny_border`/`deny_bg` box, and `candidates`
(pending count, id, "promotion is human-in-the-loop"). Collapsed: `■` (accent),
candidate count, vertical "inspector", expand chevron.

### Status bar (full width, bottom)
Left: keybind hints (`▸▸ gate fail-closed on`, enter/esc/ctrl+j / ctrl+c, `/`
commands). Right: `<model> · N antibodies · N candidate · /candidates`.

## 6. Pixel logo

The block-mushroom, from the shared 15×9 pixel map (`o` cap = `accent`, `W` speck =
`speck`, `r` rim = `rim`, `s` stem = `stem`). Rendered with half-block characters
(`▀▄█`) so two vertical pixels share one character cell → ~15×5 cells, colored by
theme role. The mockup's exact glyph rows (`▄`, `█`, `▀`, the dashed root
`╌╌┴╌╌╌╌┬╌╌╌╌┴╌╌`) are the reference. Colors come from the active theme, so the logo
recolors with the theme (amanita orange cap, phosphor green cap, etc.).

## 7. Interaction

- Session rail and inspector toggle open/closed; state persists to `~/.mycel/tui.toml`
  (or a sibling state file) — mirrors the mockup's `localStorage` keys
  `mycel-tui-rail-collapsed` / `mycel-tui-insp-collapsed`. Default: rail collapsed,
  inspector collapsed (the frozen frame's state).
- Keybinds for toggles: add to the session reducer (proposed `ctrl+b` rail, `ctrl+g`
  inspector — confirm against existing bindings during PR4).
- Spinner phase, cursor blink, and running-second counters advance on a timer in the
  interactive loop and are passed into `AppView`; `view.rs` stays pure.

## 8. Data wiring (live, not placeholder)

Each element binds to real state; where a query does not yet exist it is added in that
element's PR:
- session meta — session record + provider config + context accounting (exists).
- substrate summary (antibody count, candidates pending, decay pass, gate state) —
  substrate DB + gate; add read-only summary queries.
- gate last-decision + activity log — the gate/hook decision stream; add a bounded
  in-memory ring of recent decisions on the session.
- antibody detail + decision trace — substrate record for the last-denied antibody.
- hyphae — child-agent runtime status (exists via `/hyphae`).

## 9. Phased PRs

Each PR is TDD and passes the Rust gates (`cargo fmt --check`, `clippy --workspace
--all-targets -D warnings`, `test --workspace`, MSRV probe). Dependency-ordered:

1. **compositor + theme foundation** — `style.rs`, `compose.rs`, `theme.rs` (7
   themes + derivation + truecolor downgrade), tui.toml `theme` parse. No visible
   change yet; heavy unit tests (span→SGR, truncation, 256 downgrade, region join,
   every theme resolves every role).
2. **welcome card + pixel logo** — `header_card`, `logo`; wire real identity/substrate
   summary; render into the interactive loop (first visible change).
3. **rich transcript frames** — `transcript` with all frame kinds, diffs, gate
   allow/deny box, thinking, spinner. Golden-line tests per frame kind.
4. **collapsible rails** — `session_rail` + `inspector`, toggle keybinds + persistence,
   the decision ring + substrate summary queries.
5. **input box + status bar + polish** — drawn input box, status bar, notification
   strip; optional ~1s startup flourish behind a `tui.toml` flag (default off).

## 10. Testing

`terminal/render.rs` already has `MemoryTerminalSink`; `virtual_terminal.rs` parses
emitted bytes back into a grid. Strategy: `view.rs` and every component are pure
`AppView → Vec<StyledLine>`, tested against golden `StyledLine`s (text + roles, not
raw hex, so theme changes don't churn tests). One integration test per theme asserts
every role resolves and the frame renders without panic at a few widths. A snapshot
of the amanita frozen frame (the mockup's exact scenario) guards against regressions.

## 11. Theming scope (decided)

Decided 2026-08-28: all 7 themes theme the **whole** TUI, not just the startup
video. Amanita is pixel-exact from the mockup; the other 6 recolor the entire TUI
via the §4 derivation for the TUI-only roles the design defines only for the
startup. Every component (PRs 2–5) is theme-parameterized; PR1 ships all 7 themes.
