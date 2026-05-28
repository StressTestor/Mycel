# architecture

note: sections marked superseded-by-ADR defer to the relevant ADR once 0001-0005 are merged. this file is high-level overview, not the source of truth for decisions.

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## project overview

mycel is a local-first personal agent harness for coding, organized around substrate ecology.

the v0 design goal is to prove that agent runs can leave durable substrate records that affect future runs. **confidence: directional. load-bearing.**

## stack and dependencies

superseded-by-ADR: `docs/adr/0003-language-and-runtime.md`

planned stack:

| layer | choice |
| --- | --- |
| core runtime | Rust |
| canonical interface | MCP |
| command surface | CLI built on MCP tool surface |
| runtime defense | Sentinel as workspace subsystem |
| canonical substrate | SQLite |
| event interchange | JSONL |
| human projections | markdown |
| Hermes interop | Python adapter |
| OpenClaw interop | TypeScript adapter |

Rust should reduce ambiguity in local policy and state handling. **confidence: directional. load-bearing.**

SQLite should be enough for local substrate queries without adding a service dependency.

current Rust dependency roles:

| dependency | role |
| --- | --- |
| `rusqlite` with bundled SQLite | canonical local store access |
| `chrono` | UTC timestamps and expiry fields |
| `uuid` | antibody identifiers |
| `serde` and `serde_json` | typed records and JSON fields |
| `thiserror` | error propagation |

## directory structure

superseded-by-ADR: `docs/adr/0003-language-and-runtime.md`

current structure:

```text
mycel/
  Cargo.toml
  crates/
    mycel-core/
    mycel-mcp/
    mycel-cli/
    sentinel-guard/
  adapters/
    hermes/
    openclaw/
  schemas/
  examples/
  docs/
    adr/
    schemas/
    open-questions.md
```

`crates/sentinel-guard` enters the workspace as a Git submodule pointed at
`https://github.com/StressTestor/sentinel.git`. Mycel builds it as a workspace
member while Sentinel keeps its own package metadata, repository, license, and
publication path for non-Mycel users.

## core subsystems

| subsystem | role |
| --- | --- |
| `mycel-core` | substrate, antibodies, wake rules, decay policy |
| `mycel-mcp` | canonical MCP interface |
| `mycel-cli` | local command surface built on MCP tools |
| `sentinel-guard` | always-on runtime defense and shared policy evaluator |

Sentinel gates three scopes:

| gate scope | purpose |
| --- | --- |
| agent launch | every spawn passes Sentinel before an agent starts |
| tool invocation | every tool call is filtered before execution |
| substrate mutation | every substrate write is checked before commit |

Each gate scope owns its policy surface but shares the Sentinel evaluator.

Volva-shedding uses Sentinel as the gate substrate. it stays post-v1, but the integration path is defined.

## key patterns

- local-first substrate state.
- confidence-tagged empirical claims and assumptions.
- schema-driven adapter boundaries.
- generated human-readable workspace projections.
- always-on runtime defense through shared Sentinel gates.

Schema-driven adapters should reduce cross-language coupling. **confidence: directional. load-bearing.**

## database schema

superseded-by-ADR: `docs/adr/0001-substrate-format.md`

current tables:

| table | role |
| --- | --- |
| `antibodies` | v0.1 fail-pattern immunity records, including signature fields, source, severity, confidence, refusal mode, remediation, examples, expiry, and hit count |
| `sentinel_audit_events` | ingested Sentinel JSONL `AuditEvent` records, preserving stable fields as typed columns and unstable fields as metadata |

SQLite `PRAGMA user_version` is the migration marker. version `2` creates the
`antibodies` table, indexes antibody `tool_pattern` and `scope`, and indexes
Sentinel `matched_rule` for source-event lineage queries.

## environment variables

no environment variables are required yet.

future cloud or model provider variables must be optional unless an ADR says otherwise. **confidence: directional. load-bearing.**

## deployment and infrastructure

no deployment target exists yet.

the default operating model is local CLI plus local MCP server.

## external services and integrations

| system | role |
| --- | --- |
| STs-Mission-Control | possible kin-detection layer |
| PromptPressure | confidence-tier input for context decay |
| OpenClaw | plugin and skill interop reference |
| Hermes Agent | skill and context lifecycle reference |

OpenClaw and Hermes are useful references for interop design, but Mycel-specific ecology metadata will need graceful degradation. **confidence: directional. load-bearing.**

## gotchas

- markdown substrate files are proposed projections.
- the canonical store stays separate from the human-readable files.
- vibes-tier claims stay hypotheses.
- autonomous spawning waits behind refusal, dormancy, decay, and handoff controls.
- Sentinel is core runtime defense.

generated projections can overwrite manual edits unless an override policy exists. **confidence: directional. load-bearing.**

## commands

current useful commands:

```sh
cargo build --workspace
cargo test --workspace
git status --short
git log --oneline
```

implementation commands do not exist yet.

## last updated

2026-05-28
