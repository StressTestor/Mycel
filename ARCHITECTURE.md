# architecture

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## project overview

mycel is a local-first personal agent harness for coding, organized around substrate ecology.

the v0 design goal is to prove that agent runs can leave durable substrate records that affect future runs. **confidence: directional. load-bearing.**

## stack and dependencies

planned stack:

| layer | choice |
| --- | --- |
| core runtime | Rust |
| canonical substrate | SQLite |
| event interchange | JSONL |
| human projections | markdown |
| Hermes interop | Python adapter |
| OpenClaw interop | TypeScript adapter |

rust should reduce ambiguity in local policy and state handling. **confidence: directional. load-bearing.**

SQLite should be enough for local substrate queries without adding a service dependency. **confidence: solid. load-bearing.**

no runtime dependencies are installed yet. **confidence: solid. load-bearing.**

## directory structure

current structure:

```text
mycel/
  ARCHITECTURE.md
  README.md
  ROADMAP.md
  CONTRIBUTING.md
  LICENSE
  .gitignore
  docs/
    adr/
    open-questions.md
```

source directories are intentionally absent during initialization. **confidence: solid. load-bearing.**

## key patterns

- local-first substrate state.
- confidence-tagged empirical claims and assumptions.
- schema-driven adapter boundaries.
- generated human-readable workspace projections.

schema-driven adapters should reduce cross-language coupling. **confidence: directional. load-bearing.**

## database schema

no database schema exists yet. **confidence: solid. load-bearing.**

planned first tables likely cover antibodies, source events, decisions, and projections. **confidence: directional.**

## environment variables

no environment variables are required yet. **confidence: solid. load-bearing.**

future cloud or model provider variables must be optional unless an ADR says otherwise. **confidence: directional. load-bearing.**

## deployment and infrastructure

no deployment target exists yet. **confidence: solid. load-bearing.**

the default operating model is local CLI or local service.

## external services and integrations

planned integrations:

| system | role |
| --- | --- |
| Sentinel | antibody source and future rule consumer |
| STs-Mission-Control | possible kin-detection layer |
| PromptPressure | confidence-tier input for context decay |
| OpenClaw | plugin and skill interop reference |
| Hermes Agent | skill and context lifecycle reference |

Sentinel pairing is the strongest reason to start with fail-pattern immunity. **confidence: directional. load-bearing.**

OpenClaw and Hermes are useful references for interop design, but Mycel-specific ecology metadata will need graceful degradation. **confidence: directional. load-bearing.**

## gotchas

- markdown substrate files are proposed projections.
- the canonical store stays separate from the human-readable files.
- vibes-tier claims stay hypotheses.
- autonomous spawning waits behind refusal, dormancy, decay, and handoff controls.

generated projections can overwrite manual edits unless an override policy exists. **confidence: directional. load-bearing.**

## commands

current useful commands:

```sh
git status --short
git log --oneline
```

implementation commands do not exist yet. **confidence: solid. load-bearing.**

## last updated

2026-05-27
