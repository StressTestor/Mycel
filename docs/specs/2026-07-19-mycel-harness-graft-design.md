# mycel harness graft design

date: 2026-07-19
status: approved by joe (session decisions, brainstorming flow)
confidence key: **solid** verified or strongly supported. **directional** shape right, details may change.

## what this is

mycel stops being brain-only. the StressTestor/kimi-code fork (TypeScript agent CLI, MIT) is grafted into the Mycel monorepo as `harness/` and becomes mycel's agent body: TUI, sessions, subagents, providers, hooks, plugins. the existing Rust workspace (`crates/`) stays the substrate brain: antibodies, evaluation engine, sentinel ingestion, SQLite canonical store, MCP canonical interface. they meet at two boundaries: a fail-closed PreToolUse hook (enforcement) and MCP (conversation). **solid: all component claims verified in-session 2026-07-18/19.**

decisions locked by joe:

1. fork = harness, rust = brain. no TS rewrite of the substrate.
2. monorepo: graft into StressTestor/Mycel with full git history. StressTestor/kimi-code stays frozen as the upstream PR #1906 head.
3. model-agnostic = strip phone-home, keep all providers (kimi OAuth included, anthropic/openai-compat/google-genai promoted to equals, no vendor default model).
4. milestone 1 = boot + immunity gate live (not the full ingestion loop).
5. gate wiring = standalone rust hook binary (mycel-gate), ghost-style, `fail_mode = "closed"`.

## architecture

```
StressTestor/Mycel (monorepo)
├── crates/                     rust brain (existing, unchanged home)
│   ├── mycel-core/             substrate, antibodies, eval engine, wake rules
│   ├── mycel-mcp/              MCP server, canonical interface
│   ├── mycel-cli/              local command surface
│   ├── mycel-gate/             NEW: PreToolUse hook bin, links mycel-core
│   └── sentinel-guard/         submodule, runtime defense
├── harness/                    NEW: detached kimi-code fork (TS pnpm monorepo)
│   ├── apps/mycel/             renamed from apps/kimi-code, binary `mycel`
│   └── packages/...            agent-core, agent-core-v2, kosong, etc (names kept)
├── docs/adr/0006-harness-adoption.md   NEW: records this design, amends ADR-0003
└── docs/specs/                 this spec
```

base branch: graft lands on a new branch off `feat/substrate-ecology-v02-v05` (23 ahead of main, contains the compound-command gate-evasion fix mycel-gate needs). **directional: joe may prefer main; confirm at PR time.**

graft mechanics: `git subtree add --prefix=harness <local fork> 97f6b5e9` (full history). upstream cherry-picks later via subtree pull, by choice only.

## harness changes (milestone 1)

rename surface, not internals:

- binary + app package: `mycel` (apps/kimi-code -> apps/mycel, bin name, TUI brand strings, `--version` string).
- home dir `~/.mycel`, env `MYCEL_HOME` (replaces KIMI_CODE_HOME; read old env with a loud deprecation warning for one release).
- config migration: on first boot, if `~/.mycel` missing and `~/.kimi-code` exists, offer copy-migration (reuse migration screen pattern).
- internal `@moonshot-ai/*` package names KEPT at m1. unpublished inside our tree; renaming is mechanical churn with zero behavior.

de-moonshot (verbose failures, never silent):

- telemetry: emission disabled at the source package boundary. removal verified by test + live network observation.
- update checker / latest-version fetch: removed. `mycel doctor` reports version from build metadata only.
- plugin marketplace CDN default: removed; marketplace URL must be explicitly configured or feature reports itself disabled.
- default model: none. missing model config = actionable error naming the config key and an example block, not a fallback.
- kept: kimi OAuth provider path, moonshot API providers, anthropic, openai-compat (covers ollama/local), google-genai. all config-driven.

## the gate (mycel-gate)

new crate, small bin, links mycel-core directly (no daemon at m1):

- stdin: PreToolUse JSON (`hook_event_name`, `session_id`, `cwd`, `tool_name`, `tool_input`, `tool_call_id`). same contract as ghost/claude/kimi, proven compatible in-session.
- evaluation: mycel-core proposed-run evaluation engine against the SQLite substrate (refuse / warn / allow). compound-command token evaluation included (branch fix).
- stdout on refuse: `hookSpecificOutput.permissionDecision: "deny"` + `permissionDecisionReason` = remediation string + source pointer (roadmap metric: every refusal has both). exit 0 with JSON (structured path) — matches ghost's proven pattern.
- warn outcome: allow + `message` content so the model sees the warning in context.
- errors: every failure path (bad JSON, missing db, sqlite error, schema mismatch) prints a specific diagnostic to stderr and exits nonzero. under `fail_mode = "closed"` that means BLOCK. the gate never silently allows on error. **solid: fail-closed runner semantics are ours, tested both cores.**
- wired in `~/.mycel/config.toml`: `[[hooks]]` PreToolUse, matcher `Bash`, `fail_mode = "closed"`, timeout 10, beside ghost (two guards, one contract).

mcp: `mycel-mcp` server wired into harness mcp.json so the model can query antibodies / propose candidates / read substrate state. read paths only at m1 for proposals (candidates land as proposed, never auto-active). **directional: exact tool list follows mycel-mcp's existing surface.**

NOT in m1 (milestone 2): auto-ingestion of harness events (PostToolUseFailure, ghost block logs) into antibody candidates — that is the v0.1.1 hardening track.

## error handling standard (whole graft)

- fail loudly. no empty catches, no default-masking. every catch either rethrows with context or produces a user-visible actionable diagnostic.
- boundaries validate: gate validates hook JSON shape, harness validates config with actionable messages (the kimi config layer already does this well — preserve it), installer validates prerequisites before acting.
- fail-closed on security paths: gate errors block. T7-style absent-dependency states (missing db, missing binary) block with a message naming the fix.
- migration/copy operations: backup before write, report every file touched.

## testing standard

- both existing suites keep running: cargo workspace tests + harness vitest suites (agent-core 3900+, agent-core-v2 3600+), in one CI with pinned actions. known upstream flakes documented, not silenced.
- mycel-gate: TDD. golden-payload fixture tests (benign allow, antibody refuse with remediation, warn, malformed JSON, missing db, compound-command evasion attempts). RED observed before implementation.
- rename/de-moonshot: each strip gets a test proving absence (no telemetry emission call sites, no update fetch, config error messages assert exact actionable text).
- e2e kill-test (the proven pattern): built harness + real gate: benign command executes; seeded-antibody command blocks with remediation surfaced in transcript; SIGKILLed gate blocks (fail-closed); same with ghost co-wired.
- installer test: clean-machine simulation (fresh HOME) install -> `mycel --version` + doctor + gated command.

## installable state (goal condition)

- `install.sh` at repo root: builds mycel-gate + mycel-cli/mcp (cargo), builds harness (pnpm), installs `mycel` shim + gate binary to `~/.mycel/bin`, writes PATH line, scaffolds config.toml template (providers commented, hooks block present), runs post-install verify (version + doctor + gate golden test). idempotent, verbose, every step reports success/failure, aborts loudly on any failure.
- upstream-style checksummed binary distribution is OUT of scope for m1 (local build install only).

## m1 acceptance

- `mycel --version` from fresh install, `mycel doctor` clean.
- providers: kimi OAuth works (joe's k3), at least one non-moonshot provider verified live (anthropic or ollama).
- zero phone-home observed during a session (network watch).
- antibody gate: live block with remediation + fail-closed kill-test pass, ghost co-wired.
- all suites green (cargo + both harness cores), CI config present.
- ARCHITECTURE.md updated (mandate), ADR-0006 committed, vault captured.
