# 15. Sclerotia (dormant work records)

## Status

Accepted.

## Context

Work gets blocked: a dependency is unbuilt, a file is missing, a review is pending, a
human decision is owed. v0.3 self-specs capture a *handoff* to the next agent, but not a
*pause*: a record of work parked mid-flight that should resume when its blocker clears.

A sclerotium (in fungal ecology, a hardened dormant mass that survives lean conditions and
germinates when they improve) is Mycel's dormant-work record. It captures the blocked
task's identity, what blocked it, what was already tried, the concrete next command, and
the typed conditions under which it becomes wakeable again.

## Decision

Add a `Sclerotium` record built **on the v0.3 `TaskIdentity`** shared primitive (ADR 0012),
not a parallel blocked-work schema — satisfying the roadmap intent that sclerotia reuse
self-spec fields. Fields:

- `task: TaskIdentity` — description + deterministic signature (the dedupe/reference key).
- `blocker` — what stopped progress.
- `attempted_paths` — what was already tried (so a resumer does not repeat it).
- `next_command` — the concrete command to run on resume.
- `wake_conditions: Vec<WakeCondition>` — typed conditions (ADR 0016), AND semantics; an
  empty list means *not wakeable*.
- `inherited_context: Vec<InheritedContext>` — confidence-tagged claims with source pointers.

`Sclerotium::validate` collects all gaps at once (non-empty description, signature, blocker,
next_command, and at least one wake condition). `SclerotiumStore` persists records as a JSON
blob plus an indexed `signature` column, mirroring `SpecStore`.

**Wakeable-state detection** is `is_wakeable(world)`: non-empty wake conditions AND every
condition met against a caller-supplied `WakeWorld` (no hidden clock or filesystem reads,
per ADR 0008 time-injection).

**Resume is antibody-gated and manual-confirm only.** `evaluate_resume` is pure and returns
one of three decisions — `NotWakeable`, `BlockedByAntibody`, `ReadyForManualResume`. It maps
`next_command` to a `ProposedRun` (tool name = first whitespace token) and runs the v0.1
antibody evaluator; a `Refuse` outcome yields `BlockedByAntibody`. The most permissive
outcome is *ready for manual resume* — nothing auto-executes and nothing auto-spawns, so the
v0.3 no-auto-spawn constraint carries forward unchanged.

The `sclerotia` table is added to `FULL_SCHEMA_SQL` additively (idempotent
`CREATE TABLE IF NOT EXISTS` + index) with no schema-version bump; `user_version` stays 4.

## Consequences

Parked work survives a session without preserving the full transcript: the record carries
enough (blocker, attempts, next command, typed wake conditions) for a resumer to act. Reuse
of `TaskIdentity` means a dormant record, a self-spec, and (v0.5) a spore can be cross-
referenced by the same signature. The antibody gate means a dormant record can never resume
into a refused action, even if its wake conditions are met. Decay policy for stale dormant
records is left to a parallel-work item.

Confidence: **directional. load-bearing.** Typed wake conditions are cheaply and
deterministically evaluable (proven by fixtures); whether dormant state stays useful at
scale without the transcript is proven only on a small corpus.
