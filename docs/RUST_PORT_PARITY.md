# Rust migration record

Mycel completed its TypeScript-to-Rust cutover on 2026-08-14. This document is
the boundary record: what the Rust CLI retained, what it deliberately left
behind, and which executable gates allowed the old implementation to be
removed.

## product boundary

Mycel is a local-first CLI and terminal UI. It does not contain:

- browser applications, bundled web assets, or editor extensions
- local HTTP/WebSocket application servers or daemon control planes
- ACP or a standalone remote client SDK
- alternate agent engines, replay visualizers, or web inspectors
- telemetry, remote banners, vendor promotions, self-update, or a bundled
  marketplace
- legacy Kimi migration machinery or runtime helper downloads

Provider APIs, explicit user-configured MCP servers, local skills, and explicit
local plugins remain CLI capabilities. They are not hidden product traffic.

## retained behavior

The Rust implementation owns:

- interactive and prompt modes, text and streaming JSON output, stable exit
  codes, and signal-aware cleanup
- create, resume, continue, fork, title, export, compact, undo, usage, status,
  model, effort, plan, permission, and workspace-root behavior
- durable provider-neutral messages, streaming records, retries, tool calls,
  usage, session replay, and restart reconciliation
- OpenAI Chat/Responses, Anthropic, Kimi, Gemini/Vertex, managed Kimi, and
  experimental Codex subscription adapters with bounded credential handling
- local tools, MCP, hooks, permissions, skills, local plugins, approvals,
  questions, goals, cron, background work, subagents, swarm, Workflow, and
  Hyphae
- Unix raw terminal mode, restoration, resize and signal handling, Unicode and
  ANSI-aware rendering, editor/history behavior, dialogs, themes, clipboard
  image paste, and textual media placeholders
- the complete mushroom ecology command family and native governed delegation

Production terminal support is macOS and Linux. Windows is intentionally not
claimed: there is no fake non-Unix terminal backend.

## mushroom ecology invariants

The following are product capabilities, not fork scaffolding:

| command | invariant |
| --- | --- |
| `/immunity` | show active antibodies grouped by severity |
| `/gate` | show whether the fail-closed gate is armed and why |
| `/substrate` | show substrate health and durable counts |
| `/candidates` | show captured lessons that remain inert until signed |
| `/promote` | explicitly sign a proposed antibody |
| `/deny` | add a hard refusal without shell interpolation |
| `/delegate` | launch a capability-bounded native child agent through the same gate |

Captured events never become active antibodies without explicit promotion.
Security-path failures fail closed; informational panels fail soft.

Workflow remains declarative JSON with sequential phases, bounded parallel work,
argument/result interpolation, restrictive manifests, restart reconciliation,
and no executable host-language workflow files. Hyphae remains session-scoped,
selects xhigh effort, and uses the bounded native swarm path without persisting
the model choice.

## cutover evidence

The old implementation was removed only after these automated gates existed and
passed:

1. Shared fixtures round-trip retained config, provider, event, record, display,
   permission, and session contracts through Rust.
2. Adversarial tests cover malformed streams, missing termination, cancellation,
   denied tools, hook failures, restart loss, concurrent work, terminal
   interruption, and cleanup.
3. CLI tests cover command help, aliases, conflicts, output formats, exit codes,
   sessions, provider commands, MCP lifecycle, and process signals.
4. Pure terminal corpora cover raw key input, editor state, logical transcript
   frames, ANSI viewport state, dialogs, overlays, and resize behavior; the
   production binary has a PTY test.
5. Ecology tests cover fail-closed gate behavior and the complete
   capture-to-ingest-to-promote-to-block loop.
6. The product-boundary test rejects a returned TypeScript workspace and scans
   Rust sources for vendor telemetry, updater, and marketplace markers.
7. Installation and CI build only the Rust workspace.

Historical internal package names and Kimi-prefixed compatibility names were
not migration requirements. The normalized Rust contracts and executable tests
are the authority now.
