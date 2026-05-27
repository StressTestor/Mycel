# 0002: workspace convention

status: proposed

date: 2026-05-27

## context

mycel needs a stable local workspace convention so agents can orient themselves without knowing the internal database schema. **confidence: directional. load-bearing.**

the convention should be small enough to remember and stable enough for skills, adapters, and external tools. **confidence: directional. load-bearing.**

## decision

lock four canonical workspace files:

| file | role |
| --- | --- |
| `SUBSTRATE.md` | current substrate summary, active conditions, durable findings |
| `SPORES.md` | emitted manifests and germination candidates |
| `COMPOST.md` | decayed findings, distillations, pruned context notes |
| `MYCELIUM.md` | kin graph, live threads, dormant sclerotia, bequests |

these files are generated projections from the substrate store.

## rationale

four files map to the ecological model without forcing every agent to query SQLite directly.

locking names early makes skills and interop adapters easier to write. **confidence: directional. load-bearing.**

using projections avoids making markdown edits the consistency model. **confidence: directional. load-bearing.**

## consequences

- agents can read the workspace state through predictable files.
- humans can review substrate state without a database browser.
- manual edits need a clear policy because projections can overwrite them. **confidence: directional. load-bearing.**
- every file needs a generated header that explains source of truth and edit policy.

## unresolved

- whether to allow a fifth `MYCEL_OVERRIDES.md` for human edits.
- how much detail belongs in `SUBSTRATE.md` before it becomes noise.
- whether dormant sclerotia deserve their own file after v0.3.
