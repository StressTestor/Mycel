# roadmap

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## roadmap stance

v0.1 ships fail-pattern immunity first. it also drafts the self-spec schema and makes the interop decision early.

interop decision: pull adapter design into v0.1 as a parallel track. v0.8 becomes hardening, fixture coverage, and export polish.

why: waiting until v0.8 would let Mycel-native shapes calcify before Hermes and OpenClaw losses are understood. early adapter design should reduce schema churn. **confidence: directional. load-bearing.**

## v0.1: fail-pattern immunity

scope:

- antibody record shape: signature, source, severity, confidence, refusal mode, expiry, examples, remediation.
- Sentinel block-log ingestion path.
- proposed-run evaluation engine.
- outcomes: refuse, warn, allow.
- substrate projections after decisions.
- eval baseline.
- parallel self-spec schema draft.
- interop decision and loss matrix.

success metrics:

- seed at least 25 antibody records from curated local examples.
- validate ingestion end-to-end on at least 10 Sentinel block events.
- pass at least 50 curated evaluation fixtures.
- keep false positives under 20 percent on fixtures labeled safe.
- expiry mechanism passes at least 10 time-shift fixtures.
- every refusal includes a remediation string and source pointer.
- interop loss matrix covers Mycel-native, Hermes, OpenClaw, and agentskills.io-compatible export shapes.
- all three gate scopes, agent launch, tool invocation, and substrate mutation, are wired with at least one fixture-validated policy each.

rollback or pivot:

- if false positives exceed 30 percent on curated safe fixtures, v0.2 pivots to signature noise reduction before decay-pruned context.
- if Sentinel logs lack stable fields, ingestion stays JSONL-only until the Sentinel contract is pinned.

parallel work:

- self-spec schema draft can run beside antibody work.
- interop loss matrix can run beside antibody work.
- fixture harness design can run beside ingestion work.

size: **m**. the mechanism is narrow, but matching quality and Sentinel ingestion make it more than a small docs-to-code pass.

load-bearing assumptions:

- fail-pattern immunity will pair cleanly with Sentinel. **confidence: directional. load-bearing.**
- antibody signatures can be specific enough to catch repeats without blocking too much adjacent work. **confidence: directional. load-bearing.**
- refusal, warning, and allow outcomes are enough for v0.1 policy. **confidence: directional. load-bearing.**

## v0.2: decay-pruned context

scope:

- scheduled ttl-tiered context maintenance.
- PromptPressure confidence-tier import.
- solid, directional, vibes, and no-compost retention behavior.
- deterministic projection updates for `SUBSTRATE.md` and `COMPOST.md`.

success metrics:

- pass at least 40 ttl fixtures across solid, directional, vibes, and no-compost records.
- preserve 100 percent of no-compost records in fixtures.
- distill directional records after ttl expiry in at least 10 fixtures.
- decay vibes records in at least 10 fixtures.
- import at least 20 PromptPressure-style tiered records from fixture data.

rollback or pivot:

- if projection diffs become noisy enough to hide semantic changes in review, v0.3 pauses for projection format cleanup.
- if tier import loses confidence labels, PromptPressure integration stays experimental.

parallel work:

- self-spec schema review continues from v0.1.
- OpenClaw and Hermes export notes can be updated against the decay model.
- antibody expiry can be tested with decay scheduling.

size: **m**. the behavior is conceptually simple, but ttl policy, projections, and no-compost preservation are sharp edges.

load-bearing assumptions:

- scheduled decay is more predictable than waiting for context overflow. **confidence: directional. load-bearing.**
- PromptPressure confidence tiers map cleanly enough to Mycel ttl tiers. **confidence: directional. load-bearing.**

## v0.3: self-spec on death

scope:

- next-agent spec schema.
- manual death-spec writing path.
- no auto-spawn.
- dedupe by task signature.
- inherited context fields with confidence tags and source pointers.

success metrics:

- pass at least 30 schema validation fixtures.
- dedupe at least 15 duplicate or near-duplicate handoff specs.
- every generated spec includes preconditions, success criteria, inherited context, and refusal risks.
- at least 10 handoff specs can be reviewed and executed manually without reading the prior full transcript.

rollback or pivot:

- if manual specs routinely need full transcript recovery, v0.4 pauses for schema repair before adding sclerotia wake conditions.

parallel work:

- v0.4 wake-condition vocabulary can be drafted as an extension field.
- spore schema can share task identity fields.
- interop export mapping can test what a degraded self-spec looks like.

size: **s**. the schema is smaller than sclerotia and does not need wake evaluation yet.

load-bearing assumptions:

- sclerotia can reuse self-spec fields instead of inventing a parallel blocked-work schema. **confidence: directional. load-bearing.**
- manual self-specs are enough to prove handoff quality before spawning. **confidence: directional. load-bearing.**

## v0.4: sclerotia

scope:

- dormant work records built on self-spec schema.
- blocker, attempted paths, next command, and wake conditions.
- wakeable state detection.
- manual resume confirmation.

success metrics:

- pass at least 30 wake-condition fixtures.
- serialize and restore at least 10 blocked-work examples.
- every dormant record references a self-spec-compatible task identity.
- no dormant record can resume without passing antibody evaluation.

rollback or pivot:

- if wake conditions are too vague to evaluate deterministically, v0.5 pauses for condition vocabulary work before spores.

parallel work:

- spore catalog schema can share condition fields.
- kin signature experiments can use dormant task identities.
- decay policy can define ttl behavior for stale dormant records.

size: **m**. it reuses self-spec, but wake conditions and restore quality are real work.

load-bearing assumptions:

- typed wake conditions can be evaluated cheaply enough for local maintenance. **confidence: directional. load-bearing.**
- dormant state can stay useful without preserving the full transcript. **confidence: directional. load-bearing.**

## v0.5: spore-based discovery

scope:

- typed spore manifest.
- completed-work and adjacent-work records.
- local spore catalog.
- germination candidates only, no germination.

success metrics:

- catalog at least 25 spores from fixture runs.
- classify at least 20 adjacent-work notices into typed candidate records.
- dedupe at least 15 repeated spores.
- export at least 10 spores into the current interop loss-matrix format.
- no spore triggers an agent launch in v0.5.

rollback or pivot:

- if spores mostly duplicate self-specs, v0.6 pauses for clearer boundaries between handoff specs and propagation manifests.

parallel work:

- kin-sharing can begin similarity experiments on spore signatures.
- interop adapters can test spore export as inert metadata.
- decay rules can define stale spore archival.

size: **m**. catalog-only keeps risk down, but this is the first real propagation story.

load-bearing assumptions:

- spores give Mycel a clearer work-discovery model before substrate-conditioned spawning. **confidence: directional. load-bearing.**
- catalog-only discovery can be useful before automatic germination exists. **confidence: directional. load-bearing.**

## v0.6: mycorrhizal kin-sharing

scope:

- kin signature and similarity rules.
- targeted context bequests on death.
- live, dormant, and catalog targets.
- source, recipient, payload type, expiry, and audit trail.

success metrics:

- route at least 20 fixture bequests to expected kin targets.
- keep misrouted bequests under 15 percent on curated negative fixtures.
- prove no bequest broadcasts a full transcript.
- run at least 10 STs-Mission-Control task identity experiments if a stable local fixture is available.

rollback or pivot:

- if misrouting exceeds 25 percent, v0.7 pauses for kin-signature cleanup before spawning.
- if STs-Mission-Control identity is too unstable, kin detection stays Mycel-local.

parallel work:

- substrate-conditioned spawning can dry-run on kin and spore signals.
- interop hardening can document bequests as Mycel-only metadata.
- decay can test bequest expiry.

size: **l**. targeted transfer is easy to describe and easy to get subtly wrong.

load-bearing assumptions:

- related task signatures can be good enough for targeted bequests. **confidence: directional. load-bearing.**
- scoped context transfer is safer than broadcast. **confidence: directional. load-bearing.**

## v0.7: substrate-conditioned spawning

scope:

- typed environmental tuple matching.
- condition-matched launch planning.
- dry-plan mode before launch.
- antibody gate on every spawn path.
- guarded germination from spores and sclerotia.

success metrics:

- pass at least 50 spawn-decision fixtures.
- prove 100 percent of spawn decisions pass antibody evaluation first.
- dry-plan mode explains trigger tuple, source record, risks, and expected outputs.
- block all fixtures marked unsafe by antibody records.
- launch only from explicit allow outcomes in integration fixtures.

rollback or pivot:

- if spawn decisions are hard to explain from local records, v0.8 becomes explainability hardening before interop polish.

parallel work:

- interop hardening can continue on exports.
- cross-mechanism integration tests can begin.
- distribution design can collect requirements without implementation.

size: **l**. this is the highest-risk mechanism and depends on every guardrail before it.

load-bearing assumptions:

- substrate conditions can be evaluated cheaply enough to gate spawn decisions. **confidence: directional. load-bearing.**
- antibody-first spawning materially reduces repeat bad runs. **confidence: directional. load-bearing.**

## v0.8: interop hardening

scope:

- harden the v0.1 interop decision into tested import/export paths.
- Mycel-native export.
- Hermes-compatible degraded export.
- OpenClaw-compatible degraded export.
- agentskills.io-compatible shape where practical.
- feature-loss declarations.

success metrics:

- export at least 20 Mycel-native skill fixtures.
- export at least 10 degraded Hermes-compatible fixtures.
- export at least 10 degraded OpenClaw-compatible fixtures.
- every degraded export declares lost ecology features.
- round-trip Mycel-native fixtures without losing antibody, sclerotia, spore, or kin metadata.

rollback or pivot:

- if degraded exports mislead users about unsupported ecology behavior, v0.9 pauses on stricter feature-loss declarations.

parallel work:

- cross-mechanism integration hardening starts here.
- documentation examples can be built from fixture exports.
- distribution layer design can reuse export metadata.

size: **m**. the decision is already made in v0.1; this phase turns it into reliable tooling.

load-bearing assumptions:

- graceful degradation is safer than claiming other runtimes enforce Mycel ecology. **confidence: directional. load-bearing.**
- schema-first adapters will reduce cross-language coupling. **confidence: directional. load-bearing.**

## v0.9: cross-mechanism integration hardening

scope:

- end-to-end local harness flows.
- failure recovery across antibodies, decay, self-specs, sclerotia, spores, kin-sharing, and spawning.
- docs cleanup for v1.0.
- fixture coverage for mechanism interactions.

success metrics:

- pass at least 10 end-to-end local project scenarios.
- pass at least 100 cross-mechanism fixtures.
- regenerate all canonical workspace projections from the canonical store.
- verify every launch, resume, and germination path has an audit record.
- remove or document every experimental flag needed for v1.0.

rollback or pivot:

- if integration scenarios require hidden ordering assumptions, v1.0 waits for an explicit substrate lifecycle model.

parallel work:

- post-v1 research can continue in docs only.
- distribution layer threat modeling can begin.
- examples can be prepared from passing scenarios.

size: **l**. hardening is mostly edge cases, interaction bugs, and documentation debt. glamorous work has left the building.

load-bearing assumptions:

- the seven mechanisms can share one substrate lifecycle without becoming a planner in disguise. **confidence: directional. load-bearing.**

## v1.0: ecological harness milestone

scope:

- local-first harness with all seven core mechanisms working together.
- documented substrate lifecycle.
- stable workspace projections.
- stable v1 schema set.
- local examples.

success metrics:

- all v0.9 integration scenarios pass.
- all v1 schema fixtures pass.
- every spawn path passes antibody evaluation.
- dormant work can wake from typed conditions.
- death records can produce specs, spores, and kin bequests.
- context decay runs on schedule.
- all substrate projections can be regenerated from the canonical store.

rollback or pivot:

- if users cannot inspect why a run launched or refused, v1.0 waits for audit readability.

parallel work:

- post-v1 lifestyle classification can remain research-only.
- mycoheterotroph detection stays research-only.
- distribution layer stays design-only unless local trust boundaries are settled.

size: **m**. v1.0 is a milestone cut after hardening, not a new feature pile.

## evals strategy

evals are transverse. every version adds fixtures for its mechanism and keeps prior fixtures passing.

| version | eval focus |
| --- | --- |
| v0.1 | antibody decisions, false positives, expiry, Sentinel ingestion |
| v0.2 | ttl tiers, PromptPressure import, no-compost preservation |
| v0.3 | self-spec validation, dedupe, manual executability |
| v0.4 | wake conditions, dormant restore, antibody-gated resume |
| v0.5 | spore catalog, adjacent-work classification, dedupe |
| v0.6 | kin routing, bequest scoping, misroute rate |
| v0.7 | spawn decisions, dry-plan explanations, antibody-first launch |
| v0.8 | degraded exports, feature-loss declarations, round-trip native metadata |
| v0.9 | cross-mechanism end-to-end scenarios |
| v1.0 | schema stability and audit readability |

fixture-first development should catch substrate drift earlier than a late eval phase. **confidence: directional. load-bearing.**

## post-v1

- lifestyle classification: parasite, saprophyte, symbiote. **confidence: vibes.**
- mycoheterotroph detection for freeloader patterns. **confidence: vibes.**
- distribution layer for selected spores, skills, and antibodies. **confidence: vibes.**
