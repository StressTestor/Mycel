# 17. Spore manifest

## Status

Accepted.

## Context

Decay (v0.2) prunes context, self-specs (v0.3) hand off a task, and sclerotia (v0.4) park
blocked work. None of them *discover* new work. A spore is Mycel's work-discovery manifest:
a typed, inert record of a unit of discoverable work — either something that finished and
could seed follow-up (`CompletedWork`), or something noticed adjacent to the current task
but not done (`AdjacentWork`).

v0.5 is deliberately catalog-only. The roadmap is explicit: germination candidates only, no
germination. The first real propagation story must not also be the first autonomous-spawn
story; those risks are separated.

## Decision

A `Spore` is built **on the v0.3 `TaskIdentity`** shared primitive (ADR 0012) — the same
signature space as self-specs and sclerotia, so one task can be cross-referenced across all
three by signature. Fields:

- `task: TaskIdentity` — description + deterministic signature.
- `kind: SporeKind` — `CompletedWork` or `AdjacentWork`.
- `origin: String` — where it was discovered (source pointer, ADR 0012 format).
- `confidence: Confidence` — how strongly it is worth surfacing.
- `note: String` — short human-readable rationale.

`Spore::validate` collects all gaps (non-empty description, signature, origin, note).

**Classification.** Raw `AdjacentWorkNotice`s ("I noticed X") are turned into typed spores
by `classify_adjacent_work`: the signature is `TaskIdentity::canonicalize(description)`, the
kind is always `AdjacentWork`, and the confidence defaults to `Vibes` (a raw notice is a
hypothesis until reviewed) unless the notice carried one.

**Germination is candidate-only.** `Spore::germination_candidate` returns a
`GerminationCandidate` whose `germinated` flag is **always `false`** in v0.5. Constructing one
has no side effects: nothing is launched, nothing is spawned. The flag exists so downstream
tooling can assert the no-launch invariant explicitly.

## Consequences

Mycel gains a work-discovery model that is distinct from its handoff model: a spore says
"here is work that exists," a self-spec says "here is how to do this specific work." Because
spores reuse `TaskIdentity`, a completed-work spore can point at the same signature as the
sclerotium that was blocked on it or the self-spec that handed it off. Keeping germination to
candidates means v0.5 carries zero autonomous-spawn risk; substrate-conditioned spawning is a
later, separate decision.

Confidence: **directional. load-bearing.** Spores give a clearer work-discovery model before
substrate-conditioned spawning; whether catalog-only discovery is useful before automatic
germination exists is proven only by the fixture catalog, not by real propagation.
