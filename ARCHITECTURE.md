# architecture

Accepted ADRs are the decision record. This file describes the code that exists
today.

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## project overview

mycel is a local-first personal agent harness for coding, organized around substrate ecology.

Agent runs can leave durable substrate records that affect future runs through
explicit review and promotion.

The product direction and the reason substrate ecology is load-bearing live in
[`docs/VISION.md`](docs/VISION.md). This file is the factual wiring that exists
today; the completed Rust migration boundary is recorded in
[`docs/RUST_PORT_PARITY.md`](docs/RUST_PORT_PARITY.md). **confidence: solid.**

## stack and dependencies

superseded-by-ADR: `docs/adr/0003-language-and-runtime.md`

stack:

| layer | choice |
| --- | --- |
| core runtime | Rust |
| canonical interface | MCP |
| command surface | CLI built on MCP tool surface |
| runtime defense | fail-closed `mycel-gate` hook |
| canonical substrate | SQLite |
| event interchange | JSONL |
| human projections | markdown |
Rust owns the installed product, including the terminal client, agent runtime,
providers, tools, policy, and substrate. **confidence: solid. load-bearing.**

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
    mycel-agent-protocol/    provider-neutral messages/events/config/records
    mycel-agent-runtime/     sessions, turns, tools, MCP, orchestration, child agents
    mycel-providers/         model-provider, discovery, and credential adapters
    mycel-core/
    mycel-mcp/               McpTools lib + mycel-mcp-server bin (stdio JSON-RPC)
    mycel-cli/               bins: mycel and mycel-substrate; terminal + ecology UI
      src/clipboard.rs       bounded macOS/Linux image paste -> provider media parts
      src/system_prompt.rs   bounded environment, instructions, tree, and skill context
      src/tui_config.rs      private terminal theme/editor/client preferences
      src/workspace_config.rs  private project-local additional-root persistence
    mycel-gate/              PreToolUse hook bin, fail-closed antibody gate
    mycel-observe/           PostToolUseFailure hook bin, captures failures (m2)
    mycel-tests/
  tests/e2e/
  docs/
    adr/
    RUST_PORT_PARITY.md
    VISION.md
```

The former Kimi-derived TypeScript tree was removed after its retained behavior
was captured by Rust fixtures, adversarial tests, CLI contracts, and PTY tests.
Upstream attributions remain in `THIRD_PARTY_NOTICES.md`. The `crates/` path is
the only product implementation.

## core subsystems

| subsystem | role |
| --- | --- |
| `mycel-agent-protocol` | network- and filesystem-free Rust serialization boundary for provider messages/streams, public CLI events, normalized config, permissions, sessions, loop events, and the forward-compatible 1.4 agent record envelope |
| `mycel-agent-runtime` | agent body with explicit IDs/cancellation/events, durable JSONL records, validated full-history forks and replay, canonical context with idle-only undo, crash-recoverable steering, durable manual/automatic compaction, permissions, local/retained/MCP tools, hooks, skills, local plugins, incremental streaming turns, goals, cron, Workflow, Hyphae, background/swarm state with foreground process/subagent detachment, and capability-bounded in-process child agents |
| `mycel-providers` | retained Anthropic, OpenAI Chat/Responses, Kimi, and Google/Vertex wire families; typed registry and explicit discovery; API-key, OAuth, service-account, managed Kimi, and Codex subscription credentials; vendor marketplace/control-plane behavior is excluded |
| `mycel-core` | substrate, antibodies, deterministic proposed-run evaluation, audit/projection runtime |
| `mycel-mcp` | McpTools lib + `mycel-mcp-server` stdio MCP bin (evaluate_run, list_antibodies, propose_antibody - proposals are inert until promoted). Lib also exposes read-only `sentinel_event_count` + `list_candidates` for the CLI status/candidates surfaces |
| `mycel-observe` | `PostToolUseFailure` hook bin: appends each failed/blocked tool call to the substrate audit log as a `SentinelAuditEvent`. Observation-only, always exits 0. The capture half of the m2 learning loop |
| `mycel-cli` | Rust command package. `mycel-substrate` retains ingest/evaluate/antibody/maintenance/status operations. `mycel` owns parsing, real provider/session execution, bounded system-prompt composition, `/new`/`/sessions`/`/reload`/`/fork`/`/title` lifecycle transitions, durable `/add-dir` workspace roots, headless text/JSON output, Unix terminal driving, approvals/questions, bounded clipboard image paste, governed interactive shell execution, crash-recoverable Ctrl-S steering, Ctrl-B foreground process/subagent detachment, durable `/compact`, `/undo`, and `/swarm`, textual background-task management, doctor/export, provider management, session-scoped MCP, plan/skill/media composition, bounded local-plugin installation/state/removal, native `/init`, `/copy`, `/hyphae`, projected-history `/btw`, private terminal settings/themes, external-editor handoff, native exports, and the seven-command ecology service |
| `mycel-gate` | `PreToolUse` hook bin (catch-all matcher: governs every tool, not just Bash). For a write-class tool it extracts + canonicalizes the target path and runs a compiled protected-path floor (blocks writes to its own `bin`/`config.toml`/`substrate`) BEFORE the db is opened. Opens the substrate read-only + strict, so a 0-byte/truncated/empty-schema db fail-closes instead of allow-all; never creates the db (a deleted db reads as guard-disarmed -> block). Then runs the evaluation engine, emits a fail-closed allow/deny. Seals the Write/Edit + truncated-db self-disarm; a Bash-*command* write to a protected path is a documented residual (needs structured shell parsing) |
| `mycel-tests` | external black-box adversarial suite for v0.1 fail-pattern immunity plus a source-level product-boundary regression gate that prevents removed browser/editor/server/telemetry/update/marketplace surfaces from returning |

### gate data flow (fail-closed immunity)

```text
agent tool call (ANY tool - catch-all matcher)
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

### native delegation

```text
main Mycel agent invokes Agent, AgentSwarm, Workflow, or /delegate
  -> the native orchestration bundle derives a capability-bounded child profile
  -> the child gets its own durable session and native turn engine
  -> the shared permission and hook pipeline governs every child tool call
  -> foreground work may be detached into the durable background-task registry
  -> completion, failure, cancellation, and restart loss are terminal records
```

No external Claude/Codex process or subscription-specific helper is part of
delegation. `/delegate` is a compatibility UX over the same native Agent path.

### system prompt and workspace roots

`crates/mycel-cli/src/system_prompt.rs` builds one provider-neutral coding-agent
prompt for parent turns, compaction, goal continuation, and child agents. It
includes bounded OS/shell/time/cwd context, a two-level workspace tree, skill
summaries, and hierarchical instruction files. Instruction precedence is
`MYCEL_HOME/AGENTS.md`, user `~/.agents/AGENTS.md` (or lowercase fallback), then
root-to-leaf project `.mycel/AGENTS.md`, `AGENTS.md`, and `agents.md`. Symlinked
instruction files are ignored, each file is capped at 1 MiB, and the combined
instruction budget is 4 MiB.

Additional workspace roots are canonical existing directories. Session roots
are stored in the durable session index and immediately trigger runtime
recomposition so tool confinement changes with the UI state. `/add-dir <path>`
adds a session root; `/add-dir remember <path>` also updates the project root's
`.mycel/local.toml`. The local file preserves unrelated TOML, rejects symlinks
and special files, is capped at 1 MiB, and is written by private atomic replace.
CLI `--add-dir` values and remembered roots are merged and deduplicated before
session resolution.

Skill roots, in precedence order: project `.mycel/skills` and `.agents/skills`
(or the `--skills-dir` list instead of both), user `MYCEL_HOME/skills` and
`~/.agents/skills`, `extra_skill_dirs`, then plugin roots. Roots are
deduplicated by canonical path before scanning, so launching from `$HOME` (no
`.git`, project root == user home) scans `~/.agents/skills` once. Trust is per
source: a **project** root is confined to itself and a symlink resolving outside
it is refused (`EscapesRoot`), because the checkout may not be the user's;
**user / extra / builtin / plugin** roots follow symlinks, so a tree like
`~/.agents/skills/claude-octopus -> ~/.codex/claude-octopus/skills` loads.
Cycle detection and depth/directory/file/byte limits apply either way (a
followed symlink into a wide tree stops at the directory-visit cap with a
`DirLimit` warning).

### terminal client state and side questions

Client-only preferences live in private `MYCEL_HOME/tui.toml`, separate from
provider and agent policy in `config.toml`. The Rust TUI accepts only the built-in
`auto`, `dark`, and `light` themes; it does not reproduce the inherited custom
theme/plugin machinery. `/theme`, `/editor`, `/settings`, and `/reload-tui`
update or reload the bounded TOML document through a 0600 atomic replace.
ANSI control sequences are excluded from width and truncation calculations.
Ctrl-G restores the terminal before running the explicitly configured editor
against a private bounded temporary file, then resumes the same session with
the edited draft.

`/btw` is a separate ephemeral session, not an extra message in the parent
history. It copies only a wire-complete projection of the current context, has
an empty tool registry, streams into its own panel, accepts follow-up turns, and
may run while the main turn continues. Closing the panel cancels bounded work,
closes its record log, and removes the generated runtime directory. This keeps
the side-channel behavior without persisting an orphan session or giving it
filesystem/network tools.

### TUI command family (immune-system surface)

`crates/mycel-cli/src/ecology.rs` implements seven native commands. Read-only
panels check that the database already exists and never initialize it as a side
effect. Informational failures render a clear message instead of crashing the
TUI; mutations remain explicit and validated.

| command | alias | behavior |
| --- | --- | --- |
| `/immunity` | `/antibodies` | active antibodies grouped by severity |
| `/gate` | `/guard`, `/doorman` | derived hook, database, and antibody arming state |
| `/substrate` | `/marrow` | antibody/candidate counts, audit size, and maintenance state |
| `/candidates` | `/candidate`, `/learned` | captured lessons that are not yet signed |
| `/promote <id>` | `/sign` | resolve a bounded proposal identifier and explicitly sign it |
| `/deny <pattern>` | `/refuse`, `/block` | add a hard refusal through the Rust substrate API |
| `/delegate <task>` | `/handoff` | invoke a capability-bounded native child agent |

`/deny` and `/promote` call typed Rust APIs rather than constructing a shell
command. `/delegate` returns a typed native-orchestration request, so task text
never enters a shell.

### native workflows and Hyphae mode

The main-agent-only `Workflow` tool accepts exactly one inline plan or saved workflow name.
Saved plans live at `<MYCEL_HOME>/workflows/<name>.json` and are read and hashed
fresh on every call. Plans are declarative JSON rather than executable JavaScript:
phases run sequentially, tasks inside a phase run in parallel, and later phases
may interpolate completed earlier results with `{{result:task_id}}`. Call arguments
use `{{arg:key}}`.

One `Workflow` call becomes one detached background task. Its children are
detached from parent-turn cancellation but remain visually grouped under that
workflow. Worker profiles cannot receive `Agent`, `AgentSwarm`, or `Workflow`, so
the direct 128-task ceiling cannot be bypassed through recursive fan-out. Any
failed or aborted task stops later phases. Whole-workflow execution is bounded
by the native orchestration configuration. Task state uses the public `workflow`
background-task kind; full output is kept in the task log and an auxiliary 0600
manifest is written to `<session>/workflows/<run-id>.json`. Restart reconciliation
marks an interrupted workflow and its running manifest `lost`. Session shutdown
treats active workflow children as background agents, so
`background.keep_alive_on_exit = true` preserves their turns with the parent
workflow instead of canceling them as foreground work.

`/hyphae on` is a session-only Mycel orchestration mode: it switches to xhigh
effort without persisting the model setting and enables existing swarm-mode
orchestration authorization. `/hyphae <task>` enables the same one-shot mode
and immediately submits the task. `/hyphae off` disables swarm mode; it does
not restore the previous effort level.

Interactive and prompt modes compose the same native Workflow implementation.
The default programmatic worker cap is three tasks across all phases. Worker
agents still cannot receive `Agent`, `AgentSwarm`, or `Workflow`, so they cannot
recursively expand the cap. Normal approval policy still applies in hosts that
do not autoapprove tools.

### env vars

| var | meaning |
| --- | --- |
| `MYCEL_HOME` | mycel home dir (default `~/.mycel`) |
| `MYCEL_INSTALL_DIR` | installer target (default `~/.mycel`) |
| `MYCEL_NO_MODIFY_PATH` | skip the installer's shell-rc PATH edit |
| `MYCEL_EXPERIMENTAL_CODEX_SUBSCRIPTION_AUTH` | enable the experimental Codex subscription provider without a config override |

### gotchas

| problem | cause | fix |
| --- | --- | --- |
| gate blocks everything after a db delete | by design: missing db = guard disarmed | re-run `install.sh` to re-init the substrate |
| `mycel` not found after install | PATH rc line not sourced | restart shell or `export PATH="$HOME/.mycel/bin:$PATH"` |
| fresh-HOME install fails at cargo | changing `HOME` unroots rustup | keep `RUSTUP_HOME`/`CARGO_HOME` pointed at the real dirs |
| startup warns `skill scan EscapesRoot` for a symlinked skill dir | the root is a **project** root (confined by design) | move the link under `~/.agents/skills` or `MYCEL_HOME/skills`, or add the real dir to `extra_skill_dirs` |
| startup warns `skill ... shadowed by a higher-precedence source` for every skill in `~/.agents/skills` | pre-fix double scan when project root == `$HOME`; fixed by canonical-root dedupe | update; if it persists the two roots really are different dirs holding the same skill name |

## key patterns

- local-first substrate state.
- confidence-tagged empirical claims and assumptions.
- schema-driven adapter boundaries.
- request-scoped provider auth: OAuth adapters can supply both a bearer token
  and provider-specific headers without moving tool execution out of Mycel.
- one bounded provider-neutral system prompt supplies hierarchical project
  instructions, workspace shape, and skill summaries to parent and child turns.
- additional filesystem roots are canonicalized, session-durable, and optionally
  persisted in a private project-local `.mycel/local.toml`.
- generated human-readable workspace projections.
- fail-closed tool governance through `mycel-gate`.
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

The v0.1 evaluation command has an in-code seed corpus with at least 25 antibodies, 50
evaluation fixtures, 10 Sentinel events, 10 expiry fixtures, and all three gate
scopes. `mycel harness` calls the MCP tool surface and prints JSON metrics for
the roadmap success criteria.

## environment variables

no environment variables are required. Optional provider and installation
variables are documented above.

future cloud or model provider variables must be optional unless an ADR says otherwise. **confidence: directional. load-bearing.**

## deployment and infrastructure

no deployment target exists yet.

the default operating model is local CLI plus local MCP server.

## external services and integrations

| system | role |
| --- | --- |
| PromptPressure | confidence-tier input for context decay |
| Codex / ChatGPT | experimental subscription-backed Responses provider; `codex app-server` owns login and token refresh while Mycel keeps its own loop, tools, hooks, and gate |
| model providers | explicit user-configured Kimi, Anthropic, Google, and OpenAI-compatible endpoints |
| MCP | explicit user-configured local stdio or remote Streamable HTTP servers |
| plugins | explicit local directories are copied without symlinks into Mycel's bounded managed plugin store; the private atomic `plugins/installed.json` ledger owns enable/MCP state, and the Rust session loader composes enabled namespaced skills, MCP servers, and argv-only commands; remote archives and marketplace acquisition are unsupported |

The default CLI performs no telemetry, update, banner, marketplace, or vendor
control-plane request.

## gotchas

- markdown substrate files are proposed projections.
- the canonical store stays separate from the human-readable files.
- vibes-tier claims stay hypotheses.
- autonomous spawning waits behind refusal, dormancy, decay, and handoff controls.
- `storage = "codex"` depends on a current `codex` binary on `PATH` and an
  existing `codex login`. It uses an undocumented ChatGPT Responses endpoint,
  so compatibility is version-sensitive and failures must remain explicit. It
  also requires `[experimental] codex_subscription_auth = true` or the matching
  environment flag.
- `Workflow` plans are intentionally not source-compatible with Claude Code's
  executable workflow scripts. Mycel uses a bounded JSON contract so a saved
  plan cannot run arbitrary host JavaScript.
- `/hyphae off` leaves xhigh effort active. Select another effort explicitly
  if the session should return to its earlier setting.
- MCP's deprecated SSE transport is rejected. Use stdio or Streamable HTTP.
- Local plugin contribution changes take effect in a new session. Rust owns
  install/remove/enable/MCP state; `/plugins reload` only refreshes the
  informational ledger view. A plugin may still declare an explicit local `node`
  command, but Mycel does not inject a bundled Node runtime.
- `/add-dir remember` writes `.mycel/local.toml` at the detected project root;
  use session-only `/add-dir` when that project-local persistence is unwanted.

generated projections can overwrite manual edits unless an override policy exists. **confidence: directional. load-bearing.**

## commands

current useful commands:

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
bash tests/e2e/gate-contract.sh
bash tests/e2e/immunity-loop.sh
mycel-substrate harness
mycel-substrate ingest --jsonl <path>
mycel-substrate evaluate --tool-name <name>
mycel-substrate list-antibodies
mycel-substrate list-candidates --db <path>
mycel-substrate status --db <path>
mycel-substrate import-promptpressure --db <path> --jsonl <path> [--now <ts>]
mycel-substrate maintain --db <path> --workspace <dir> [--now <ts>]
```

## last updated

2026-08-16 — skill roots deduplicate by canonical path and follow symlinks on
user/extra/builtin/plugin roots (project roots stay confined). Fixes the
post-cutover startup noise and the 54 octo skills that silently stopped loading
from `~/.agents/skills`.

2026-08-14 — completed the Rust-only cutover. The installed CLI now owns real
providers, durable sessions, streaming turns, bounded prompts and workspace
roots, local/retained/MCP tools, permissions and hooks, Unix terminal execution,
dialogs, clipboard image paste, steering, undo, compaction, background work,
native child orchestration, Workflow, Hyphae, goals, cron, local plugins,
provider management, exports, BTW conversations, and the seven-command ecology
service. The Kimi-derived TypeScript oracle and its build/CI dependency spine
were removed after the Rust fixture, adversarial, CLI, and PTY gates passed.
