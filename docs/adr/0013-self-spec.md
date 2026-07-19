# 13. Self-spec on death

## status

accepted

date: 2026-05-30

## context

agent tasks frequently die mid-run: context fills, the session ends, or a panic kills the
process. the agent's in-flight knowledge of what it was doing, what the preconditions were, and
what risks it had already reasoned about disappears with it. a resuming agent (human or AI) has
to reconstruct that context from scratch.

the spec-on-death pattern addresses this: before context is lost, an author (human or agent)
writes a structured record — a "self-spec" — capturing the task's intent, preconditions,
success criteria, inherited reasoning, and known refusal risks. the spec is inserted into the
substrate so it survives the session.

## decision

### SelfSpec schema

```rust
pub struct SelfSpec {
    pub task: TaskIdentity,                   // description + dedupe signature (ADR 0012)
    pub preconditions: Vec<String>,           // what must be true before work starts
    pub success_criteria: Vec<String>,        // what "done" looks like
    pub inherited_context: Vec<InheritedContext>,  // confidence-tagged facts carried forward
    pub refusal_risks: Vec<String>,           // known triggers that might block execution
}
```

`TaskIdentity` and `InheritedContext` are defined in ADR 0012.

### manual authoring only (v0.3)

specs are NEVER auto-executed or auto-spawned in v0.3. a spec is authored manually — by a
human or agent — and inserted via `SpecStore::insert`. the store validates before writing.
no background task, no hook, no trigger. v0.4 may add automated sclerotia creation from specs;
v0.5 may propagate spec identity across spawn. those are out of scope here.

### persistence

specs are stored in the `specs` table:

```sql
CREATE TABLE IF NOT EXISTS specs (
    id          TEXT    PRIMARY KEY NOT NULL,  -- uuid v4 as string
    signature   TEXT    NOT NULL,             -- task.signature (indexed for dedupe lookups)
    spec_json   TEXT    NOT NULL,             -- full SelfSpec as JSON blob
    created_at  INTEGER NOT NULL              -- unix timestamp, injected by caller (ADR 0008)
);
CREATE INDEX IF NOT EXISTS idx_specs_signature ON specs(signature);
```

the table is additive DDL: `CREATE TABLE IF NOT EXISTS` — idempotent on existing databases.
no schema version bump: the table is backwards-compatible and `user_version` stays at 4.

`SpecStore<'a>` borrows `&'a Db` and provides:

- `insert(spec, now) -> Result<String>` — validates first; on invalid, returns
  `Err(MycelError::InvalidSpec(String))` summarizing the gaps. on valid, serializes spec to
  JSON, generates a uuid id, writes the row, returns the id.
- `get_by_signature(signature) -> Result<Vec<SelfSpec>>` — returns all specs with matching
  signature, deserialized from JSON.
- `list() -> Result<Vec<SelfSpec>>` — returns all specs ordered by `(created_at, id)`.

`MycelError::InvalidSpec(String)` is added to `MycelError`. the string summarizes which
validation errors were collected (joined with `; `). this is a new variant, distinct from the
antibody-specific `EmptySignature` variant, to keep error semantics unambiguous.

### validation (collect-all, not fail-fast)

`SelfSpec::validate() -> Result<(), Vec<SpecValidationError>>` collects ALL applicable errors.
`Ok(())` only if the spec is fully valid. errors:

| variant | condition |
| --- | --- |
| `EmptyDescription` | `task.description.trim().is_empty()` |
| `EmptySignature` | `task.signature.is_empty()` |
| `MissingPreconditions` | `preconditions.is_empty()` |
| `MissingSuccessCriteria` | `success_criteria.is_empty()` |
| `MissingInheritedContext` | `inherited_context.is_empty()` |
| `MissingRefusalRisks` | `refusal_risks.is_empty()` |
| `InheritedContextMissingSource` | any `inherited_context` item where `source.trim().is_empty()` |

`InheritedContextMissingSource` is raised at most once per validate call, even if multiple
items have empty sources. `EmptyDescription` and `EmptySignature` are independent — both may
fire simultaneously (e.g. a zeroed struct).

### dedupe by signature

`dedupe_specs(specs: Vec<SelfSpec>) -> (Vec<SelfSpec>, usize)`:

- iterate in input order, tracking seen signatures in an `IndexSet` or `HashSet`.
- keep the FIRST occurrence of each signature (stable).
- count duplicates (total input length minus unique length).
- return `(unique_specs, duplicate_count)`.

no sorting. no mutation. a spec with a signature seen before is a duplicate regardless of
whether the description differs.

### metric: ≥15 near-duplicate specs collapsed (v0.3 roadmap gate)

the fixture corpus (`selfspec_dupes.jsonl`) must contain enough near-duplicate pairs that
`dedupe_specs` removes ≥15 inputs. near-duplicates are specs whose descriptions differ only
in ways `canonicalize` normalizes away (case, whitespace, trailing punctuation).

## consequences

- specs are immutable once inserted. no update path in v0.3.
- dedupe is at read/import time (caller passes a `Vec<SelfSpec>` to `dedupe_specs`), not at
  write time. the store allows multiple specs with the same signature — deduplication is a
  separate concern applied by callers who need it (e.g. the import harness).
- v0.4 sclerotia: may query `specs` by signature to hydrate a dormant-work record's task
  header. **confidence: directional.**
- v0.5 spores: may use `spec:<signature>` source pointers to link a spawned task back to its
  parent spec. **confidence: directional.**
- `user_version` stays at 4. if v0.4 or v0.5 require a new version, they own that bump.
