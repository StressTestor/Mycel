# mycel

personal agent harness for coding, built around substrate ecology.

mycel treats agent runs as living work units inside a local substrate. the first job is substrate memory: remember failures, preserve durable findings, hibernate blocked work, and transfer useful context when an agent dies.

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## language recommendation

recommendation: **rust core, with thin python and typescript adapters**. **confidence: directional. load-bearing.**

why:

| option | read |
| --- | --- |
| rust | best fit for a local-first harness that writes durable state, evaluates spawn rules, and shares antibody records with Sentinel. **confidence: directional. load-bearing.** |
| python | best fit for agent experiments and Hermes interop, but weaker as the long-term substrate runtime. **confidence: directional. load-bearing.** |
| typescript | best fit for OpenClaw plugin interop and editor-adjacent tooling, but less aligned with Sentinel. **confidence: directional. load-bearing.** |
| hybrid | best fit if the core stays small and adapters stay dumb. **confidence: directional. load-bearing.** |

initial runtime shape:

- rust owns substrate storage, antibody matching, condition evaluation, and file projection. **confidence: directional. load-bearing.**
- python adapters export and import Hermes-compatible skills and run optional eval tooling. **confidence: directional.**
- typescript adapters export and import OpenClaw-compatible plugin and skill metadata. **confidence: directional.**
- no source directories exist yet. this repo starts with design docs only. **confidence: solid. load-bearing.**

## v0.1 pick

v0.1 should ship **fail-pattern immunity** first. **confidence: directional. load-bearing.**

the reason is boring in the useful way: it proves the substrate has memory, policy, and enforcement without requiring full autonomous spawning. it also pairs directly with Sentinel. Sentinel block logs can become antibody candidates, and Mycel antibodies can later become Sentinel rules. **confidence: directional. load-bearing.**

the tradeoff: immunity is less visually magical than spore discovery or sclerotia. the win is that it creates a hard measurable loop: failed run, antibody record, future refusal or warning. **confidence: directional.**

## repo structure proposal

this repository starts flat and document-first:

```text
mycel/
  ARCHITECTURE.md
  README.md
  ROADMAP.md
  CONTRIBUTING.md
  LICENSE
  .gitignore
  docs/
    adr/
      0001-substrate-format.md
      0002-workspace-convention.md
      0003-language-and-runtime.md
      0004-skill-interop.md
      0005-license.md
    open-questions.md
```

future source layout, not created yet:

```text
crates/
  mycel-core/          substrate, antibodies, wake rules
  mycel-cli/           local command surface
adapters/
  hermes/              python skill import/export
  openclaw/            typescript plugin and skill import/export
schemas/               json schema for spores, antibodies, sclerotia
examples/              small local workspaces
```

justification:

- keep v0 design reviewable before code appears. **confidence: solid. load-bearing.**
- separate the rust core from adapter languages so interop does not infect the substrate model. **confidence: directional. load-bearing.**
- put schemas at the repo root once formats stabilize, because spores and antibodies are public-ish contracts. **confidence: directional.**

## mechanisms

| mechanism | v1 role | confidence |
| --- | --- | --- |
| decay-pruned context | ttl-tiered context compaction on schedule, driven by solid, directional, and vibes confidence tags | **directional. load-bearing.** |
| spore-based plugin discovery | finished agents emit typed manifests of completed work and adjacent opportunities | **directional.** |
| self-spec on death | terminating agents write the next agent spec before exit | **directional.** |
| substrate-conditioned spawning | agents start when typed environmental tuples match | **directional. load-bearing.** |
| fail-pattern immunity | failed signatures become antibody records that refuse or pre-flag similar runs | **directional. load-bearing.** |
| sclerotia | blocked agents serialize work-in-progress and wake conditions | **directional. load-bearing.** |
| mycorrhizal kin-sharing | terminating agents bequeath useful context to related live or dormant work | **directional.** |

post-v1:

- lifestyle classification: parasite, saprophyte, symbiote. **confidence: vibes.**
- mycoheterotroph detection: identify freeloader patterns that consume context without contributing useful substrate. **confidence: vibes.**
- distribution layer: share selected spores, skills, and antibodies across machines or users. **confidence: vibes.**

## workspace files

mycel workspaces should expose four canonical human files:

| file | role | confidence |
| --- | --- | --- |
| `SUBSTRATE.md` | current substrate summary, active conditions, durable findings | **directional. load-bearing.** |
| `SPORES.md` | emitted manifests and germination candidates | **directional.** |
| `COMPOST.md` | decayed findings, distillations, and pruned context notes | **directional.** |
| `MYCELIUM.md` | kin graph, live threads, dormant sclerotia, resource transfers | **directional. load-bearing.** |

these files should be projections from the local substrate store. the database stays the source of truth. **confidence: directional. load-bearing.**

## getting started shape

this is hypothetical until implementation begins:

```sh
mycel init
mycel antibody ingest --from sentinel
mycel run --task "repair failing tests"
mycel substrate maintain
```

expected v0.1 workflow:

1. initialize a local workspace substrate. **confidence: directional.**
2. ingest Sentinel block logs or failed run records. **confidence: directional.**
3. normalize them into typed antibody records. **confidence: directional.**
4. evaluate a proposed run against the antibody registry. **confidence: directional.**
5. refuse, warn, or allow with attached context. **confidence: directional.**

## positioning

OpenClaw is a useful reference for a typed context-engine lifecycle and native plugin manifests. its current context engine interface includes bootstrap, ingest, after-turn, assemble, compact, maintain, and subagent lifecycle hooks. **confidence: solid. source-checked 2026-05-27.**

Hermes Agent is a useful reference for a pluggable context engine, overflow or threshold-triggered compression, background review after turns, and active/stale/archive skill curation. **confidence: solid. source-checked 2026-05-27.**

mycel's wedge is ecological substrate management: immune memory, dormancy, and kin-aware death transfer. **confidence: directional. load-bearing.**

## references checked

- [OpenClaw context engine types](https://raw.githubusercontent.com/openclaw/openclaw/main/src/context-engine/types.ts)
- [OpenClaw plugin manifest docs](https://raw.githubusercontent.com/openclaw/openclaw/main/docs/plugins/manifest.md)
- [OpenClaw commitments types](https://raw.githubusercontent.com/openclaw/openclaw/main/src/commitments/types.ts)
- [Hermes context engine](https://raw.githubusercontent.com/NousResearch/hermes-agent/main/agent/context_engine.py)
- [Hermes context compressor](https://raw.githubusercontent.com/NousResearch/hermes-agent/main/agent/context_compressor.py)
- [Hermes background review](https://raw.githubusercontent.com/NousResearch/hermes-agent/main/agent/background_review.py)
- [Hermes curator](https://raw.githubusercontent.com/NousResearch/hermes-agent/main/agent/curator.py)
