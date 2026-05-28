# 0003: language and runtime

status: accepted

date: 2026-05-27

## context

mycel needs a local runtime that can evaluate launch conditions, store durable substrate state, gate agent behavior, and expose adapters for other agent systems. **confidence: directional. load-bearing.**

Sentinel already exists as the Rust project `sentinel-guard` and is published independently. it now becomes a core Mycel subsystem.

reference systems use TypeScript for OpenClaw and Python for Hermes Agent. **confidence: solid. source-checked 2026-05-27.**

## decision

use **Rust for the core runtime**, with adapter packages in Python and TypeScript only where interop requires them.

Sentinel is a core subsystem:

- workspace member at `crates/sentinel-guard/`.
- still published independently to crates.io for non-Mycel users.
- shared evaluator across three gate scopes: agent launch, tool invocation, substrate mutation.
- each gate scope owns its policy surface.

interface decision:

- expose CLI and MCP from day one.
- MCP is canonical.
- CLI is built on the MCP tool surface.

locked workspace layout:

```text
crates/
  mycel-core/        substrate, antibodies, wake rules
  mycel-mcp/         MCP server, canonical interface
  mycel-cli/         local command surface (built on MCP tool surface)
  sentinel-guard/    workspace member, also published independently
adapters/
  hermes/            python skill import/export
  openclaw/          typescript plugin and skill import/export
schemas/             json schema for spores, antibodies, sclerotia
examples/            small local workspaces
docs/
  adr/               architectural decision records
  schemas/           schema appendix (antibody schema, sentinel-fields, etc)
  open-questions.md
```

## rationale

Rust fits local-first state handling, typed policy evaluation, and Sentinel as always-on runtime defense. **confidence: directional. load-bearing.**

keeping Sentinel in the workspace should reduce boundary friction across launch, tool, and mutation gates while preserving independent publication. **confidence: directional. load-bearing.**

MCP as the canonical interface should keep CLI behavior from drifting away from the harness tool contract. **confidence: directional. load-bearing.**

Python is useful for Hermes export and eval workflows.

TypeScript is useful for OpenClaw plugin and manifest export.

keeping adapters thin reduces the chance that interop concerns reshape the substrate model. **confidence: directional. load-bearing.**

## consequences

- initial implementation starts with a Rust workspace.
- `sentinel-guard` needs workspace hygiene that does not break standalone publishing.
- agent launch, tool invocation, and substrate mutation each need fixtures.
- adapter contracts should be schema-driven rather than direct internal bindings. **confidence: directional. load-bearing.**
- Python and TypeScript packages should not become required for core local operation. **confidence: directional. load-bearing.**
- cross-language tests will matter once adapters exist.

## alternatives

| option | result |
| --- | --- |
| all Rust | clean core, weaker early interop ergonomics |
| all Python | faster experiments, weaker fit with Sentinel and policy runtime |
| all TypeScript | better OpenClaw alignment, weaker Sentinel alignment |
| Sentinel as external crate only | cleaner publication boundary, weaker gate integration |
| hybrid with Rust core | best balance if schemas stay tight. **confidence: directional. load-bearing.** |

## resolved items

- crate split: `mycel-core`, `mycel-mcp`, `mycel-cli`, `sentinel-guard`.
- interface: CLI plus MCP from day one.
- canonical interface: MCP.
- CLI implementation: built on MCP tool surface.
- Sentinel role: core subsystem and workspace member, with independent crates.io publication preserved.
