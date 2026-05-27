# 0001: substrate format

status: proposed

date: 2026-05-27

## context

mycel needs durable local state for antibodies, spores, sclerotia, context decay, kin links, and audit history. **confidence: directional. load-bearing.**

the substrate must be queryable by typed fields, append-friendly, easy to back up, and inspectable by a human when the harness behaves badly. **confidence: directional. load-bearing.**

## decision

use a **hybrid substrate**:

- SQLite is the canonical local store.
- JSONL is the append-only audit stream and interchange format.
- markdown workspace files are projections generated from the canonical store.

lock these v0.1 details:

- antibody matching uses deterministic tag matching.
- sqlite-vec stays behind a feature flag for v0.2 evaluation.
- JSONL is always emitted with a rotation policy.
- projections update per mutation with a 500ms debounce.

## rationale

SQLite gives typed queries, transactions, indexes, and local portability without a server.

JSONL keeps event history easy to diff, replay, redact, and move between harnesses. **confidence: directional.**

markdown projections keep the substrate legible to agents and humans without making prose files the consistency boundary. **confidence: directional. load-bearing.**

deterministic tag matching is a better v0.1 fit than vector similarity because refusal policy needs explainable false-positive handling. **confidence: directional. load-bearing.**

always-emitted JSONL gives recovery and audit lineage from the first release. **confidence: directional. load-bearing.**

## alternatives

| option | result |
| --- | --- |
| SQLite only | strong querying, weaker portable audit stream |
| JSONL only | easy append and audit, weaker indexed matching and updates |
| markdown only | friendly to agents, too brittle as canonical state |
| vector matching in v0.1 | better fuzzy lookup, weaker refusal explainability |

## consequences

- every substrate mutation needs a canonical database write.
- every mutation also emits a JSONL event.
- projection regeneration must be deterministic enough for review.
- rotation policy becomes part of the v0.1 storage contract.
- sqlite-vec evaluation can run without affecting refusal decisions.

## resolved items

- antibody matching: deterministic tags in v0.1, sqlite-vec feature flag for v0.2 evaluation.
- JSONL emission: always emit, with rotation policy.
- projection updates: per mutation, debounced by 500ms.
