# 0001: substrate format

status: proposed

date: 2026-05-27

## context

mycel needs durable local state for antibodies, spores, sclerotia, context decay, kin links, and audit history. **confidence: directional. load-bearing.**

the substrate must be queryable by typed fields, append-friendly, easy to back up, and inspectable by a human when the harness behaves badly. **confidence: directional. load-bearing.**

## decision

use a **hybrid substrate**:

- SQLite is the canonical local store.
- JSONL is the append-only import/export and audit interchange format.
- markdown workspace files are projections generated from the canonical store.

## rationale

SQLite gives typed queries, transactions, indexes, and local portability without a server. **confidence: solid. load-bearing.**

JSONL keeps event history easy to diff, replay, redact, and move between harnesses. **confidence: directional.**

markdown projections keep the substrate legible to agents and humans without making prose files the consistency boundary. **confidence: directional. load-bearing.**

## alternatives

| option | result |
| --- | --- |
| SQLite only | strong querying, weaker portable audit stream. |
| JSONL only | easy append and audit, weaker indexed matching and updates. |
| markdown only | friendly to agents, too brittle as canonical state. **confidence: directional. load-bearing.** |

## consequences

- every substrate mutation needs a canonical database write. **confidence: directional. load-bearing.**
- every exported event should have a stable schema version. **confidence: directional. load-bearing.**
- projection regeneration must be deterministic enough for review.
- corruption recovery can use JSONL replay if the database is lost.

## unresolved

- whether to use SQLite FTS, sqlite-vec, or deterministic tags for v0.1 antibody matching.
- whether JSONL is always emitted or only on explicit export.
- whether projections are updated per mutation or during scheduled maintenance.
