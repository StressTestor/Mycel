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

substrate edit policy:

- v0.1 is projection-only. humans and agents do not edit canonical projection files as input.
- v0.2 and later plan projection-with-override through a separate override path.

## rationale

four files map to the ecological model without forcing every agent to query SQLite directly.

locking names early makes skills and interop adapters easier to write. **confidence: directional. load-bearing.**

using projections avoids making markdown edits the consistency model. **confidence: directional. load-bearing.**

projection-only v0.1 keeps mutation authority in one place while the substrate format is still proving itself. **confidence: directional. load-bearing.**

## consequences

- agents can read the workspace state through predictable files.
- humans can review substrate state without a database browser.
- generated headers need to state that projection files are not input surfaces in v0.1.
- override design can wait until the projection format has real usage.

## resolved items

- substrate edit policy: projection-only for v0.1.
- planned upgrade path: projection-with-override for v0.2 and later.
