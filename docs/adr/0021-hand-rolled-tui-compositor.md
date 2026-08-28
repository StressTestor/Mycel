# 0021: Hand-rolled TUI compositor

status: accepted

date: 2026-08-28

## context

The TUI rebuild (`docs/design/tui-implementation-spec.md`) needs multi-panel
layout: two collapsible rails beside a transcript, a drawn welcome card, a drawn
input box, and a themeable styled palette. The post-cutover renderer
(`terminal/render.rs`) is a single-column line-diff over pre-colored strings.

The obvious framework is ratatui. Measured against the current 220-crate tree on
2026-08-28: ratatui with default features resolves +107 net-new crates (termwiz,
six wezterm crates, termina, terminfo, palette, pest, time); with only the
crossterm backend, +57 (crossterm, palette, time, darling, strum, kasuari,
signal-hook, parking_lot). ratatui also expects to own the terminal backend — raw
mode, input, alt-screen — which `terminal/driver.rs` already implements and which
is part of the gate-audited surface. Adopting it means replacing that tested layer
with crossterm or writing a custom backend seam.

## decision

- Build the TUI by extending the existing hand-rolled terminal layer. Do not adopt
  ratatui, crossterm, or another TUI framework.
- Keep `terminal/driver.rs` (raw mode, input, alt-screen) and the
  `DifferentialRenderer` as the rendering foundation.
- Add a styled-span model, a fixed-column region compositor, and a theme module
  above them; the diff layer keeps diffing final strings.

## consequences

- Zero new dependencies; the lean cutover (ADR-0019) holds.
- The gate-audited terminal driver keeps its contract; no backend swap.
- Layout and widget behavior are ours to write and test — a few hundred lines of
  compositor code and golden-line tests, rather than a framework's widget set.
- If a future screen genuinely needs constraint-solved layout, this decision is
  revisited then, not pre-emptively.
