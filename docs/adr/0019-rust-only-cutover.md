# 0019: Rust-only cutover

status: accepted

date: 2026-08-14

## context

ADR-0006 adopted a Kimi Code fork to provide a mature agent body while Mycel's
substrate and policy remained in Rust. That was useful scaffolding, but keeping
two complete implementations would preserve the exact dependency and product
bloat the rewrite was meant to remove.

The Rust path now has provider, protocol, session, turn, permission, hook, tool,
MCP, skill, plugin, orchestration, terminal, CLI, and ecology implementations.
Shared fixtures, adversarial suites, CLI contract tests, a real PTY test, and
ecology end-to-end scripts cover the retained behavior.

## decision

- The Rust workspace is Mycel's only product implementation.
- Remove the TypeScript workspace, its package manager state, and its CI job.
- Support the production terminal on macOS and Linux. Do not claim Windows
  support until a real raw-mode, signal, resize, and PTY path exists.
- Keep provider APIs, explicit MCP servers, skills, and explicit local plugins;
  remove browser, editor, server, telemetry, updater, marketplace, and vendor
  control-plane surfaces.
- Preserve the mushroom ecology, native delegation, Workflow, and Hyphae.
- Preserve upstream MIT notices in `THIRD_PARTY_NOTICES.md`.
- Make Rust fixtures and executable tests the behavioral authority.

## consequences

- Cargo is the only product build tool and CI dependency.
- Installation cannot fall back to Node or a repository-bound shim.
- Historical Kimi package names and internal wire drift are no longer a
  compatibility burden.
- Deliberate unsupported behavior remains visible in
  `docs/RUST_PORT_PARITY.md`; it must not be simulated as successful.
