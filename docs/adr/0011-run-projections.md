# 11. Run projections and maintenance cycle

## status

accepted

date: 2026-05-30

## context

the decay engine (ADR 0009) transitions run records to `retained`, `distilled`, or `decayed`
states. the ROADMAP specifies a rollback condition: "if projection diffs become noisy enough to
hide semantic changes in review, v0.3 pauses." deterministic, stable projections are therefore
load-bearing — any non-determinism in the rendered markdown makes diffs untrustworthy.

the workspace needs two separate human-readable files:

- `SUBSTRATE.md` — active, usable context: what the agent can draw on right now.
- `COMPOST.md` — post-decay record: what was pruned and what gist was kept.

the split is semantic. substrate is a live index; compost is the audit trail of what got pruned.
mixing them would conflate "context available" with "context history."

## decision

### SUBSTRATE.md (live / retained / preserved)

three sections, each a markdown table `| id | kind | confidence | summary |`:

| section | filter |
| --- | --- |
| `## live` | `decay_state IS NULL` and `no_compost = false` |
| `## retained` | `decay_state = 'retained'` (Solid confidence, survived decay) |
| `## preserved` | `no_compost = true` (kept regardless of tier) |

empty section → `none`. sections are independent predicates, not else-chained.

### COMPOST.md (distilled / decayed)

two sections:

| section | filter | columns |
| --- | --- | --- |
| `## distilled` | `decay_state = 'distilled'` | `id`, `confidence`, `distilled` (the compressed gist) |
| `## decayed` | `decay_state = 'decayed'` | `id`, `confidence` (tombstone only — body intentionally dropped) |

the distilled/decayed split is the visible proof of the decay tiers. distilled rows keep a gist
(the `distilled_summary` column set by the decay engine). decayed rows keep only a tombstone:
the original summary is gone. this is the semantic guarantee. tests assert that the original
summary string of a decayed run does not appear anywhere in `COMPOST.md`.

### determinism

both renderers take `&[Run]` and return `String`. they are pure functions. determinism is
achieved by:

1. stable sort within each section by `(created_at, id)`.
2. no generation timestamp in the body (only the generated-file header comment).
3. pipe (`|`) and newline characters in summary text are escaped before rendering.

tests verify byte-identical output for repeated calls and for the same logical runs inserted in
different `Vec` order.

### maintenance cycle

`run_maintenance(db, workspace_dir, now)` is the single entry point for a scheduled cycle:

1. `DecayEngine::new(db).run(now)` — apply decay transitions.
2. `Substrate::new(db).list()` — fetch runs after decay (decay_state now current).
3. Render both md strings.
4. `fs::create_dir_all(workspace_dir)` + write `SUBSTRATE.md` and `COMPOST.md`.
5. Append a `maintenance` audit event with decay counts: `{ now, retained, distilled, decayed, preserved, skipped_live }`.

the order (decay → list → render → audit) matters: listing before decay would show stale
decay_state values. audit fires last, after files are written, so a partial failure leaves files
without a confusing audit entry.

## consequences

- `mycel maintain --db <path> --workspace <dir> [--now <ts>]` drives the cycle from the CLI.
- `mycel import-promptpressure --db <path> --jsonl <path> [--now <ts>]` feeds PromptPressure
  records into the substrate before a maintenance run.
- `SUBSTRATE.md` and `COMPOST.md` are generated files. the generated-file header prevents
  manual edits from accumulating between runs.
- adding a run section (e.g. a `## distilled-preview` in substrate) is a formatting change only
  and does not require a new ADR unless the filter logic changes.
- `DecayReport` is not `Serialize`; the maintenance audit payload serializes count fields
  individually to avoid coupling the report type to the wire format.
