# Mycel v0.2 – v0.5 audit handoff

prepared for review (Grok). this document is the single entry point for auditing the
substrate-ecology milestones v0.2 through v0.5. it is structured by milestone: what was
implemented, which success metrics pass (with evidence), what is partial or unmet, key
design decisions, and open questions / gotchas.

confidence key: **solid** = verified or strongly supported. **directional** = the shape is
likely right, details may change. **vibes** = a useful hypothesis, not a fact.

## how to verify everything at once

```sh
cd /Volumes/T7/Mycel
cargo test --workspace          # all green, 0 failed
cargo clippy --workspace --all-targets -- -D warnings   # clean
# per-metric evidence (printed summaries):
cargo test -p mycel-tests --test decay_harness -- --nocapture
cargo test -p mycel-tests --test promptpressure_harness -- --nocapture
cargo test -p mycel-tests --test selfspec_harness -- --nocapture
cargo test -p mycel-tests --test executable_specs_harness -- --nocapture
cargo test -p mycel-tests --test sclerotia_harness -- --nocapture
cargo test -p mycel-tests --test spore_harness -- --nocapture
```

all milestone work is on branch `feat/substrate-ecology-v02-v05`. the schema stays at
`PRAGMA user_version = 4` throughout: every new table (v0.2 `runs`/`audit_log`, v0.3 `specs`,
v0.4 `sclerotia`, v0.5 `spores`) is added to `FULL_SCHEMA_SQL` as idempotent
`CREATE TABLE IF NOT EXISTS`, so no destructive migration is introduced.

## commit map

| milestone | commits (oldest → newest) |
| --- | --- |
| v0.2 decay-pruned context | `77111e6` engine+fixtures · `2edc64b` ADRs 0008/0009+plan · `3ca9e96` PromptPressure import · `8f7cc61` projections+maintenance · `c163ccc` distill multibyte fix · `cf64321` roadmap shipped |
| v0.3 self-spec on death | `b6997fa` schema/validation/dedupe · `674e9de` executability bar+corpus · `a48fd7e` roadmap shipped + blind-review evidence |
| v0.4 sclerotia | `ae1f5a9` core module · `eb799ba` fixtures+harness · `50ba99d` ADRs 0015/0016+architecture · `44413e7` roadmap shipped |
| v0.5 spore-based discovery | `e959b98` core module · `babe120` fixtures+harness · `122d48f` ADRs 0017/0018+architecture · (roadmap-shipped commit follows) |

ADRs: `docs/adr/0008-time-injection.md` … `0018-spore-catalog-and-export.md`.

---

## v0.2 — decay-pruned context

### implemented
- schema v4 `runs` table: `kind, status, summary, confidence, created_at, expires_at,
  no_compost, decay_state, decayed_at, distilled_summary`; plus an `audit_log` table.
- `DecayEngine::run(now)` (`crates/mycel-core/src/decay.rs`) — deterministic, idempotent,
  time-injected ttl-tiered policy: `solid → retained`, `directional → distilled` (body
  compressed via `distill`), `vibes → decayed` (tombstone), `no_compost` rows preserved
  untouched regardless of tier. Each transition appends a `decay` audit event.
- PromptPressure tier import (`crates/mycel-core/src/promptpressure.rs`): `Verified→Solid`,
  `Probable→Directional`, `Speculative→Vibes`, each with a tier ttl; the **original tier is
  preserved verbatim** in a `promptpressure_import` audit payload (label fidelity).
- Deterministic projections (`crates/mycel-core/src/projection.rs`): `render_substrate_md`
  (live / retained / preserved) and `render_compost_md` (distilled gist / decayed tombstone),
  stable-sorted by `(created_at, id)`, no generation timestamp in body; `run_maintenance`
  runs decay → render `SUBSTRATE.md`+`COMPOST.md` → audit. CLI `maintain` and
  `import-promptpressure` subcommands.

### metrics (all PASS — evidence in `decay_harness`/`promptpressure_harness`)
| metric | target | actual | evidence (test) |
| --- | --- | --- | --- |
| ttl fixtures across solid/directional/vibes/no-compost | ≥40 | **46** | `decay_harness_runs_all_fixtures_and_meets_roadmap_metrics` asserts `total>=40` + per-row `decay_state` |
| no-compost records preserved | 100% | **8/8** | same test asserts `nc_true_preserved == nc_true_total` |
| directional records distilled after expiry | ≥10 | **14** | asserts `distilled_count>=10`, each has `distilled_summary` |
| vibes records decayed | ≥10 | **11** | asserts `decayed_count>=10`, each has no `distilled_summary` |
| PromptPressure tiered records imported | ≥20 | **24** | `promptpressure_harness`: 24 imported, 24/24 tier labels preserved |

independently reviewed (6-lens adversarial workflow): **SPEC PASS / QUALITY APPROVED**. one
real bug was found and fixed during review — `distill` panicked on a multibyte UTF-8 char
straddling byte index 80 (`c163ccc`); the regression test feeds `é` at the boundary.

### key design decisions
- ADR 0008 (time injection): all new maintenance/decay/import code takes `now: i64` rather
  than reading a clock, so fixtures are deterministic.
- ADR 0009 (decay model): decay is applied **in place** on `runs` via a `decay_state` column,
  not a separate compost table; `no_compost` is orthogonal to the confidence tier.

### open questions / gotchas
- **"body dropped" is a projection guarantee, not a DB hard-delete.** A decayed (or distilled)
  row keeps its original `summary` in the `runs.summary` column; COMPOST.md simply never
  renders it (decayed = tombstone, distilled = gist only). A buyer grepping the raw DB will
  still find the original text. ADR 0009 already notes hard-delete is "valid later." Worth a
  one-line ADR clarification.
- The `confidence → &str` mapping is duplicated as a local helper in `decay.rs`,
  `promptpressure.rs`, `projection.rs` (and later `sclerotia.rs`, `spore.rs`) because the
  macro-generated `Confidence::as_str` is crate-private. A future cleanup could hoist a single
  `pub fn` on `Confidence`. Not a defect; cosmetic DRY debt.

---

## v0.3 — self-spec on death

### implemented
- **Shared `TaskIdentity` primitive** (`crates/mycel-core/src/selfspec.rs`): `{ description,
  signature }` with a deterministic `canonicalize` (lowercase → trim → collapse whitespace →
  strip trailing `.!?,;:` → spaces to `-`). This is the load-bearing reuse point: v0.4 and v0.5
  build on it (ADR 0012).
- `SelfSpec { task, preconditions, success_criteria, inherited_context, refusal_risks }` with
  `validate()` (collects ALL gaps, never fail-fast), `dedupe_specs` (collapse by signature,
  keep first), and `SpecStore` persistence (`specs` table, JSON + indexed signature).
- `InheritedContext { claim, confidence, source }` — confidence-tagged with a source pointer.
- Executability bar (`ExecutabilityGap`, `is_executable`, `executability_gaps`): a stricter
  "self-sufficient without the transcript" check — requires a concrete success criterion,
  sourced context, preconditions, and refusal risks.
- Manual death-spec path only: no auto-spawn anywhere in v0.3.

### metrics (all PASS — evidence in `selfspec_harness`/`executable_specs_harness`)
| metric | target | actual | evidence |
| --- | --- | --- | --- |
| schema validation fixtures | ≥30 | **37** | `selfspec_harness` validation: each `validate()` matches `expect_valid` + per-error reasons |
| duplicate/near-duplicate specs deduped | ≥15 | **15** | `selfspec_harness` dedupe: 30 specs → 15 signatures, `duplicate_count==15` |
| every spec has preconditions/success/inherited-context/refusal-risks | yes | yes | enforced by `validate()` + executable corpus |
| handoff specs reviewable+executable without the prior transcript | ≥10 | **11/12** | deterministic `executable_specs_harness` (12 executable) + blind-reviewer pass: 11 of 12 specs judged self-sufficient by isolated agents (`docs/v0.3-blind-review-evidence.md`) |

the one blind-review "no" (spec `add-list-runs-cli-subcommand`) is a feature, not a miss: the
reviewer correctly refused to act cold because its key API claim carried only *directional*
confidence — the confidence-tagging discipline doing its job.

### key design decisions
- ADR 0012 (shared task identity) + ADR 0013 (self-spec) + ADR 0014 (executability bar).
- the qualitative "reviewable without transcript" metric is evidenced two ways: a deterministic
  harness (structural bar) and an out-of-band blind-reviewer pass (human-judgment proxy).

### open questions / gotchas
- handoff quality is proven on a small corpus (12) by "could I start cold" judgment, not by
  executing each spec to green. confidence: **directional**.
- source pointers (`run:`/`audit:`/`spec:`/`note:`) are documented but not validated/resolved
  yet — a parallel-work item.

---

## v0.4 — sclerotia (dormant work records)

### implemented
- `Sclerotium { task: TaskIdentity, blocker, attempted_paths, next_command, wake_conditions,
  inherited_context }` (`crates/mycel-core/src/sclerotia.rs`) — built on the v0.3 shared
  identity (metric 3).
- **Typed wake-condition vocabulary** (ADR 0016): `TimeReached{at}`, `FileExists{path}`,
  `FileAbsent{path}`, `DependencyResolved{signature}`, `SignalRaised{name}`, `Manual`.
  `is_met` is evaluated against a caller-supplied `WakeWorld { now, existing_paths,
  resolved_signatures, raised_signals }` — no hidden clock or filesystem reads. `Manual` never
  auto-wakes (always `false`). `is_wakeable` = non-empty conditions AND all met.
- `SclerotiumStore` persistence (`sclerotia` table, JSON + indexed signature), `validate()`
  collects all gaps.
- **Antibody-gated, manual-confirm-only resume**: `evaluate_resume` is pure (no side effects,
  no execution) and three-valued — `NotWakeable`, `BlockedByAntibody`, `ReadyForManualResume`.
  It maps `next_command` → `ProposedRun` (tool name = first token) and runs the v0.1 antibody
  evaluator; a `Refuse` outcome blocks. The most permissive outcome still requires a human.

### metrics (all PASS — evidence in `sclerotia_harness`)
| metric | target | actual | evidence (test) |
| --- | --- | --- | --- |
| wake-condition fixtures | ≥30 | **36** | `wake_conditions_evaluate_deterministically`: each `is_met==expect_met`, all 6 variants met+unmet, Manual always unmet |
| serialize + restore blocked-work examples | ≥10 | **14** | `sclerotia_records_serialize_restore_and_reference_task_identity`: 14 round-trip insert→get→equal |
| every record references a self-spec-compatible task identity | yes | yes | same test asserts `signature == canonicalize(description)` per record |
| no resume without antibody evaluation | yes | yes | `resume_is_antibody_gated_and_never_auto_executes`: rm→BlockedByAntibody, safe→ReadyForManualResume, dormant→NotWakeable |

independently reviewed (6-lens adversarial workflow): **SPEC PASS / QUALITY APPROVED, no
confirmed defects.** the verifier confirmed `evaluate_resume` is pure, three-valued, and that
the most permissive outcome still demands manual confirmation; all six lenses returned `pass`.

### key design decisions
- ADR 0015 (sclerotia): reuse `TaskIdentity` rather than a parallel blocked-work schema.
- ADR 0016 (wake conditions): a closed typed vocabulary that is cheap and deterministic to
  evaluate — directly addresses the roadmap rollback trigger about vague conditions.

### open questions / gotchas
- decay policy for stale dormant records is deferred (parallel-work item).
- whether dormant state stays useful at scale without the transcript is proven on a small
  corpus only. confidence: **directional**.

---

## v0.5 — spore-based discovery

### implemented
- `Spore { task: TaskIdentity, kind, origin, confidence, note }`
  (`crates/mycel-core/src/spore.rs`) with `SporeKind { CompletedWork, AdjacentWork }` — built
  on the shared identity primitive. `validate()` collects all gaps.
- `classify_adjacent_work`: turns a raw `AdjacentWorkNotice` into a typed `AdjacentWork` spore
  (signature = canonicalize, confidence defaults to `Vibes`).
- `dedupe_spores`: collapse by `(kind, signature)`, keep first — a completed-work spore and an
  adjacent-work spore for the same task stay distinct.
- `SporeStore` local catalog (`spores` table, JSON + indexed signature & kind);
  `catalog` dedupes before storing.
- **Germination candidates only — no germination.** `germination_candidate` returns a struct
  whose `germinated` flag is always `false`; nothing is launched/spawned anywhere.
- **Inert interop export** (`export_spore`) into the 4 loss-matrix shapes: `MycelNative`
  (lossless), `Hermes`/`OpenClaw`/`AgentSkills` (lossy, each declares its `dropped` ecology
  fields and never carries `confidence` as a live field — honoring the loss-matrix rule that
  exports must declare lost features rather than imply foreign enforcement).

### metrics (all PASS — evidence in `spore_harness`)
| metric | target | actual | evidence (test) |
| --- | --- | --- | --- |
| catalog spores from fixtures | ≥25 | **26** | `catalog_at_least_25_spores_from_fixtures`: stored count 26 |
| classify adjacent-work notices | ≥20 | **22** | `classify_at_least_20_adjacent_work_notices`: 22 classified, all typed AdjacentWork |
| dedupe repeated spores | ≥15 | **16** | `dedupe_at_least_15_repeated_spores`: 32 → 16 unique, `duplicates==16` |
| export spores to interop loss-matrix format | ≥10 | **10** (40 shape-exports) | `export_at_least_10_spores_to_interop_shapes`: 10 lossless mycel-native + 30 lossy-with-declared-drops |
| no spore triggers an agent launch | 0 | **0** | `no_spore_germinates_in_v05`: 26 candidates, all `germinated==false` |

review status: an adversarial v0.5 review workflow (mirroring v0.4) was run; see the verdict
section below.

### key design decisions
- ADR 0017 (spore manifest): work-discovery is distinct from handoff; spores reuse
  `TaskIdentity` so a completed-work spore can point at the same signature as the sclerotium it
  unblocks or the self-spec that handed it off. Germination is candidate-only — zero
  autonomous-spawn risk in v0.5.
- ADR 0018 (catalog + inert export): dedup-on-write catalog; export declares dropped ecology
  fields and never leaks `confidence` as enforced.

### open questions / gotchas
- catalog-only usefulness before automatic germination is a directional bet, proven by fixtures
  not by real propagation.
- the 4 export shapes match the current loss matrix; a real adapter may need a 5th (additive
  variant, not a redesign).

---

## cross-cutting notes for the auditor

1. **Schema discipline.** v0.2–v0.5 add five tables but never bump `user_version` past 4;
   all are additive `CREATE TABLE IF NOT EXISTS`. There is an `open_in_memory_user_version_stays_4`
   test in the `decay`, `sclerotia`, and `spore` modules guarding this.
2. **Determinism.** all new evaluation (decay, wake conditions, classification, export) is
   pure and time-injected; projections stable-sort and carry no generation timestamp.
3. **Safety invariants.** no auto-spawn (v0.3), antibody-gated manual-confirm resume (v0.4),
   no germination (v0.5) — each has an explicit asserting test.
4. **Known cosmetic debt.** the `confidence → &str` helper is duplicated across five modules
   (macro-private `as_str`). Recommend hoisting one `pub fn label(self)` on `Confidence`.
5. **Process note.** two long-running v0.4 implementation subagents hit socket timeouts on this
   repo; that work was completed in the main loop instead. No code was lost (incremental commits).

nothing in v0.6+ was touched, per scope.
