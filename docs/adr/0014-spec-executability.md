# 14. Spec executability bar

## status

accepted

date: 2026-05-30

confidence: directional. load-bearing.

## context

ADR 0013 defined the `SelfSpec` schema and `validate()` — a structural well-formedness check.
`validate()` answers "is this spec formed correctly?" but not "can a fresh agent act on this
spec without reading the originating transcript?"

the v0.3 roadmap metric is: "at least 10 handoff specs can be reviewed and executed manually
WITHOUT reading the prior full transcript." this is a self-sufficiency bar. a spec that passes
`validate()` may still be useless for a cold-start agent if it has no preconditions (agent
cannot know starting state), a vague success criterion (agent cannot know when done), no sourced
context (agent has no verifiable anchor facts), or no refusal risks (agent cannot know which
guardrails apply).

the executability bar closes that gap deterministically. it does not replace the qualitative
blind-reviewer pass — it is the floor the harness can check mechanically.

## decision

### ExecutabilityGap enum

```rust
pub enum ExecutabilityGap {
    FailsValidation,
    NoPrecondition,
    NoActionableSuccessCriterion,
    NoSourcedContext,
    NoRefusalRisk,
}
```

`SelfSpec::executability_gaps()` returns a `Vec<ExecutabilityGap>` — all applicable gaps,
collected independently (not fail-fast). `SelfSpec::is_executable()` returns `gaps.is_empty()`.

### gap rules (exact, deterministic)

each gap is checked independently. a single spec may have multiple gaps.

| gap | condition | why it blocks cold-start |
| --- | --- | --- |
| `FailsValidation` | `self.validate().is_err()` | a malformed spec cannot be trusted — the agent has no reliable task identity or field guarantees |
| `NoPrecondition` | `self.preconditions.is_empty()` | a fresh agent has no way to know the required starting state (branch, file, env) before beginning work |
| `NoRefusalRisk` | `self.refusal_risks.is_empty()` | a fresh agent has no signal about which guardrails or constraints apply — it will violate them by accident |
| `NoSourcedContext` | no `inherited_context` item with `!source.trim().is_empty()` | unsourced claims are unverifiable; the agent has no anchor facts it can cross-check |
| `NoActionableSuccessCriterion` | no `success_criteria` entry is concrete (see heuristic below) | a fresh agent cannot determine when the task is complete; vague criteria ("make it better") do not terminate |

### concreteness heuristic (deterministic)

a success-criterion string is **concrete** iff ALL of the following hold:

1. `trimmed.chars().count() >= 12` (minimum meaningful length — rules out fragments)
2. it contains at least one **concreteness signal**:
   - an ASCII digit (`'0'`–`'9'`), OR
   - a literal `/` character, OR
   - a literal backtick (`` ` ``), OR
   - the substring `.rs`, OR
   - (case-insensitive substring match) one of the verb keywords:
     `pass`, `passes`, `return`, `returns`, `exit`, `commit`, `render`, `insert`,
     `write`, `emit`, `match`, `matches`, `equal`, `equals`, `assert`, `print`,
     `prints`, `contains`

the rule is intentionally mechanical. it is not an LLM call. rationale for each signal:

- **digit**: references an exit code, count, version number, line number — all concrete.
- **`/`**: references a file path or URL — concrete and verifiable.
- **backtick**: a quoted command or code snippet — actionable.
- **`.rs`**: references a Rust source file by name — directly navigable.
- **verb keywords**: action verbs that describe a testable outcome ("passes", "returns",
  "contains", etc.) rather than a state description. case-insensitive so "Pass" and "PASS"
  both count.

examples:
- `"make it better"` — 14 chars, no signal → **not concrete** (NoActionableSuccessCriterion).
- `` "`cargo test -p mycel-cli` passes with exit code 0" `` — backtick + digit + keyword → **concrete**.
- `"crates/mycel-cli/src/main.rs compiles"` — `/` + `.rs` → **concrete**.
- `"the implementation looks good to reviewers"` — 43 chars, no digit/slash/backtick/.rs/keyword → **not concrete**.

### corpus and harness

`crates/mycel-tests/tests/fixtures/executable_specs.jsonl` contains ≥12 self-contained
handoff specs for realistic Mycel-flavored coding tasks. each must pass
`SelfSpec::is_executable()`.

`crates/mycel-tests/tests/executable_specs_harness.rs` asserts:
- corpus size ≥ 10
- every spec is executable (panics with the gap list on first failure)
- all signatures are unique
- prints a summary table: index, signature, precondition count, criterion count,
  context count, executable (y/n)

### metric evidence (two layers)

the v0.3 roadmap metric ("≥10 handoff specs executable without the prior transcript") is
evidenced two ways:

1. **this deterministic harness** (`executable_specs_harness`): confirms every fixture clears
   the mechanical floor. run with `cargo test -p mycel-tests --test executable_specs_harness`.

2. **out-of-band blind-reviewer pass**: the orchestrator gives only the spec (not the
   transcript) to a fresh agent and records whether that agent can name the next concrete
   action. evidence recorded in `docs/v0.3-blind-review-evidence.md`.

the harness is necessary but not sufficient. a spec can pass the heuristic while still being
too thin for a cold-start agent. the blind-reviewer pass catches that. both must pass for the
metric to count.

## consequences

- `ExecutabilityGap` is exported from `mycel-core` alongside `SpecValidationError`.
- `executability_gaps()` calls `validate()` internally — callers do not need to call both.
- the concreteness heuristic is frozen at this keyword set for v0.3. additions require
  updating this ADR and all affected fixtures.
- v0.4 sclerotia: may check `is_executable()` before promoting a spec to a dormant-work
  record — only self-sufficient specs become actionable sclerotia. **confidence: directional.**
- the heuristic produces false positives (a criterion with a digit that is still vague) and
  false negatives are unlikely (the keyword list is broad). false positives are caught by the
  blind-reviewer pass, which is why both layers are required.
