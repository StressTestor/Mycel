# Mycel Harness Graft Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Graft the kimi-code fork into the Mycel monorepo as `harness/`, rename its surface to mycel, strip phone-home, and wire the Rust antibody evaluation engine as a fail-closed PreToolUse gate, ending in an installable `mycel` command.

**Architecture:** Two-toolchain monorepo: `crates/` (Rust brain, exists) + `harness/` (TS body, grafted with history). They meet at a new `mycel-gate` hook binary (stdin PreToolUse JSON → `AntibodyStore::evaluate_run` → structured deny/allow, wired `fail_mode = "closed"`) and at `mycel-mcp` in the harness's mcp.json.

**Tech Stack:** Rust (rusqlite/serde/chrono/uuid, existing workspace), TypeScript pnpm monorepo (vitest, tsdown), bash installer.

## Global Constraints

- Spec: `docs/specs/2026-07-19-mycel-harness-graft-design.md`. Read it before starting any task.
- Branch: all work on `feat/harness-graft` in `/Volumes/T7/mycel`. Never touch `/Volumes/T7/Conductor/**` worktrees or `/Volumes/T7/kimi-code-audit` (frozen PR #1906 head; it is the SOURCE of the graft, read-only).
- Node >= 24.15.0, pnpm 10.x, cargo stable. Harness suites run from `harness/` after graft.
- Fail loudly: no empty catches, no silent fallbacks. Gate errors BLOCK (fail-closed). Every user-facing error names the fix.
- TDD everywhere testable: watch each new test fail before implementing.
- The kimi PROVIDER (OAuth, `kimi-code/k3` models) is KEPT. "kimi" strings in provider/protocol code stay. Only user-facing product branding changes.
- Internal `@moonshot-ai/*` package names stay, EXCEPT the app package `apps/kimi-code` which becomes `apps/mycel` / name `mycel`.
- No AI attribution in commits. Conventional commits: `type(scope): description`.
- Baseline before graft: cargo workspace 198 passed / 0 failed at `aa5d78a`. Harness at fork ref `97f6b5e9`: agent-core suites 3900+ green, agent-core-v2 3600+ green minus documented env flakes (Vertex providerOptions, loop snapshot, image/sharp under load, yolo under parallel load).

---

### Task 1: Graft harness with full history

**Files:**
- Create: `harness/` (entire tree via subtree)
- Modify: `.gitignore` (root)

**Interfaces:**
- Produces: `harness/` = kimi-code fork at `97f6b5e9`, buildable with pnpm from `harness/`.

- [ ] **Step 1: Subtree add**

```bash
cd /Volumes/T7/mycel
git subtree add --prefix=harness /Volumes/T7/kimi-code-audit 97f6b5e9
```
Expected: merge commit "Add 'harness/' from commit '97f6b5e9...'". `git log --oneline harness/ | wc -l` > 100 (history preserved).

- [ ] **Step 2: Root .gitignore additions**

Append to `/Volumes/T7/mycel/.gitignore`:
```
harness/node_modules/
harness/**/dist/
harness/**/dist-web/
```

- [ ] **Step 3: Install + baseline harness suites inside monorepo**

```bash
cd /Volumes/T7/mycel/harness && pnpm install --frozen-lockfile
cd packages/agent-core && pnpm vitest run test/hooks test/config test/plugin   # expect 342 passed
cd ../agent-core-v2 && pnpm vitest run test/agent/externalHooks test/app/externalHooksRunner test/app/plugin  # expect 142 passed
```
Expected: same counts as pre-graft baseline. Any delta = investigate before continuing.

- [ ] **Step 4: Cargo still green**

```bash
cd /Volumes/T7/mycel && cargo test --workspace 2>&1 | grep -E "^test result"
```
Expected: 198 passed / 0 failed total, unchanged.

- [ ] **Step 5: Commit gitignore**

```bash
git add .gitignore && git commit -m "chore(harness): ignore node_modules and dist outputs under harness/"
```

### Task 2: mycel-gate crate (TDD)

**Files:**
- Create: `crates/mycel-gate/Cargo.toml`, `crates/mycel-gate/src/main.rs`, `crates/mycel-gate/tests/gate.rs`
- Modify: `Cargo.toml` (root workspace members)

**Interfaces:**
- Consumes: `mycel_core::{AntibodyStore, ProposedRun, SignatureScope, EvaluationOutcome}`; `AntibodyStore::open(path)`, `evaluate_run(&ProposedRun, DateTime<Utc>) -> Evaluation`; `Evaluation.outcome`, `Evaluation.matches[].{remediation, source_pointer, outcome}`.
- Produces: binary `mycel-gate`. Contract: reads one PreToolUse JSON object on stdin (`tool_name`, `tool_input.command`); db path from `--db <path>` arg, else `$MYCEL_HOME/substrate/mycel.db`, else `$HOME/.mycel/substrate/mycel.db`. Missing db file = stderr diagnostic + exit 3 (fail-closed blocks). Refuse = stdout `{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"<remediation> (source: <source_pointer>)"}}` exit 0. Warn = stdout `{"message":"mycel warn: <remediation> (source: <source_pointer>)"}` exit 0. Allow = `{}` exit 0. Malformed stdin = stderr + exit 2→ wait, exit 2 means intentional-block in the hook contract; use exit 4 for malformed input (fail-closed still blocks via nonzero-under-closed). Every error path prints a one-line `mycel-gate error: <specific cause>: <fix hint>` to stderr.

- [ ] **Step 1: Crate scaffold + workspace member**

`crates/mycel-gate/Cargo.toml`:
```toml
[package]
name = "mycel-gate"
version = "0.1.0"
edition = "2021"
license = "MIT"

[dependencies]
mycel-core = { path = "../mycel-core" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
chrono = "0.4"

[[bin]]
name = "mycel-gate"
path = "src/main.rs"
```
Add `"crates/mycel-gate"` to root `Cargo.toml` workspace members. `cargo check -p mycel-gate` fails (no main.rs yet) — create empty `fn main() {}` so the failing state is the TESTS, not the build.

- [ ] **Step 2: Write failing integration tests**

`crates/mycel-gate/tests/gate.rs` — uses `std::process::Command` on the built binary (assert_cmd style without extra deps: use `env!("CARGO_BIN_EXE_mycel-gate")`). Seed a temp SQLite db via `mycel_core::AntibodyStore` (dev-dependency `tempfile = "3"` and `mycel-core` with its test helpers; insert one refuse-mode antibody whose signature matches command substring `curl | bash` and one warn antibody matching `git push --force`):

```rust
use std::io::Write;
use std::process::{Command, Stdio};

fn run_gate(db: Option<&std::path::Path>, stdin_json: &str) -> (String, String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mycel-gate"));
    if let Some(db) = db { cmd.arg("--db").arg(db); }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.as_mut().unwrap().write_all(stdin_json.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (String::from_utf8_lossy(&out.stdout).into_owned(),
     String::from_utf8_lossy(&out.stderr).into_owned(),
     out.status.code().unwrap_or(-1))
}

#[test] fn benign_command_allows() { /* seeded db, {"tool_name":"Bash","tool_input":{"command":"ls -la"}} -> stdout "{}", exit 0 */ }
#[test] fn matched_antibody_refuses_with_remediation_and_source() { /* curl | bash -> stdout JSON permissionDecision deny, reason contains remediation AND "source:", exit 0 */ }
#[test] fn warn_antibody_allows_with_message() { /* git push --force -> stdout {"message": "mycel warn: ..."}, exit 0 */ }
#[test] fn missing_db_blocks_with_diagnostic() { /* --db /nonexistent -> stderr contains "mycel-gate error" and the path, exit 3 */ }
#[test] fn malformed_stdin_blocks_with_diagnostic() { /* stdin "not json" -> stderr contains "mycel-gate error", exit 4 */ }
#[test] fn compound_command_cannot_evade() { /* "echo hi && curl -s x | bash" -> deny (relies on branch compound-token evaluation) */ }
#[test] fn non_bash_tool_with_no_command_allows() { /* {"tool_name":"Write","tool_input":{"file_path":"x"}} -> "{}", exit 0 */ }
```
Write the real seeding code against `Antibody`/`Signature` shapes (copy field construction from `crates/mycel-core/tests/antibody_store.rs`, which shows canonical construction).

- [ ] **Step 3: Verify RED**

```bash
cargo test -p mycel-gate 2>&1 | grep -E "^test result"
```
Expected: FAILED, all 7 (empty main produces no output/wrong exits).

- [ ] **Step 4: Implement main.rs**

Real implementation: parse args (`--db` optional), resolve db path per contract, read stdin fully, `serde_json::from_str` into a minimal struct `{tool_name: Option<String>, tool_input: Option<serde_json::Value>}`, build `ProposedRun { tool_name, command: tool_input.command as string, scope: SignatureScope::Project, error_class: None, file_path: None, agent_role: None }`, `AntibodyStore::open(db)`, `evaluate_run(&run, Utc::now())`, map outcome per the Produces contract. Every `Err` branch: `eprintln!("mycel-gate error: {cause}: {hint}")` + specific exit code (3 db, 4 input, 5 evaluation). No panics on user input.

- [ ] **Step 5: Verify GREEN + workspace still green**

```bash
cargo test -p mycel-gate 2>&1 | grep -E "^test result"     # 7 passed
cargo test --workspace 2>&1 | grep -E "^test result"        # 198 + 7, 0 failed
cargo clippy -p mycel-gate -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add crates/mycel-gate Cargo.toml Cargo.lock
git commit -m "feat(gate): mycel-gate hook binary - fail-closed antibody gate for PreToolUse"
```

### Task 3: Rename surface (apps/kimi-code → apps/mycel)

**Files:**
- Rename: `harness/apps/kimi-code/` → `harness/apps/mycel/`
- Modify: `harness/apps/mycel/package.json` (name `mycel`, bin `mycel`, version `0.1.0`), `harness/pnpm-workspace.yaml` if it names apps explicitly, `harness/package.json` scripts referencing `apps/kimi-code`, `harness/apps/mycel/src/constant/app.ts` (`KIMI_CODE_HOME_ENV` → `MYCEL_HOME_ENV = 'MYCEL_HOME'`, product name constant), `harness/apps/mycel/src/utils/paths.ts` (default home `~/.mycel`; if legacy `KIMI_CODE_HOME` env set and `MYCEL_HOME` unset: use it + print one-line deprecation warning to stderr), user-visible brand strings in TUI/CLI help.

**Interfaces:**
- Produces: `node harness/apps/mycel/dist/main.mjs --version` prints a version; TUI/help say "Mycel"; `MYCEL_HOME` honored; `~/.mycel` default. Kimi provider untouched.

- [ ] **Step 1: Failing test for home resolution**

Add to the existing paths test file (find with `ls harness/apps/mycel/test | grep -i path`; if none exists create `harness/apps/mycel/test/paths.test.ts` following a sibling test's imports) — assert: `MYCEL_HOME` env wins; legacy `KIMI_CODE_HOME` works with deprecation warning captured on stderr spy; default ends with `/.mycel`. Run → RED (constants not yet renamed).

- [ ] **Step 2: git mv + reference sweep**

```bash
cd /Volumes/T7/mycel/harness && git mv apps/kimi-code apps/mycel
grep -rln "apps/kimi-code" --include="*.json" --include="*.yaml" --include="*.ts" --include="*.mjs" . | grep -v node_modules
```
Fix every hit (workspace scripts, tsconfig references, CI paths). Then package.json: `"name": "mycel"`, `"bin": {"mycel": ...}` (keep the same entry file), `"version": "0.1.0"`.

- [ ] **Step 3: Constants + brand strings**

In `src/constant/app.ts`: rename env constant, add `PRODUCT_NAME = 'Mycel'`. Sweep user-facing strings:
```bash
grep -rn "Kimi Code" apps/mycel/src --include="*.ts" | grep -v -i "provider\|moonshot\|kimi-code/\|api"
```
Replace product-branding hits with `Mycel` (TUI title, help text, update-hint strings). Provider/protocol/model-id strings stay.

- [ ] **Step 4: GREEN + build + verify**

```bash
cd harness && pnpm install && pnpm -C apps/mycel run build
node apps/mycel/dist/main.mjs --version         # prints 0.1.0
node apps/mycel/dist/main.mjs --help | head -3  # says Mycel, no "Kimi Code"
pnpm -C apps/mycel vitest run                    # app suite green incl. new paths test
```

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(harness): rename app surface to mycel - binary, home dir, brand strings"
```

### Task 4: De-moonshot (strip phone-home)

**Files:**
- Modify: `harness/apps/mycel/src/cli/update/preflight.ts` (and its call sites in `main.ts` / `run-shell.ts`), `harness/apps/mycel/src/utils/plugin-marketplace.ts`, `harness/apps/mycel/src/cli/telemetry.ts` + telemetry call sites, config default-model error path (find via `grep -rn "default_model" harness/apps/mycel/src harness/packages/agent-core/src/config`).
- Test: sibling test files per touched module.

**Interfaces:**
- Produces: no network call sites for update-check/telemetry/marketplace-default remain; missing `default_model` produces an actionable error naming `default_model` and showing an example TOML block.

- [ ] **Step 1: Failing tests first**

Per strip, a test that proves ABSENCE or the new behavior: update preflight module exports a no-op/removed (test: main startup path contains no fetch to version endpoint — assert the module/symbol is gone by import failure or by asserting the new `doctor` version source is build metadata); marketplace URL getter returns disabled-state error unless explicitly configured (test asserts the exact actionable message); telemetry emitter never constructs a network client (test asserts the disabled flag / stripped call sites — grep-based guard test acceptable: a vitest test that greps `src/` for the telemetry endpoint constant and asserts zero hits outside the disabled shim); missing default_model error includes the string `default_model` and an example block. RED first.

- [ ] **Step 2: Implement strips**

Remove/neutralize each site. Deletions preferred over flags. Where a module is imported elsewhere, replace with an explicit `disabled` stub that throws an actionable error if invoked in a way that would phone home. No silent no-ops: `mycel doctor` must REPORT "update checks: disabled (mycel)" and "telemetry: removed".

- [ ] **Step 3: GREEN + live network observation**

```bash
cd harness && pnpm -C apps/mycel vitest run && pnpm -C apps/mycel run build
# live check: run a short prompt session with lsof/nettop watching, only provider + localhost traffic
```

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(harness): strip update-check, telemetry, marketplace default - local-first, no phone-home"
```

### Task 5: Config template + MCP + hooks wiring

**Files:**
- Create: `config/mycel.config.toml.template` (repo root `config/`), `config/mcp.json.template`
- Test: installer verify step consumes these (Task 6).

**Interfaces:**
- Produces: template config with: no default_model (commented examples for kimi OAuth / anthropic / openai-compat+ollama / google-genai), `[[hooks]]` block wiring `mycel-gate` (`fail_mode = "closed"`, matcher `Bash`, timeout 10) and a commented ghost block; mcp.json template registering `mycel-mcp` (command `~/.mycel/bin/mycel-mcp`, args from existing mycel-cli MCP entry — copy invocation shape from `crates/mycel-mcp`).

- [ ] **Step 1: Write both templates with real content** (providers commented with working field names from `harness/apps/mycel/src` config schema; hooks block exactly as proven in-session).
- [ ] **Step 2: Validate template parses**: a small vitest in apps/mycel that loads the template through the real config parser expecting zero errors (uncommenting nothing).
- [ ] **Step 3: Commit** `feat(config): mycel config + mcp templates with fail-closed gate wiring`.

### Task 6: Installer

**Files:**
- Create: `install.sh` (repo root)

**Interfaces:**
- Consumes: everything prior. Produces: `~/.mycel/bin/{mycel,mycel-gate,mycel-mcp}`, `~/.mycel/config.toml` (from template, only if absent), `~/.mycel/substrate/` dir, PATH line in shell rc (skippable via `MYCEL_NO_MODIFY_PATH`), post-install verification.

- [ ] **Step 1: Write install.sh** — bash, `set -euo pipefail`, verbose `==>` logging per step, prereq checks with exact remedies (node >= 24.15, pnpm, cargo; each missing tool prints its install command), builds `cargo build --release -p mycel-gate -p mycel-mcp -p mycel-cli`, `pnpm install && pnpm -C apps/mycel run build` in harness, installs a `mycel` shim (`exec node <repo>/harness/apps/mycel/dist/main.mjs "$@"` with repo path baked at install time + existence check that errors loudly with remedy if the repo moved), copies release binaries, scaffolds config/substrate dir (never overwrites existing config — prints "kept existing"), then POST-VERIFY: `mycel --version`, `mycel-gate` golden benign payload through stdin expecting `{}`, doctor run. Any step failure = abort with the failing step named.
- [ ] **Step 2: Fresh-HOME test**: `HOME=$(mktemp -d) MYCEL_NO_MODIFY_PATH=1 bash install.sh` → full pass on a clean home; run twice → idempotent ("kept existing" on second run).
- [ ] **Step 3: Commit** `feat(install): verbose fail-loud installer with post-install verification`.

### Task 7: E2E kill-test

**Files:**
- Create: `tests/e2e/harness-gate.sh` + seed fixture JSONL

**Interfaces:** end-to-end proof on the REAL installed surface.

- [ ] **Step 1: Script**: seed temp substrate db with the curl|bash refuse antibody (via `mycel-cli` or direct sqlite through a small rust helper in mycel-cli's existing ingest path), isolated `MYCEL_HOME`, then: (a) benign prompt through `mycel -p` executes echo; (b) prompt asking for the seeded-pattern command → BLOCKED, remediation string present in transcript; (c) replace gate binary with a script that SIGKILLs itself → command BLOCKED (fail-closed); (d) restore. Uses the file-based prompt pattern (avoids claude-side ghost self-FP).
- [ ] **Step 2: Run it, all 4 phases pass.** Requires a configured provider (kimi OAuth from migrated home or local ollama).
- [ ] **Step 3: Commit** `test(e2e): live fail-closed gate proof against installed harness`.

### Task 8: CI, ADR, docs

**Files:**
- Create: `.github/workflows/monorepo-ci.yml`, `docs/adr/0006-harness-adoption.md`
- Modify: `ARCHITECTURE.md`, `README.md`

- [ ] **Step 1: CI**: two jobs (cargo: fmt-check scoped to mycel crates + clippy + test; harness: pnpm install + targeted suites agent-core/agent-core-v2 hook+config+plugin + apps/mycel suite + build). Actions pinned to commit SHAs. Known upstream flake list documented in the workflow comments, not skipped silently.
- [ ] **Step 2: ADR-0006**: status accepted; records fork=harness/rust=brain, monorepo graft, ADR-0003 amendment (harness is a full TS layer; substrate/policy stays Rust), gate contract, de-moonshot scope, PR #1906 relationship.
- [ ] **Step 3: ARCHITECTURE.md + README**: harness section (stack table row, directory tree, gate data flow, env vars MYCEL_HOME, install instructions, gotchas incl. T7-dependency + fail-closed semantics). Joe's voice for README prose (run voice-check).
- [ ] **Step 4: Commit** `docs(adr): 0006 harness adoption; architecture + readme for the graft`.

---

## Self-review notes

- Spec coverage: graft(T1), gate(T2), rename+MYCEL_HOME(T3), de-moonshot(T4), config+MCP(T5), installer/installable(T6), e2e+kill-test(T7), CI/ADR/docs(T8). Migration-from-~/.kimi-code offer: covered by T3 legacy-env support + T6 "kept existing" config handling; full migration screen deferred (spec allows: offer copy-migration — T6 prints the manual copy command when `~/.kimi-code` exists and `~/.mycel` fresh). No gaps against m1 acceptance.
- Placeholders: task 2 test bodies are contracts with exact expected behavior; implementer writes bodies against the named fixture file `crates/mycel-core/tests/antibody_store.rs` for canonical construction. Sweep tasks carry exact grep contracts + verification commands that fail if incomplete.
- Type consistency: gate consumes only named public items verified present in `mycel-core/src/lib.rs` (lines 112-258 region).
