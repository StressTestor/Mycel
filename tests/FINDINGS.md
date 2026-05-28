# Mycel v0.1 adversarial findings

date: 2026-05-28

suite: `crates/mycel-tests`

scope: black-box only. fixtures exercise `mycel-core` public API and the
`mycel-mcp` tool surface. `mycel-core` internals were not changed.

## summary

| category | fixtures | handled-correctly | gap-found |
| --- | ---: | ---: | ---: |
| false-positive bait | 3 | 1 | 2 |
| false-negative bait | 3 | 0 | 3 |
| expiry edge cases | 4 | 3 | 1 |
| signature collision | 6 order fixtures x 5 repeats | 1 | 0 |
| malformed sentinel input | 5 | 4 | 1 |
| wildcard explosion | 3 | 3 | 0 |

total categories with at least one gap: 4 of 6.

## category 1: false-positive bait

Expected behavior: safe work that only superficially resembles a prior failure
should not be refused. Matching should avoid turning broad tool or file fields
into permanent tripwires.

| fixture | observed behavior | classification |
| --- | --- | --- |
| same `shell` tool, different safe intent | refused by `tool_pattern = shell` | gap-found: public `ProposedRun` has no command or intent fields, so safe shell usage is indistinguishable from the failed shell pattern |
| same `README.md` file, unrelated change | warned by exact `file_pattern = README.md` | gap-found: file-only signatures over-match unrelated work on the same path |
| `python` run without `secret_access` error class | allowed | handled-correctly: AND matching across populated fields avoids this false positive |

## category 2: false-negative bait

Expected behavior: repeat failures with the same root cause should be catchable
even when surface spelling changes. v0.1 deterministic matching can stay simple,
but the harness should make this limitation visible.

| fixture | observed behavior | classification |
| --- | --- | --- |
| `permission_denied` antibody vs `PermissionDenied` run | allowed | gap-found: case and spelling variants bypass the antibody |
| `src/config.rs` antibody vs renamed `src/settings/config.rs` run | allowed | gap-found: path variants bypass exact file matching |
| `bash -lc cargo test` antibody vs `cargo test` run | allowed | gap-found: reordered or normalized command surfaces bypass exact tool matching |

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

## category 4: signature collision

Expected behavior: if multiple antibodies match, resolution should be
deterministic and the strongest safety action should win.

| fixture | observed behavior | classification |
| --- | --- | --- |
| hard refuse, soft warn, and log-only antibodies inserted in 6 order variants and evaluated repeatedly | refuse always wins | handled-correctly |

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

## category 6: wildcard explosion

Expected behavior: an antibody with all signature fields empty must not persist,
because empty fields are wildcards and a refuse/hard wildcard would refuse every
run.

| fixture | observed behavior | classification |
| --- | --- | --- |
| direct `AntibodyStore::insert_antibody` with empty signature | returns error | handled-correctly |
| `McpTools::insert_antibodies` with empty signature | returns error | handled-correctly |
| safe run after rejected wildcard attempt | allowed | handled-correctly |

## public API coverage notes

The public API was sufficient to exercise every requested category. It was not
sufficient to model command intent, normalized command arguments, or true
mid-evaluation expiry. Those limits are part of the observed findings rather
than blockers for the suite.
