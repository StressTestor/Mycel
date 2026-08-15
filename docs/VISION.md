# mycel

Mycel is a local-first coding-agent CLI built around substrate ecology. The
agent loop, providers, sessions, tools, terminal UI, policy, and substrate are
implemented in Rust.

## why it exists

Most agent safety is advisory state inside the same process as the model. A
missing hook, timeout, or child process can turn that safety off. Session memory
has the same weakness: useful corrections disappear with the conversation or
remain as prose the model may ignore.

Mycel puts both below the model in a durable substrate:

- deterministic policy runs before a tool action;
- failure of the security path denies the action;
- failed work becomes an inert candidate, never an active rule;
- a human explicitly signs every antibody promotion;
- durable records survive sessions, models, and provider changes.

The goal is not a policy plugin attached to another harness. The goal is a
mature terminal agent whose safety and memory are structural parts of its
runtime.

## product shape

Mycel is one Rust CLI with two local contracts:

1. The agent runtime owns providers, the turn loop, context, tools, sessions,
   approvals, orchestration, and the terminal interface.
2. The substrate owns antibodies, audit records, candidate promotion, and
   fail-closed evaluation.

MCP is the extension boundary. Provider calls and user-configured MCP or plugin
traffic are explicit; default startup has no telemetry, update, marketplace,
banner, or vendor control-plane request.

The former Kimi-derived implementation was removed after the Rust path passed
the shared fixture, adversarial, CLI, PTY, and ecology gates recorded in
`docs/RUST_PORT_PARITY.md`. Its package graph and product surfaces are not a
compatibility contract.

## the ecology

The metaphor is load-bearing:

- **substrate** is durable local state: records, audit, candidates, and active
  antibodies;
- **innate immunity** is the compiled, deterministic floor that exists before
  learning;
- **adaptive immunity** is a specific lesson proposed from failed work;
- **promotion** is the human signature that makes a candidate enforceable;
- **clearance** is expiry, decay, and removal so the system does not become
  permanently over-restrictive;
- **Hyphae** is session-scoped orchestration with bounded, gated workers;
- **Workflow** is declarative phased work, never executable workflow code.

The seven ecology commands remain product behavior: `/immunity`, `/gate`,
`/substrate`, `/candidates`, `/promote`, `/deny`, and `/delegate`.

## design principles

- Fail closed on security-path error, absence, timeout, or malformed input.
- Keep authoritative decisions deterministic and independent of model judgment.
- Persist a record before publishing the corresponding live event.
- Treat replay as a pure reduction over the durable log.
- Let lower-trust configuration tighten policy, never widen a higher-trust deny.
- Keep secrets redacted in memory, logs, errors, and debug output.
- Return structured denial feedback so the agent can re-plan.
- Give child agents no capability the parent lacks; re-check at execution time.
- Keep learned rules inert until a human promotes them.
- Prefer small explicit Rust contracts over inherited framework layers.

## mature CLI baseline

The Rust product must retain:

- interactive and headless operation with text and streaming JSON output;
- secure provider authentication and all retained model wire families;
- durable create, resume, continue, fork, export, compact, undo, and title flows;
- tools, hooks, approvals, questions, plan/YOLO/auto modes, and shell execution;
- MCP, skills, explicit plugins, media, diffs, and terminal rendering;
- foreground and background agents, task inspection, queueing, swarm, goals,
  cron, Workflow, Hyphae, and restart reconciliation;
- the full ecology command family and fail-closed gate integration.

Shared fixtures, adversarial tests, PTY behavior, and end-to-end ecology tests
keep this baseline executable. A parser-only or unwired success path is not a
product capability.

## non-goals

- browser UI, VS Code or other editor implementations;
- local HTTP/WebSocket daemons or remote control planes;
- cloud telemetry, self-update, promotions, or bundled marketplaces;
- ACP, a standalone client SDK, or a second experimental agent engine;
- Node or TypeScript product runtime dependencies;
- autonomous rule promotion;
- executable host-language workflow files;
- being the most autonomous harness at the expense of a trustworthy boundary.

## hardening direction

After the Rust cutover, the safety spine continues with:

- structured shell parsing with deny-on-unparseable behavior;
- OS sandboxing that refuses to start when isolation cannot initialize;
- default-deny egress and substrate-owned credential injection;
- immutable protected paths and asymmetric config trust;
- substrate-owned checkpoints around mutating tool calls;
- first-class gate verdicts in the replayable event log;
- antibody expiry, scope enforcement, disable/delete, and promotion audit;
- an inert reflector that proposes memory or antibodies without activating them.

These are directions, not claims about shipped behavior. Current wiring and
known residuals belong in `ARCHITECTURE.md`; executable parity status belongs in
`docs/RUST_PORT_PARITY.md`.

## success

Mycel succeeds when it can handle daily coding work without another agent body,
every child inherits the same gate, a crash resumes from durable state, and
fault injection cannot turn a missing safety component into permission.

Last updated 2026-08-14.
