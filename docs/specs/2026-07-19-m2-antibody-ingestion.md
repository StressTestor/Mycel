# m2: antibody ingestion loop (the immunity learns)

date: 2026-07-19
status: building
depends on: harness graft m1 (mycel-gate, mycel-substrate, mycel-mcp-server)

## what m1 left open

m1 shipped a fail-closed antibody GATE: it blocks tool calls that match active
antibodies. but the substrate was static — nothing fed it. an operator had to
hand-author every antibody. the immunity could enforce, but it could not learn.

## the loop m2 closes

```
tool fails / is blocked
  -> PostToolUseFailure hook -> mycel-observe
       appends a SentinelAuditEvent JSONL line to ~/.mycel/substrate/audit.jsonl
  -> SessionEnd hook -> mycel-substrate ingest
       records the events + surfaces antibody CANDIDATES (inert)
  -> human review -> mycel-substrate antibody-add
       promotes a chosen candidate to an active antibody
  -> next time, mycel-gate BLOCKS that pattern
```

the substrate now learns from what goes wrong, but nothing auto-activates:
candidates are inert until a human promotes them. this preserves the
"inert until promoted" principle from the mcp propose path — the loop
surfaces signal, it does not silently expand what gets blocked.

## components

- **mycel-observe** (new crate): the capture half. a PostToolUseFailure hook
  binary. reads the hook JSON on stdin, appends one `SentinelAuditEvent`-shaped
  line (`{timestamp, tool_name, action:"block", mode:"observe", reason,
  matched_rule:null}`) to the audit log. observation-only, fail-safe: always
  exits 0, never disrupts the harness, creates the audit log if absent
  (append-only observation data, unlike the gate which must never create the db).
- **mycel-substrate ingest** (exists): reads the audit JSONL, records the
  sentinel events, returns candidates. wired as a SessionEnd hook.
- **mycel-substrate antibody-add** (exists): promotes a candidate to active.
- **mycel-core `ingest_sentinel_audit_jsonl`** (exists, v0.1): the parse +
  candidate-derivation the emitted line is coupled to (round-trip tested).

## why this is the right m2

it is the v0.1.1 "sentinel integration hardening" track from ROADMAP.md made
real against the harness: the harness's own failures ARE the sentinel-style
block events the core was designed to ingest. it makes Mycel a self-improving
coding harness rather than a static rule-checker — the defining substrate-
ecology behavior.

## verification

`tests/e2e/immunity-loop.sh`: deterministic, no model. novel bad command
allowed -> fails -> captured -> ingested as candidate (still inert, gate still
allows) -> promoted -> gate now blocks the same command with a learned
remediation. the whole loop, end to end.

## not in m2 (future roadmap)

decay-pruned context (v0.2), self-spec on death (v0.3), sclerotia (v0.4),
spore discovery (v0.5), kin-sharing (v0.6), conditioned spawning (v0.7). m2 is
the immunity loop only.
