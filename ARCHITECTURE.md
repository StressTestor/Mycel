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
    mycel-tests/
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
| `mycel-core` | substrate, antibodies, deterministic proposed-run evaluation, audit/projection runtime |
| `mycel-mcp` | canonical tool surface for ingest, evaluate, list-antibodies, and harness metrics |
| `mycel-cli` | local command surface that calls the MCP tool surface |
| `mycel-tests` | external black-box adversarial suite for v0.1 fail-pattern immunity |
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
- deterministic antibody evaluation: populated signature fields are AND matches,
  empty signature fields are wildcards, expired antibodies do not gate runs,
  `file_pattern` uses glob matching, and `command_pattern` uses substring matching.
- substrate mutations append JSONL audit events immediately and debounce
  `SUBSTRATE.md` projection regeneration by 500ms.
- ttl-tiered decay maintenance: solid records are retained, directional records
  are distilled to a gist, vibes records decay to a tombstone, and `no_compost`
  records are preserved regardless of tier.
- handoff specs (self-spec) and dormant-work records (sclerotia) share one
  `TaskIdentity` signature; dormant records become wakeable only when all typed
  wake conditions are met, and resume only through antibody-gated, manual-confirm
  evaluation — never auto-execution.
- work-discovery spores (completed-work / adjacent-work) reuse the same
  `TaskIdentity` signature, are catalogued dedup-on-write, and export to the
  interop loss-matrix shapes as inert metadata that declares its dropped ecology
  fields; v0.5 produces germination candidates only and never launches an agent.

Schema-driven adapters should reduce cross-language coupling. **confidence: directional. load-bearing.**

## database schema

superseded-by-ADR: `docs/adr/0001-substrate-format.md`

current tables:

| table | role |
| --- | --- |
| `antibodies` | v0.1 fail-pattern immunity records, including signature fields, source, severity, confidence, refusal mode, remediation, examples, expiry, and hit count |
| `sentinel_audit_events` | ingested Sentinel JSONL `AuditEvent` records, preserving stable fields as typed columns and unstable fields as metadata |
| `runs` | v0.2 substrate run records: kind, status, summary, confidence, TTL (`expires_at`), preservation flag (`no_compost`), decay state (`retained`/`distilled`/`decayed`), and `distilled_summary` gist |
| `audit_log` | append-only structured event log; entries include `event` type (e.g. `decay`, `promptpressure_import`, `maintenance`) and a JSON payload |
| `specs` | v0.3 self-spec handoff records stored as JSON with an indexed `signature` column |
| `sclerotia` | v0.4 dormant-work records (blocker, attempted paths, next command, typed wake conditions) stored as JSON with an indexed `signature` column |
| `spores` | v0.5 work-discovery manifests (completed-work / adjacent-work) stored as JSON with indexed `signature` and `kind` columns |

SQLite `PRAGMA user_version` is the migration marker. version `4` creates the
`runs` and `audit_log` tables in addition to the v3 schema. The `specs` (v0.3),
`sclerotia` (v0.4), and `spores` (v0.5) tables are added additively to the same schema
build, so they do not bump `user_version` past 4.

Sentinel `matched_rule` parsing populates signature fields:
- `deny.paths: X` or `allow.paths: X` → `file_pattern = X`
- `deny.commands: X` or `allow.commands: X` → `command_pattern = X`
- `deny.secrets: X` → `error_class = X`

Signature matching uses glob patterns for `file_pattern` (supports `*`, `**`, `?`)
and substring matching for `command_pattern`.

## projections and audit

`SubstrateRuntime` wraps the SQLite store when mutations need filesystem side
effects. every antibody insert, update, and delete appends one JSONL audit event
and schedules `SUBSTRATE.md` regeneration for 500ms after the latest mutation.

`SUBSTRATE.md` carries a generated-file header that says it is projection-only
and not an input surface. audit logs rotate from `name.jsonl` to `name.1.jsonl`
when the configured size limit would be exceeded by the next event.

`mycel maintain` runs a full decay cycle and regenerates two workspace files:

| file | content |
| --- | --- |
| `SUBSTRATE.md` | live / retained / preserved runs (active substrate) |
| `COMPOST.md` | distilled runs (gist kept) + decayed runs (tombstone only) |

both files are deterministic projections (stable sort by `(created_at, id)`, no generation
timestamp in body). see ADR 0011.

## eval harness

The v0.1 harness has an in-code seed corpus with at least 25 antibodies, 50
evaluation fixtures, 10 Sentinel events, 10 expiry fixtures, and all three gate
scopes. `mycel harness` calls the MCP tool surface and prints JSON metrics for
the roadmap success criteria.

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
cargo fmt -p mycel-core -p mycel-mcp -p mycel-cli -p mycel-tests
cargo clippy --workspace --all-targets -- -D warnings
mycel harness
mycel ingest --jsonl <path>
mycel evaluate --tool-name <name>
mycel list-antibodies
mycel import-promptpressure --db <path> --jsonl <path> [--now <ts>]
mycel maintain --db <path> --workspace <dir> [--now <ts>]
git status --short
git log --oneline
```

note: `cargo fmt --all` walks into `crates/sentinel-guard/` (submodule) and reformats code we don't own. always scope fmt to the four mycel crates.

implementation commands do not exist yet.

## last updated

2026-05-30
