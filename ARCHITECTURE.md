# architecture

note: sections marked superseded-by-ADR defer to the relevant ADR once 0001-0005 are merged. this file is high-level overview, not the source of truth for decisions.

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## project overview

mycel is a local-first personal agent harness for coding, organized around substrate ecology.

the v0 design goal is to prove that agent runs can leave durable substrate records that affect future runs. **confidence: directional. load-bearing.**

the product direction - a 14-harness field survey, the honest measured state of the gate ("the seam, measured"), and 12 scope-tiered bets (now/next/later) - lives in [`docs/VISION.md`](docs/VISION.md). read it for the "why" behind the substrate/immune-system framing. this file (`ARCHITECTURE.md`) is the how-it's-wired-today; VISION.md is the where-it's-going. **confidence: directional.**

## stack and dependencies

superseded-by-ADR: `docs/adr/0003-language-and-runtime.md`

planned stack:

| layer | choice |
| --- | --- |
| core runtime | Rust |
| canonical interface | MCP |
| command surface | CLI built on MCP tool surface |
| runtime defense | Sentinel as workspace subsystem |
| canonical substrate | SQLite |
| event interchange | JSONL |
| human projections | markdown |
| Hermes interop | Python adapter |
| OpenClaw interop | TypeScript adapter |

Rust should reduce ambiguity in local policy and state handling. **confidence: directional. load-bearing.**

SQLite should be enough for local substrate queries without adding a service dependency.

current Rust dependency roles:

| dependency | role |
| --- | --- |
| `rusqlite` with bundled SQLite | canonical local store access |
| `chrono` | UTC timestamps and expiry fields |
| `uuid` | antibody identifiers |
| `serde` and `serde_json` | typed records and JSON fields |
| `thiserror` | error propagation |

## directory structure

superseded-by-ADR: `docs/adr/0003-language-and-runtime.md`

current structure:

```text
Mycel/
  Cargo.toml
  install.sh                 verbose fail-loud installer -> ~/.mycel
  config/
    mycel.config.toml.template
    mcp.json.template
  crates/
    mycel-core/
    mycel-mcp/               McpTools lib + mycel-mcp-server bin (stdio JSON-RPC)
    mycel-cli/               bin name: mycel-substrate
    mycel-gate/              PreToolUse hook bin, fail-closed antibody gate
    mycel-observe/           PostToolUseFailure hook bin, captures failures (m2)
    mycel-tests/
    sentinel-guard/
  harness/                   grafted kimi-code fork (TS), the agent body
    apps/mycel/              bin name: mycel
      src/tui/commands/mycel/  Mycel slash-command family (immunity/gate/substrate/
                               candidates/promote/deny/delegate) + substrate-runner + panel
    packages/                agent-core, agent-core-v2, kosong, oauth, ...
  adapters/
    hermes/
    openclaw/
  schemas/
  examples/
  docs/
    adr/                     0006 = harness adoption
    specs/                   harness graft design
    plans/
    schemas/
    open-questions.md
```

The `harness/` tree is the kimi-code fork (MIT), grafted with full history and
diverged from upstream (ADR-0006). It is the agent body; `crates/` is the
substrate brain. They meet at two contracts: the `mycel-gate` hook (enforcement,
fail-closed) and `mycel-mcp-server` over MCP (conversation).

`crates/sentinel-guard` enters the workspace as a Git submodule pointed at
`https://github.com/StressTestor/sentinel.git`. Mycel builds it as a workspace
member while Sentinel keeps its own package metadata, repository, license, and
publication path for non-Mycel users.

## core subsystems

| subsystem | role |
| --- | --- |
| `mycel-core` | substrate, antibodies, deterministic proposed-run evaluation, audit/projection runtime |
| `mycel-mcp` | McpTools lib + `mycel-mcp-server` stdio MCP bin (evaluate_run, list_antibodies, propose_antibody - proposals are inert until promoted). Lib also exposes read-only `sentinel_event_count` + `list_candidates` for the CLI status/candidates surfaces |
| `mycel-observe` | `PostToolUseFailure` hook bin: appends each failed/blocked tool call to the substrate audit log as a `SentinelAuditEvent`. Observation-only, always exits 0. The capture half of the m2 learning loop |
| `mycel-cli` | local command surface (bin `mycel-substrate`): ingest, evaluate, list-antibodies, antibody-add, maintain, import-promptpressure, plus read-only `list-candidates` (derives a candidate per stored sentinel event) and `status --db` (one JSON blob: antibody + candidate counts, audit-trail size, last maintenance). Both require an existing db and never auto-create it |
| `mycel-gate` | `PreToolUse` hook bin (catch-all matcher: governs every tool, not just Bash). For a write-class tool it extracts + canonicalizes the target path and runs a compiled protected-path floor (blocks writes to its own `bin`/`config.toml`/`substrate`) BEFORE the db is opened. Opens the substrate read-only + strict, so a 0-byte/truncated/empty-schema db fail-closes instead of allow-all; never creates the db (a deleted db reads as guard-disarmed -> block). Then runs the evaluation engine, emits a fail-closed allow/deny. Seals the Write/Edit + truncated-db self-disarm; a Bash-*command* write to a protected path is a documented residual (needs structured shell parsing) |
| `mycel-tests` | external black-box adversarial suite for v0.1 fail-pattern immunity |
| `sentinel-guard` | always-on runtime defense and shared policy evaluator |
| `harness/apps/mycel` | the agent body (bin `mycel`): TUI, sessions, subagents, providers, hooks |
| `mycel-delegate` | script: runs a governed `claude -p` subagent on the Claude subscription. Claude generates + drives; every Bash command still passes `mycel-gate --claude` (fail-closed), so delegated work stays under the immunity gate. Preferred for subagent work via `~/.mycel/AGENTS.md` when `claude` is present |
| `harness/packages/oauth` | managed credential adapters, including the experimental Codex app-server bridge |

### gate data flow (fail-closed immunity)

```text
harness tool call (ANY tool - catch-all matcher)
  -> PreToolUse hook (fail_mode = "closed")
    -> mycel-gate  (stdin: {tool_name, tool_input, cwd})
      write-class tool? -> extract + canonicalize the target path (payload cwd, ~, symlinks, case)
        -> protected-path floor: bin / config.toml / substrate   [runs BEFORE the db is opened]
             hit -> deny  (same-session self-disarm blocked)
      -> open substrate READ-ONLY + strict  (0-byte / truncated / empty-schema -> BLOCKED; never created)
      -> AntibodyStore::evaluate_run  (SQLite substrate)
        refuse -> {"hookSpecificOutput":{"permissionDecision":"deny", reason: remediation + source}} -> tool BLOCKED
        warn   -> {"message": "..."} -> tool runs, model sees warning
        allow  -> {} -> tool runs
      gate crash / timeout / missing|truncated db / bad json -> nonzero exit -> BLOCKED
```

### immunity learning loop (m2)

```text
tool fails / is blocked
  -> PostToolUseFailure hook -> mycel-observe
       append SentinelAuditEvent -> ~/.mycel/substrate/audit.jsonl (observe)
  -> SessionEnd hook -> mycel-substrate ingest
       record events + surface antibody CANDIDATES (inert, never auto-active)
  -> human review -> mycel-substrate antibody-add   (promote a candidate)
  -> next matching tool call -> mycel-gate BLOCKS it
```

The substrate learns from what goes wrong; nothing auto-activates. Proven by
`tests/e2e/immunity-loop.sh`.

### governed delegation (claude subagents on the subscription)

```text
main mycel agent decides to delegate substantial work
  -> ~/.mycel/AGENTS.md steers it to `mycel-delegate "<task>"` when claude is present
  -> mycel-delegate runs `claude -p` (ANTHROPIC_API_KEY unset -> subscription auth)
       --settings -> PreToolUse Bash hook = mycel-gate --claude (fail-closed)
       --mcp-config -> mycel-mcp-server (subagent can query the substrate)
       --append-system-prompt -> the Mycel subagent preamble
  -> the subagent's every Bash command passes mycel-gate --claude
       deny  -> exit 2 + stderr reason -> Claude Code BLOCKS the tool
       error -> exit 2 (fail-closed) -> BLOCKED
       allow -> exit 0
  -> the subagent returns a final message; mycel relays what matters
```

Claude generates and drives; Mycel keeps governance. `mycel-gate --claude`
speaks Claude Code's hook dialect (exit 2 blocks) instead of the native
`permissionDecision` JSON, so one gate governs both Mycel itself and delegated
`claude -p` subagents. Live proof: `tests/e2e/delegate-live.sh`.

### TUI command family (immune-system surface)

The harness TUI ships a cohesive family of Mycel slash commands under
`harness/apps/mycel/src/tui/commands/mycel/`. They share one launcher
(`substrate-runner.ts`: resolves the managed binaries + db/audit/proposals paths
from MYCEL_HOME and runs a subcommand via `execFile` with an argv array, never a
shell string) and one panel renderer (`panel.ts`: theme painters, severity/mode
colors, whitespace-fold, `mountPanel`). `index.ts` aggregates the seven
`KimiSlashCommand` objects into `MYCEL_SLASH_COMMANDS`, spread once into
`registry.ts`; handlers wire into `dispatch.ts`. Every command fails SOFT — a
missing db/binary/empty result renders a clear panel/message, never a crash.

| command | alias | kind | source |
| --- | --- | --- | --- |
| `/immunity` | `/antibodies` | panel | `mycel-substrate list-antibodies` — active antibodies grouped by severity |
| `/gate` | `/guard`, `/doorman` | panel | config.toml hook wiring + db presence + antibody count -> derived arming state |
| `/substrate` | `/marrow` | panel | `mycel-substrate status --json` — antibody/candidate counts, audit size, last maintenance |
| `/candidates` | `/candidate`, `/learned` | panel | `mycel-substrate list-candidates` — captured lessons not yet signed |
| `/promote <id>` | `/sign` | action | reads `proposals.jsonl`, shells `antibody-add` to sign a proposal |
| `/deny <pattern>` | `/refuse`, `/block` | action | shells `antibody-add` (refuse/hard) to teach the gate a refusal |
| `/delegate <task>` | `/handoff` | action | spawns `mycel-delegate` (governed `claude -p` subagent), streams the result |

Injection safety: `/deny`, `/promote`, and `/delegate` pass user/agent-authored
text (pattern, remediation, task) as a single argv element to `execFile`/`spawn`
with no shell, so shell metacharacters are inert.

### env vars

| var | meaning |
| --- | --- |
| `MYCEL_HOME` | mycel home dir (default `~/.mycel`). Legacy `KIMI_CODE_HOME` honored with a deprecation warning |
| `MYCEL_INSTALL_DIR` | installer target (default `~/.mycel`) |
| `MYCEL_NO_MODIFY_PATH` | skip the installer's shell-rc PATH edit |
| `KIMI_CODE_EXPERIMENTAL_CODEX_SUBSCRIPTION_AUTH` | enable the experimental Codex subscription provider without a config override |

### gotchas

| problem | cause | fix |
| --- | --- | --- |
| gate blocks everything after a db delete | by design: missing db = guard disarmed | re-run `install.sh` to re-init the substrate |
| `mycel` not found after install | PATH rc line not sourced | restart shell or `export PATH="$HOME/.mycel/bin:$PATH"` |
| harness build missing at launch | repo moved or drive unmounted | shim errors loudly with the fix; re-run `install.sh` |
| fresh-HOME install fails at cargo | changing `HOME` unroots rustup | keep `RUSTUP_HOME`/`CARGO_HOME` pointed at the real dirs |

Sentinel gates three scopes:

| gate scope | purpose |
| --- | --- |
| agent launch | every spawn passes Sentinel before an agent starts |
| tool invocation | every tool call is filtered before execution |
| substrate mutation | every substrate write is checked before commit |

Each gate scope owns its policy surface but shares the Sentinel evaluator.

Volva-shedding uses Sentinel as the gate substrate. it stays post-v1, but the integration path is defined.

## key patterns

- local-first substrate state.
- confidence-tagged empirical claims and assumptions.
- schema-driven adapter boundaries.
- request-scoped provider auth: OAuth adapters can supply both a bearer token
  and provider-specific headers without moving tool execution out of Mycel.
- generated human-readable workspace projections.
- always-on runtime defense through shared Sentinel gates.
- deterministic antibody evaluation: populated signature fields are AND matches,
  empty signature fields are wildcards, expired antibodies do not gate runs,
  `file_pattern` uses glob matching, and `command_pattern` uses substring matching.
- substrate mutations append JSONL audit events immediately and debounce
  `SUBSTRATE.md` projection regeneration by 500ms.
- ttl-tiered decay maintenance: solid records are retained, directional records
  are distilled to a gist, vibes records decay to a tombstone, and `no_compost`
  records are preserved regardless of tier.
- handoff specs (self-spec) and dormant-work records (sclerotia) share one
  `TaskIdentity` signature; dormant records become wakeable only when all typed
  wake conditions are met, and resume only through antibody-gated, manual-confirm
  evaluation — never auto-execution.
- work-discovery spores (completed-work / adjacent-work) reuse the same
  `TaskIdentity` signature, are catalogued dedup-on-write, and export to the
  interop loss-matrix shapes as inert metadata that declares its dropped ecology
  fields; v0.5 produces germination candidates only and never launches an agent.

Schema-driven adapters should reduce cross-language coupling. **confidence: directional. load-bearing.**

## database schema

superseded-by-ADR: `docs/adr/0001-substrate-format.md`

current tables:

| table | role |
| --- | --- |
| `antibodies` | v0.1 fail-pattern immunity records, including signature fields, source, severity, confidence, refusal mode, remediation, examples, expiry, and hit count |
| `sentinel_audit_events` | ingested Sentinel JSONL `AuditEvent` records, preserving stable fields as typed columns and unstable fields as metadata |
| `runs` | v0.2 substrate run records: kind, status, summary, confidence, TTL (`expires_at`), preservation flag (`no_compost`), decay state (`retained`/`distilled`/`decayed`), and `distilled_summary` gist |
| `audit_log` | append-only structured event log; entries include `event` type (e.g. `decay`, `promptpressure_import`, `maintenance`) and a JSON payload |
| `specs` | v0.3 self-spec handoff records stored as JSON with an indexed `signature` column |
| `sclerotia` | v0.4 dormant-work records (blocker, attempted paths, next command, typed wake conditions) stored as JSON with an indexed `signature` column |
| `spores` | v0.5 work-discovery manifests (completed-work / adjacent-work) stored as JSON with indexed `signature` and `kind` columns |

SQLite `PRAGMA user_version` is the migration marker. version `4` creates the
`runs` and `audit_log` tables in addition to the v3 schema. The `specs` (v0.3),
`sclerotia` (v0.4), and `spores` (v0.5) tables are added additively to the same schema
build, so they do not bump `user_version` past 4.

Sentinel `matched_rule` parsing populates signature fields:
- `deny.paths: X` or `allow.paths: X` → `file_pattern = X`
- `deny.commands: X` or `allow.commands: X` → `command_pattern = X`
- `deny.secrets: X` → `error_class = X`

Signature matching uses glob patterns for `file_pattern` (supports `*`, `**`, `?`)
and substring matching for `command_pattern`.

## projections and audit

`SubstrateRuntime` wraps the SQLite store when mutations need filesystem side
effects. every antibody insert, update, and delete appends one JSONL audit event
and schedules `SUBSTRATE.md` regeneration for 500ms after the latest mutation.

`SUBSTRATE.md` carries a generated-file header that says it is projection-only
and not an input surface. audit logs rotate from `name.jsonl` to `name.1.jsonl`
when the configured size limit would be exceeded by the next event.

`mycel maintain` runs a full decay cycle and regenerates two workspace files:

| file | content |
| --- | --- |
| `SUBSTRATE.md` | live / retained / preserved runs (active substrate) |
| `COMPOST.md` | distilled runs (gist kept) + decayed runs (tombstone only) |

both files are deterministic projections (stable sort by `(created_at, id)`, no generation
timestamp in body). see ADR 0011.

## eval harness

The v0.1 harness has an in-code seed corpus with at least 25 antibodies, 50
evaluation fixtures, 10 Sentinel events, 10 expiry fixtures, and all three gate
scopes. `mycel harness` calls the MCP tool surface and prints JSON metrics for
the roadmap success criteria.

## environment variables

no environment variables are required yet.

future cloud or model provider variables must be optional unless an ADR says otherwise. **confidence: directional. load-bearing.**

## deployment and infrastructure

no deployment target exists yet.

the default operating model is local CLI plus local MCP server.

## external services and integrations

| system | role |
| --- | --- |
| STs-Mission-Control | possible kin-detection layer |
| PromptPressure | confidence-tier input for context decay |
| OpenClaw | plugin and skill interop reference |
| Hermes Agent | skill and context lifecycle reference |
| Codex / ChatGPT | experimental subscription-backed Responses provider; `codex app-server` owns login and token refresh while Mycel keeps its own loop, tools, hooks, and gate |

OpenClaw and Hermes are useful references for interop design, but Mycel-specific ecology metadata will need graceful degradation. **confidence: directional. load-bearing.**

## gotchas

- markdown substrate files are proposed projections.
- the canonical store stays separate from the human-readable files.
- vibes-tier claims stay hypotheses.
- autonomous spawning waits behind refusal, dormancy, decay, and handoff controls.
- Sentinel is core runtime defense.
- `storage = "codex"` depends on a current `codex` binary on `PATH` and an
  existing `codex login`. It uses an undocumented ChatGPT Responses endpoint,
  so compatibility is version-sensitive and failures must remain explicit. It
  also requires `[experimental] codex_subscription_auth = true` or the matching
  environment flag.

generated projections can overwrite manual edits unless an override policy exists. **confidence: directional. load-bearing.**

## commands

current useful commands:

```sh
cargo build --workspace
cargo test --workspace
cargo fmt -p mycel-core -p mycel-mcp -p mycel-cli -p mycel-tests
cargo clippy --workspace --all-targets -- -D warnings
cd harness && pnpm --filter @moonshot-ai/kimi-code-oauth typecheck
cd harness && pnpm --filter @moonshot-ai/agent-core typecheck
cd harness && pnpm --filter @moonshot-ai/agent-core-v2 typecheck
mycel harness
mycel ingest --jsonl <path>
mycel evaluate --tool-name <name>
mycel list-antibodies
mycel list-candidates --db <path>
mycel status --db <path>
mycel import-promptpressure --db <path> --jsonl <path> [--now <ts>]
mycel maintain --db <path> --workspace <dir> [--now <ts>]
cd harness/apps/mycel && pnpm run typecheck
cd harness/apps/mycel && pnpm exec vitest run
cd harness/apps/mycel && pnpm run build
git status --short
git log --oneline
```

(`mycel` above = the `mycel-substrate` bin.)

note: `cargo fmt --all` walks into `crates/sentinel-guard/` (submodule) and reformats code we don't own. always scope fmt to the four mycel crates.

implementation commands do not exist yet.

## last updated

2026-07-21 — added the TUI Mycel command family (immunity/gate/substrate/candidates/promote/deny/delegate) + the read-only `list-candidates` and `status` substrate subcommands
