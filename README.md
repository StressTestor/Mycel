# mycel

Mycel is a local-first coding-agent CLI built around substrate ecology. The
agent loop, terminal client, providers, tools, policy, and durable memory are
implemented in Rust.

The product is deliberately terminal-only. It does not ship a browser UI,
editor extension, daemon, remote control plane, cloud telemetry, self-updater,
or bundled plugin marketplace.

## what ships

- `crates/mycel-agent-protocol`: the normalized Rust message, event, record,
  permission, session, and configuration contracts.
- `crates/mycel-agent-runtime`: the native replay, context, permission,
  scheduler, session, tools, orchestration, skills, plugins, and MCP runtime.
- `crates/mycel-providers`: Rust adapters for the retained provider wire families
  and credential flows.
- `crates/mycel-core`: the SQLite substrate, antibodies, task records, decay,
  sclerotia, spores, and deterministic evaluation logic.
- `crates/mycel-gate`: the fail-closed `PreToolUse` policy hook.
- `crates/mycel-observe`: failure capture for the immunity learning loop.
- `crates/mycel-cli`: the installed native `mycel` CLI/TUI plus
  `mycel-substrate`.
- `crates/mycel-mcp`: the local stdio MCP interface to the substrate.

The Kimi-derived TypeScript implementation was removed after its retained
behavior was captured in Rust fixtures, adversarial tests, CLI contracts, and
PTY tests. The migration boundary and intentional exclusions are recorded in
[`docs/RUST_PORT_PARITY.md`](docs/RUST_PORT_PARITY.md).

## install

Requirement: a stable Rust toolchain.

```sh
bash install.sh
```

The installer builds and installs the native Rust binaries under `~/.mycel`,
initializes the substrate, and verifies that the gate allows a
benign operation and blocks when its database is missing.

Then edit `~/.mycel/config.toml`, configure a provider and model, and run:

```sh
mycel
```

Mycel ships no default model. Kimi, Anthropic, Google, OpenAI-compatible local
servers, and the experimental Codex subscription adapter are available through
explicit configuration. Provider traffic and user-configured MCP/plugin traffic
are the only expected network activity.

Production terminal support is macOS and Linux. Windows is not currently
supported.

## immunity loop

Mycel never promotes a captured failure automatically:

```text
tool failure or block
  -> mycel-observe records an audit event
  -> mycel-substrate ingest creates an inert candidate
  -> a human reviews and promotes the candidate
  -> mycel-gate enforces the active antibody on later calls
```

The TUI exposes this substrate through `/immunity`, `/gate`, `/substrate`,
`/candidates`, `/promote`, `/deny`, and `/delegate`. Declarative Workflow and
session-scoped `/hyphae` orchestration are also part of Mycel's product, not
fork scaffolding.

The gate fails closed when its process crashes, times out, receives invalid
input, or cannot open a valid substrate database. Its compiled protected-path
floor blocks structured write tools from replacing the installed gate, config,
or substrate before database evaluation. Shell-command writes to those paths
remain a documented residual until structured shell parsing lands.

## develop

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bash tests/e2e/gate-contract.sh
bash tests/e2e/immunity-loop.sh
```

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the current wiring and
[`docs/VISION.md`](docs/VISION.md) for the product direction.

## license

Mycel is MIT licensed. Licenses for code derived from Kimi Code and pi-tui are
preserved in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
