# architecture

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## project overview

mycel is a local-first personal agent harness for coding, organized around substrate ecology. **confidence: directional. load-bearing.**

the v0 design goal is to prove that agent runs can leave durable substrate records that affect future runs. **confidence: directional. load-bearing.**

## stack and dependencies

planned stack:

| layer | choice | confidence |
| --- | --- | --- |
| core runtime | Rust | **directional. load-bearing.** |
| canonical substrate | SQLite | **directional. load-bearing.** |
| event interchange | JSONL | **directional.** |
| human projections | markdown | **directional. load-bearing.** |
| Hermes interop | Python adapter | **directional.** |
| OpenClaw interop | TypeScript adapter | **directional.** |

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

- local-first substrate state. **confidence: directional. load-bearing.**
- confidence-tagged claims. **confidence: directional. load-bearing.**
- schema-driven adapter boundaries. **confidence: directional.**
- generated human-readable workspace projections. **confidence: directional. load-bearing.**

## database schema

no database schema exists yet. **confidence: solid. load-bearing.**

planned first tables likely cover antibodies, source events, decisions, and projections. **confidence: directional.**

## environment variables

no environment variables are required yet. **confidence: solid. load-bearing.**

future cloud or model provider variables must be optional unless an ADR says otherwise. **confidence: directional. load-bearing.**

## deployment and infrastructure

no deployment target exists yet. **confidence: solid. load-bearing.**

the default operating model is local CLI or local service. **confidence: directional. load-bearing.**

## external services and integrations

planned integrations:

| system | role | confidence |
| --- | --- | --- |
| Sentinel | antibody source and future rule consumer | **directional. load-bearing.** |
| STs-Mission-Control | possible kin-detection layer | **directional.** |
| PromptPressure | confidence-tier input for context decay | **directional.** |
| OpenClaw | plugin and skill interop reference | **solid for reference, directional for Mycel adapter.** |
| Hermes Agent | skill and context lifecycle reference | **solid for reference, directional for Mycel adapter.** |

## gotchas

- markdown substrate files are proposed projections, not canonical state. **confidence: directional. load-bearing.**
- vibes-tier claims must stay hypotheses. **confidence: directional. load-bearing.**
- autonomous spawning is intentionally delayed until refusal, dormancy, and decay controls exist. **confidence: directional. load-bearing.**

## commands

current useful commands:

```sh
git status --short
git log --oneline
```

implementation commands do not exist yet. **confidence: solid. load-bearing.**

## last updated

2026-05-27
