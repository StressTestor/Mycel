# mycel

personal agent harness for coding, built around substrate ecology.

mycel treats agent runs as living work units inside a local substrate. the first job is substrate memory: remember failures, preserve durable findings, hibernate blocked work, and transfer useful context when an agent dies.

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## what it is now

two halves in one repo.

- `crates/` is the rust brain: substrate, antibodies, the evaluation engine, sentinel. this is where policy and memory live.
- `harness/` is the agent body: a forked, de-vendored agent CLI (grafted from kimi-code, MIT, ADR-0006). it runs sessions, drives whatever model you configure, executes tools.

they meet at one hard edge. every shell command the agent wants to run goes through `mycel-gate` as a fail-closed `PreToolUse` hook. a command that matches an active antibody is blocked with a remediation string. if the gate crashes, times out, or the substrate db is missing, the command is blocked, not allowed. a deleted db reads as a disarmed guard, not a fresh start.

model-agnostic on purpose. no default model, no telemetry, no update pings, no marketplace phone-home. bring kimi, anthropic, a local ollama, gemini - config picks all of it.

## install

```sh
bash install.sh
```

builds the rust binaries and the harness, installs `mycel` + `mycel-gate` + `mycel-substrate` + `mycel-mcp-server` into `~/.mycel/bin`, scaffolds a config, and verifies the gate before it calls itself done. every step is loud and every failure names the fix. then edit `~/.mycel/config.toml`, pick a provider, set `default_model`, and run `mycel`.

seed the immunity gate:

```sh
mycel-substrate antibody-add --db ~/.mycel/substrate/mycel.db \
  --command-pattern "rm -rf /" \
  --remediation "no." --severity refuse --refusal-mode hard
```

## language recommendation

recommendation: **rust core, with thin python and typescript adapters**.

why:

| option | read |
| --- | --- |
| rust | best fit for local state, typed policy, and Sentinel pairing. rust should reduce runtime ambiguity in the substrate core. **confidence: directional. load-bearing.** |
| python | best fit for Hermes interop and eval experiments. weaker as the long-term policy runtime. **confidence: directional. load-bearing.** |
| typescript | best fit for OpenClaw manifest work and editor-adjacent tooling. less aligned with Sentinel. **confidence: directional. load-bearing.** |
| hybrid | best fit if the core stays small and adapters stay schema-driven. **confidence: directional. load-bearing.** |

initial runtime shape:

- rust owns substrate storage, antibody matching, condition evaluation, and file projection.
- sentinel is a core runtime-defense subsystem inside the future cargo workspace.
- python adapters export and import Hermes-compatible skills and run optional eval tooling.
- typescript adapters export and import OpenClaw-compatible plugin and skill metadata.
- no source directories exist yet.

## v0.1 pick

v0.1 ships **fail-pattern immunity** first.

the reason is boring in the useful way: it proves the substrate has memory, policy, and enforcement before autonomous spawning. it also pairs directly with Sentinel. Sentinel block logs can become antibody candidates, and Mycel antibodies can later become Sentinel rules. **confidence: directional. load-bearing.**

v0.1 also drafts self-spec schema and makes the interop decision early. early interop design should reduce schema churn once Hermes and OpenClaw export losses are visible. **confidence: directional. load-bearing.**

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

future source layout, still uncreated:

```text
crates/
  mycel-core/        substrate, antibodies, wake rules
  mycel-mcp/         MCP server, canonical interface
  mycel-cli/         local command surface (built on MCP tool surface)
  sentinel-guard/    workspace member, also published independently
adapters/
  hermes/            python skill import/export
  openclaw/          typescript plugin and skill import/export
schemas/             json schema for spores, antibodies, sclerotia
examples/            small local workspaces
docs/
  adr/               architectural decision records
  schemas/           schema appendix
```

why:

- keeping v0 document-first lowers early churn while decisions are still moving. **confidence: directional. load-bearing.**
- separating the rust core from adapter languages should keep interop from shaping the substrate model too early. **confidence: directional. load-bearing.**
- schemas likely belong at repo root once formats stabilize because spores and antibodies become public-ish contracts. **confidence: directional.**

## mechanisms

| mechanism | v1 role |
| --- | --- |
| fail-pattern immunity | failed signatures become antibody records that refuse or pre-flag similar runs |
| decay-pruned context | ttl-tiered context compaction on schedule, driven by solid, directional, and vibes tiers |
| self-spec on death | terminating agents write the next agent spec before exit |
| sclerotia | blocked agents serialize work-in-progress with wake conditions |
| spore-based plugin discovery | finished agents emit typed manifests of completed work and adjacent opportunities |
| mycorrhizal kin-sharing | terminating agents bequeath useful context to related live or dormant work |
| substrate-conditioned spawning | agents start when typed environmental tuples match |

post-v1:

- lifestyle classification: parasite, saprophyte, symbiote. **confidence: vibes.**
- mycoheterotroph detection: identify freeloader patterns that consume context without contributing useful substrate. **confidence: vibes.**
- distribution layer: share selected spores, skills, and antibodies across machines or users. **confidence: vibes.**

## workspace files

mycel workspaces expose four canonical human files:

| file | role |
| --- | --- |
| `SUBSTRATE.md` | current substrate summary, active conditions, durable findings |
| `SPORES.md` | emitted manifests and germination candidates |
| `COMPOST.md` | decayed findings, distillations, and pruned context notes |
| `MYCELIUM.md` | kin graph, live threads, dormant sclerotia, resource transfers |

these files should be projections from the local substrate store. the database stays the source of truth. **confidence: directional. load-bearing.**

## getting started shape

hypothetical until implementation begins:

```sh
mycel init
mycel antibody ingest --from sentinel
mycel run --task "repair failing tests"
mycel substrate maintain
```

expected v0.1 workflow:

1. initialize a local workspace substrate.
2. ingest Sentinel block logs or failed run records.
3. normalize them into typed antibody records.
4. evaluate a proposed run against the antibody registry.
5. refuse, warn, or allow with attached context.

## positioning

OpenClaw is a useful reference for a typed context-engine lifecycle and native plugin manifests. its current context engine interface includes bootstrap, ingest, after-turn, assemble, compact, maintain, and subagent lifecycle hooks. **confidence: solid. source-checked 2026-05-27.**

Hermes Agent is a useful reference for a pluggable context engine, threshold-triggered compression, background review after turns, and active/stale/archive skill curation. **confidence: solid. source-checked 2026-05-27.**

mycel's wedge is ecological substrate management: immune memory, dormancy, and kin-aware death transfer.

## references checked

- [OpenClaw context engine types](https://raw.githubusercontent.com/openclaw/openclaw/main/src/context-engine/types.ts)
- [OpenClaw plugin manifest docs](https://raw.githubusercontent.com/openclaw/openclaw/main/docs/plugins/manifest.md)
- [OpenClaw commitments types](https://raw.githubusercontent.com/openclaw/openclaw/main/src/commitments/types.ts)
- [Hermes context engine](https://raw.githubusercontent.com/NousResearch/hermes-agent/main/agent/context_engine.py)
- [Hermes context compressor](https://raw.githubusercontent.com/NousResearch/hermes-agent/main/agent/context_compressor.py)
- [Hermes background review](https://raw.githubusercontent.com/NousResearch/hermes-agent/main/agent/background_review.py)
- [Hermes curator](https://raw.githubusercontent.com/NousResearch/hermes-agent/main/agent/curator.py)
