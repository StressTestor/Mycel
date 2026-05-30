# 10. PromptPressure tier import

## status

accepted

date: 2026-05-30

## context

PromptPressure is an external confidence-tier source that emits findings labelled `verified`,
`probable`, or `speculative`. Mycel's run substrate uses a three-level `Confidence` enum
(`Solid | Directional | Vibes`) that drives TTL policy and the decay engine.

the import path needs to:

1. map each PromptPressure tier to the appropriate Mycel `Confidence`
2. assign a TTL based on tier so the decay engine picks them up at the right time
3. preserve the original tier label durably so the mapping is auditable and reversible

the ROADMAP specifies a hard rollback condition: "if tier import loses confidence labels,
PromptPressure integration stays experimental." label fidelity is therefore the load-bearing
correctness property.

## decision

### tier → confidence mapping

| PromptPressure tier | Mycel Confidence | rationale |
| --- | --- | --- |
| `verified` | `Solid` | verified facts; highest confidence, long retention |
| `probable` | `Directional` | shaped-right findings; medium confidence, sprint-scale ttl |
| `speculative` | `Vibes` | hypotheses; low confidence, prune aggressively |

mapping is exhaustive and covers the full PromptPressure tier vocabulary. adding a new
PromptPressure tier requires an explicit match arm in `PromptPressureTier::to_confidence` —
the compiler will fail if a variant is unhandled.

### tier → TTL policy

| tier | TTL constant | seconds | rationale |
| --- | --- | --- | --- |
| `Speculative` | `TTL_SPECULATIVE` | 604 800 (7 d) | hypotheses are high-noise; prune within a week |
| `Probable` | `TTL_PROBABLE` | 2 592 000 (30 d) | relevant across a typical sprint cycle |
| `Verified` | `TTL_VERIFIED` | 31 536 000 (365 d) | long-term substrate material; retained by decay engine |

`expires_at` is computed as `now + tier.ttl_seconds()` at import time. a verified import
will hit the decay engine after one year and transition to `decay_state = 'retained'`
(Solid policy). a probable import transitions to `'distilled'` after 30 days. a speculative
import transitions to `'decayed'` after 7 days.

### label fidelity implementation

every import appends an `audit_log` entry with `event = "promptpressure_import"` carrying:

```json
{
  "source_id": "<original PromptPressure source id>",
  "tier": "<original tier string: verified|probable|speculative>",
  "confidence": "<mapped confidence string: solid|directional|vibes>",
  "run_id": "<new run uuid>",
  "now": <import unix timestamp>
}
```

this makes the tier → confidence mapping auditable and reversible: the `runs` table holds the
mapped confidence, the `audit_log` entry holds both the original tier and the mapped confidence.
if the mapping needs to change, the audit log provides the source of truth to reconstruct which
runs were imported at which tier.

the harness asserts zero label loss across all fixtures: for every imported run, the audit
payload `"tier"` field must equal the fixture's input tier string.

**confidence: directional. load-bearing.** the rollback condition (ROADMAP) is satisfied when
and only when every `promptpressure_import` audit entry carries the original tier string.

### import API

`PromptPressureImport::import_record(record, now)` and `import_batch(records, now)` in
`crates/mycel-core/src/promptpressure.rs`. batch returns run ids in input order, which the
harness uses to zip against records for deterministic fidelity checking without re-parsing.

## rationale

- the three-way tier mapping is a clean 1:1 correspondence. no ambiguity, no merging.
- TTLs are anchored to real time units (days) and documented with rationale. the values can be
  tuned in a follow-up ADR without breaking the mapping invariant.
- audit-log label preservation is the cheapest durable fidelity proof: it adds one row per
  import, requires no schema changes, and is queryable with a standard `WHERE event =
  'promptpressure_import'` filter.
- `import_batch` is not transactional across the batch — each record commits individually. this
  means partial failures leave the DB in a partially-imported state rather than rolling back.
  a future ADR can wrap the batch in a SQLite transaction if atomicity becomes a requirement.

## consequences

- PromptPressure imports land in `runs` as `RunKind::Observation, RunStatus::Applied` with
  a TTL-gated `expires_at`. the decay engine handles them identically to any other run.
- the audit log grows by one row per imported record. at current volumes this is negligible.
- tier string is stored verbatim in the audit payload. if PromptPressure renames a tier,
  historical audit entries retain the old string. migration of labels requires a one-time
  audit-log backfill.
- `no_compost` is always `false` on imported records. if a specific finding needs permanent
  retention, it must be updated post-import.
- `TTL_VERIFIED = 365 d` is a policy default. findings tied to a specific dependency version
  or external API contract may need a shorter TTL set at import time — this is unresolved for v0.2.
