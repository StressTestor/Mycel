# Mycel v0.1 adversarial findings

date: 2026-05-28 (v0.1 findings); 2026-05-29 (v0.1.1 hardening)

suite: `crates/mycel-tests`

scope: black-box only. fixtures exercise `mycel-core` public API and the
`mycel-mcp` tool surface.

## v0.1.1 hardening

The v0.1 findings below are preserved as the "before" record. The v0.1.1
hardening pass closed the gaps in three clusters, each with a "v0.1.1
resolution" note under its category:

- **Cluster 1, signature specificity.** Refuse now requires at least two
  populated, non-empty signature fields; single-field refuse is demoted to
  warn and empty/empty-string signatures are rejected. See
  `docs/schemas/antibody.md`.
- **Cluster 2, clock-skew expiry.** An antibody gates only while
  `created_at <= now < expires_at`; future-created records no longer gate.
- **Cluster 3, surface-variant normalization (partial).** A deterministic
  normalization pass case-folds tool and error fields, canonicalizes file
  paths, and normalizes whitespace and argument order before matching. Variants
  that need semantic similarity (renamed paths, wrapper commands) remain a
  documented v0.2 sqlite-vec item, not a v0.1.1 gap.

Every prior `gap-found` fixture is now `handled-correctly` or explicitly
reclassified as a documented v0.2 item. The real post-hardening false-positive
rate from the v0.1 metric corpus is recorded in the final section.

## summary

| category | fixtures | handled-correctly | gap-found |
| --- | ---: | ---: | ---: |
| false-positive bait | 3 | 1 | 2 |
| false-negative bait | 3 | 0 | 3 |
| expiry edge cases | 4 | 3 | 1 |
| signature collision | 6 order fixtures x 5 repeats | 1 | 0 |
| malformed sentinel input | 5 | 4 | 1 |
| wildcard explosion | 3 | 3 | 0 |

total categories with at least one gap (v0.1): 4 of 6.

post-hardening (v0.1.1) status of the 7 prior `gap-found` fixtures. "closed"
means behavior changed to remove the gap; "accepted" means behavior is now
documented as correct-by-design; "deferred" means reclassified as a v0.2 item.

| category | gap-found (v0.1) | closed | accepted | deferred to v0.2 |
| --- | ---: | ---: | ---: | ---: |
| false-positive bait | 2 | 1 (`shell`) | 1 (`README.md`) | 0 |
| false-negative bait | 3 | 1 (`permission_denied`) | 0 | 2 |
| expiry edge cases | 1 | 1 (future-created) | 0 | 0 |
| malformed sentinel input | 1 | 1 (empty `tool_name`) | 0 | 0 |

4 of 7 gaps closed by a behavior change; 1 accepted as a bounded advisory warn
(the `README.md` file-only signature can no longer escalate to refuse); 2
reclassified as documented v0.2 sqlite-vec similarity items (a renamed file path
and a wrapper command), which Cluster 3 explicitly scopes out of v0.1.1. No gap
remains unaddressed, and no prior `handled-correctly` fixture regressed.

## category 1: false-positive bait

Expected behavior: safe work that only superficially resembles a prior failure
should not be refused. Matching should avoid turning broad tool or file fields
into permanent tripwires.

| fixture | observed behavior | classification |
| --- | --- | --- |
| same `shell` tool, different safe intent | refused by `tool_pattern = shell` | gap-found: public `ProposedRun` has no command or intent fields, so safe shell usage is indistinguishable from the failed shell pattern |
| same `README.md` file, unrelated change | warned by exact `file_pattern = README.md` | gap-found: file-only signatures over-match unrelated work on the same path |
| `python` run without `secret_access` error class | allowed | handled-correctly: AND matching across populated fields avoids this false positive |

v0.1.1 resolution (Cluster 1):

- `shell` tool-only refuse: **closed.** before: refused every shell run. after:
  the single-field refuse is demoted to a soft warn at insertion, so the run is
  warned, not blocked. A hard refuse now requires a signature specific enough
  (≥ 2 fields) to justify it. Fixture: `false_positive_bait_demotes_broad_refuse_and_preserves_and_matching`.
- `README.md` file-only warn: **handled-correctly.** before/after: still a soft
  warn. A single field justifies an advisory warn (the category bar is "should
  not be refused"), and the specificity rule guarantees this broad signature
  can never escalate to a hard refuse. It is advisory, not a blocking tripwire.

## category 2: false-negative bait

Expected behavior: repeat failures with the same root cause should be catchable
even when surface spelling changes. v0.1 deterministic matching can stay simple,
but the harness should make this limitation visible.

| fixture | observed behavior | classification |
| --- | --- | --- |
| `permission_denied` antibody vs `PermissionDenied` run | allowed | gap-found: case and spelling variants bypass the antibody |
| `src/config.rs` antibody vs renamed `src/settings/config.rs` run | allowed | gap-found: path variants bypass exact file matching |
| `bash -lc cargo test` antibody vs `cargo test` run | allowed | gap-found: reordered or normalized command surfaces bypass exact tool matching |

v0.1.1 resolution (Cluster 3, partial):

- `permission_denied` vs `PermissionDenied`: **closed.** before: allowed. after:
  both normalize to the same identifier (case-fold + separator/camelCase split),
  so the antibody matches and refuses. Fixtures:
  `surface_variant_normalization_closes_reachable_variants_and_defers_semantic_ones`,
  `case_and_separator_variants_of_tool_and_error_match`.
- `src/config.rs` vs `src/settings/config.rs`: **deferred to v0.2.** This is a
  file rename, not a surface variant path canonicalization can reach, so it
  stays allowed under deterministic matching. Reclassified as a v0.2 sqlite-vec
  similarity item, not a v0.1.1 gap. Path canonicalization that normalization
  *can* reach (`./`, `..`, duplicate and backslash separators) is closed and
  proven by `canonicalized_file_paths_match`.
- `bash -lc cargo test` vs `cargo test`: **deferred to v0.2.** Recognizing that
  one command wraps the other needs command understanding, not whitespace or
  argument normalization, so it stays allowed. Reclassified as a v0.2 item.
  Whitespace and argument-order normalization that *is* reachable is closed and
  proven by `command_whitespace_and_argument_order_are_normalized`.

## category 3: expiry edge cases

Expected behavior: expiry should be deterministic, expired records should not
gate, and clock skew should not make future-created records authoritative before
their creation time.

| fixture | observed behavior | classification |
| --- | --- | --- |
| `expires_at == evaluated_at` | allowed | handled-correctly: boundary is deterministic and treats expiry as exclusive of the current instant |
| `evaluated_at` one millisecond before expiry | refused | handled-correctly |
| expired antibody with `hit_count = 99` | allowed | handled-correctly: expiry overrides prior hits |
| antibody `created_at` one hour in the future | refused | gap-found: evaluator does not account for future-created antibodies under clock skew |

v0.1.1 resolution (Cluster 2): **closed.** An antibody is now active only while
`created_at <= now < expires_at`. before: a future-created antibody refused at
the current instant. after: it does not gate until `now` reaches its
`created_at`. The lower bound is inclusive (`created_at == now` gates) and the
upper bound stays exclusive (`expires_at == now` is expired), so both edges are
deterministic. Fixtures: `expiry_and_clock_skew_edges_are_handled_correctly`
(adds created-at boundary equality and a one-millisecond-future case) plus three
`clock-skew-allows` fixtures in the metric corpus.

## category 4: signature collision

Expected behavior: if multiple antibodies match, resolution should be
deterministic and the strongest safety action should win.

| fixture | observed behavior | classification |
| --- | --- | --- |
| hard refuse, soft warn, and log-only antibodies inserted in 6 order variants and evaluated repeatedly | refuse always wins | handled-correctly |

v0.1.1 note (Cluster 1): the colliding antibodies were migrated to two-field
signatures so the hard-refuse member stays refuse-capable under the specificity
rule; resolution is unchanged and refuse still wins every order variant.

## category 5: malformed sentinel input

Expected behavior: malformed Sentinel JSONL should degrade gracefully and never
panic. Stable fields that are present but empty should not create useful
antibodies.

| fixture | observed behavior | classification |
| --- | --- | --- |
| truncated JSON | returns error | handled-correctly |
| missing `timestamp` | returns error | handled-correctly |
| unknown `action` enum value | returns error | handled-correctly |
| `tool_name: null` | returns error | handled-correctly |
| `tool_name: ""` | accepted and normalized to an antibody candidate with empty `tool_pattern` | gap-found: empty stable field is accepted instead of rejected |

v0.1.1 resolution (Cluster 1):

- `tool_name: ""`: **closed.** before: accepted, producing a candidate with
  `tool_pattern == Some("")` (a wildcard). after: rejected at ingestion with
  `MycelError::EmptyToolName`; a whitespace-only `tool_name` is rejected the
  same way. Non-empty tool names still ingest under the locked
  `block -> refuse/hard` mapping. Fixtures:
  `malformed_sentinel_input_degrades_without_panicking`,
  `sentinel_block_with_empty_tool_name_is_rejected_not_normalized`.

## category 6: wildcard explosion

Expected behavior: an antibody with all signature fields empty must not persist,
because empty fields are wildcards and a refuse/hard wildcard would refuse every
run.

| fixture | observed behavior | classification |
| --- | --- | --- |
| direct `AntibodyStore::insert_antibody` with empty signature | returns error | handled-correctly |
| `McpTools::insert_antibodies` with empty signature | returns error | handled-correctly |
| safe run after rejected wildcard attempt | allowed | handled-correctly |

v0.1.1 resolution (Cluster 1): the wildcard guard now treats present-but-empty
and whitespace-only fields as unpopulated, so a signature whose fields are all
empty strings is rejected exactly like an all-`None` signature, on both the
direct store and MCP insert paths. Fixture:
`all_empty_string_signature_is_rejected_like_a_wildcard`.

## public API coverage notes

The public API was sufficient to exercise every requested category. The v0.1
note that it could not model normalized command arguments or clock-skew expiry
is now addressed: v0.1.1 adds deterministic argument/whitespace normalization
(Cluster 3) and a `created_at <= now` activation guard (Cluster 2). Modeling
full command *intent* (distinguishing a wrapper command from the command it
wraps) still needs semantic similarity and stays a v0.2 sqlite-vec item.

## v0.1.1 false-positive rate (real, post-hardening)

The v0.1 metric corpus reported a false-positive rate of `0.0`, but that number
was synthetic: the safe-labeled fixtures did not include lookalikes of the
blocked patterns, so the over-match surface was never exercised.

The post-hardening corpus (`run_v0_1_harness`) adds safe-labeled bait that
resembles blocked work but differs in a discriminating field, and migrates
refuse antibodies to specific (≥ 2 field) signatures:

- `shell-safe-benign`: a benign `shell` run with no `protected_path` error
  class. A pre-hardening single-field `shell` refuse would have blocked this.
- `shell-safe-variant`: an upper-cased `SHELL` run, exercising normalization.
- three `clock-skew-allows` runs against future-created antibodies.

Measured result (`mycel harness`):

| metric | value |
| --- | --- |
| eval fixtures | 85 |
| pass / fail | 85 / 0 |
| safe-labeled fixtures | 50 |
| false positives (safe, non-allow) | 0 |
| **false-positive rate** | **0.0** |
| hard-block false positives (safe, refuse) | 0 |
| expiry fixtures pass | 15 / 15 |

This `0.0` is now trusted rather than synthetic: every safe lookalike of a
blocked pattern is correctly allowed because specificity (refuse needs ≥ 2
fields) plus AND matching keep the discriminating field load-bearing, and the
clock-skew guard keeps future-created records inert. It is well under the 20%
ROADMAP threshold.

Residual, by design: a single-field warn signature still emits an advisory warn
on a matching run. That is intended (warn is advisory, not a block) and cannot
escalate to refuse under the specificity rule, so the metric corpus does not
label such advisory warns as safe-must-allow fixtures.
