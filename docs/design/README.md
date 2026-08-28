# TUI design mockups

Static design references for the Mycel TUI. Both files are self-contained HTML
(no network, no build step) — open them in any browser.

- `mycel-tui-mockup.html` — the full-screen TUI, one frozen mid-session frame:
  session rail (collapsible), live transcript with a gate DENY
  (`ab-017 protected-path-write`), substrate inspector (collapsible), status bar.
- `mycel-startup-animation.html` — 13s looping pixel-art boot sequence:
  spore → mycelium → fruiting → wordmark → gate check ready.

Terminology mirrors the codebase: antibody fields from `mycel-core`
(source / scope / severity / confidence / refusal), fail-closed decision order
from `mycel-gate` (structural checks → protected-path floor → substrate db eval),
candidate language from the immunity loop ("learned, not yet trusted"),
keybinds from `mycel-cli`'s session reducer.

Palette: bg #0a0c0a · fg #b7beb3 · gray-green #8ba18c · amanita orange #e05a1e
(reserved for denials, candidates, wordmark) · green #55a868 (status dots only).
Type: JetBrains Mono 13px/20px.

These are design artifacts, not product code. Source of truth for the mockups
lives in the design project.
