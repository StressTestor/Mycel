# 12. Shared task identity

## status

accepted

date: 2026-05-30

confidence: directional. load-bearing.

## context

v0.3 introduces self-spec on death: a structured record of what a task was supposed to do,
authored before context is lost. the spec has a task header (description + a stable dedupe key)
and inherited context annotated with confidence and provenance.

v0.4 (sclerotia) records dormant work. v0.4 can reuse the same task header fields instead of
inventing a parallel blocked-work schema. v0.5 (spores) propagates task identity across spawn
boundaries. sharing the same identity type eliminates duplicated schema at each tier.

the design question: what is the minimal shared type that v0.3, v0.4, and v0.5 all need?

## decision

### TaskIdentity

```rust
pub struct TaskIdentity {
    pub description: String,  // human-readable, original-case, original whitespace
    pub signature: String,    // deterministic dedupe key (see canonicalization below)
}
```

`TaskIdentity::new(description)` constructs a value; `TaskIdentity::canonicalize(description)`
produces the signature deterministically.

**canonicalization rule** — applied in this exact order:

1. lowercase the entire string
2. split on whitespace and rejoin with a single space (trims ends + collapses internal runs)
3. strip a run of trailing punctuation characters from the set `.!?,;:` using
   `trim_end_matches(|c: char| ".!?,;:".contains(c))`
4. replace every remaining space with `-`

two descriptions that differ only in case, whitespace, or trailing punctuation from the defined
set MUST produce the same signature. examples:

| input | signature |
| --- | --- |
| `"Fix the bug."` | `"fix-the-bug"` |
| `"  fix   the BUG  "` | `"fix-the-bug"` |
| `"Fix the bug"` | `"fix-the-bug"` |
| `"Refactor the auth module"` | `"refactor-the-auth-module"` |
| `"refactor the auth module!"` | `"refactor-the-auth-module"` |

### source pointer format

a source pointer is a plain `String` identifying where a piece of context came from. the format is:

| prefix | meaning | example |
| --- | --- | --- |
| `run:<id>` | references a run record by id | `"run:abc-123"` |
| `audit:<id>` | references an audit log entry by id | `"audit:def-456"` |
| `spec:<signature>` | references another spec by its signature | `"spec:fix-the-bug"` |
| `note:<text>` | free-form provenance note | `"note:from pair session"` |

the prefix is validated only by convention; v0.3 does not parse or resolve pointers at runtime.
v0.4 and v0.5 inherit the same format and MAY add resolution logic without breaking v0.3 data.

### confidence-tagged inherited context

each inherited context item carries:

```rust
pub struct InheritedContext {
    pub claim: String,       // the fact or belief being carried forward
    pub confidence: Confidence,  // reuses the existing Confidence enum (solid | directional | vibes)
    pub source: String,      // source pointer (see above)
}
```

`Confidence` is the existing crate type from `mycel_core`. no new enum is introduced.

### crate-root re-export

`TaskIdentity`, `SelfSpec`, `InheritedContext`, `SpecStore`, `SpecValidationError`, and
`dedupe_specs` are re-exported at the `mycel_core` crate root. v0.4 sclerotia and v0.5 spores
import `mycel_core::TaskIdentity` directly — not via the `selfspec` submodule path. this is
what makes TaskIdentity a genuine shared primitive rather than an internal detail.

## consequences

- v0.4 sclerotia: uses `TaskIdentity` as the header for dormant-work records. no new identity
  schema needed. **confidence: directional.**
- v0.5 spores: uses `TaskIdentity` as the identity token passed across spawn boundaries.
  `signature` is the stable key for deduplicating related specs across runs.
  **confidence: directional.**
- the dedupe key is pure text normalization — no hashing, no external state. any environment
  can reproduce it from the raw description string. this is intentional for auditability.
- descriptions with trailing punctuation attached to the last word normalize correctly.
  descriptions with trailing whitespace before punctuation (e.g. `"fix . "`) may produce a
  trailing `-` after step 4; authors should avoid that shape. the ADR-defined 5-step rule is
  the contract; step changes require a new ADR.
- source pointers are unvalidated strings in v0.3. parseable/resolvable in v0.4+.
