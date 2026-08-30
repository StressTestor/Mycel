# Provider Manager Dialog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `/provider` and `/provider list` open an in-TUI provider manager dialog instead of tearing down and relaunching the whole interactive session.

**Architecture:** The `ProviderManagerReducer` dialog (shipped in PR #29, currently wired only to `tests/dialog_parity.rs`) becomes a local modal in the interactive loop: a new `Option<ProviderManagerReducer>` field on `InteractiveLoopState`, an input interception slot below RPC dialogs, and a render branch below the RPC dialog branch. Provider rows come from a `provider_views` snapshot captured in `PreparedInteractive` at prepare time (every session transition re-prepares, so the snapshot refreshes on every mutation). Mutating subcommands (`login`, `logout`, `remove`, `list --json`) keep the existing `InteractiveSessionTransition::Provider` teardown path verbatim; dialog-driven delete routes through that same path, exiting the app only when the active provider is deleted (existing fail-closed semantics), resuming otherwise.

**Tech Stack:** Rust (toolchain pinned 1.98.0), single crate `mycel-cli`, no new dependencies.

**Spec:** No standalone spec doc. Design context: mc finding `f_*` "Mycel /provider flash-relaunch REPRODUCED + root-caused" (2026-08-29) + the Design section below. `docs/design/tui-implementation-spec.md` does not cover the provider dialog.

## Design (inline spec)

Root cause being fixed: every `/provider` subcommand routes through `InteractiveSessionTransition::Provider` (`production.rs:5060-5073` → `1304-1336`), which exits the alternate screen, runs the command outside the TUI, then relaunches the session with the transcript reseeded to two status lines (`seed_transcript`, `production.rs:7252`). For read-only `list` this is pure disruption: screen flash, scrollback display wiped, provider table crammed into one `warning:` line.

Contract after this plan:
- `/provider`, `/providers`, `/provider list` → open the in-TUI dialog. No transition, no flash, transcript untouched.
- `/provider list --json`, `/provider login kimi`, `/provider logout kimi`, `/provider remove <id>` → unchanged (transition path).
- Dialog keys (already implemented in the reducer, covered by `tests/dialog_parity.rs` — do NOT re-test that layer): up/down/left/right/pgup/pgdn move, `d` asks `y/n` confirm for delete, enter on the add row emits Add, esc emits Close.
- Dialog actions: Close → dismiss. Add → dismiss + status hint naming the real add flows. Delete(ids) → dismiss + `Provider { Remove, close_after }` transition where `close_after` is true only when the deleted id is the session's active provider (deleting the active provider makes the resume unresolvable — `remove()` drops the provider's models, so re-prepare would fail on the current alias; exiting is the existing fail-closed behavior). Non-active delete resumes the session and the re-prepare refreshes the snapshot.
- Modal precedence, both paths: RPC dialogs (`state.dialogs`) first, provider manager second, btw third. An approval arriving mid-dialog takes the screen and the keyboard; the provider dialog resumes when it clears.
- Ctrl-c inside the dialog closes the dialog (matches `DialogHost`'s 0x03-cancels convention); the next ctrl-c reaches the turn.

## Global Constraints

- Rust toolchain pinned to 1.98.0 (CI, PR #28). Gates before any commit: `cargo check`, `cargo test -p mycel-cli`, `cargo clippy -- -D warnings`, `cargo fmt --check`. Run them, do not reason about them.
- Repo root: `/Volumes/T7/Mycel` (case-sensitive path). Branch off `main` (currently `11e8b06f`), branch name `fix/provider-dialog-in-tui`.
- Conventional commits: `type(scope): description`, imperative, lowercase, no period. No AI attribution lines, no Co-Authored-By.
- Full test suite for the touched crate must be green before claiming any task done: `cargo test -p mycel-cli`.
- Code comments in this repo may use ghost voice (kaomoji + deadpan) but each comment must state a true constraint, one per comment. Plain is always acceptable.

---

### Task 1: Extract `provider_views` from `ProviderCommandService::list`

**Files:**
- Modify: `crates/mycel-cli/src/provider_commands.rs:510-533` (extract body of `list()`)
- Test: `crates/mycel-cli/src/provider_commands.rs` (tests module in the same file)

**Interfaces:**
- Consumes: existing private `credential_status(&ProviderEntryConfig, &dyn ProviderCommandEnvironment) -> CredentialStatus` (`provider_commands.rs:945`), existing `MycelConfig` (the type `load_config()` at `provider_commands.rs:735` returns).
- Produces: `pub fn provider_views(config: &MycelConfig, environment: &dyn ProviderCommandEnvironment) -> Vec<ConfiguredProviderView>` — free function in `provider_commands.rs`. Tasks 2 and 3 call it. `ConfiguredProviderView` fields (already public, `provider_commands.rs:331`): `id: String`, `provider_type: ProviderType`, `base_url: Option<String>`, `model_count: usize`, `credential: CredentialStatus`, `is_default: bool`.

- [ ] **Step 1: Write the failing test**

In the existing `#[cfg(test)] mod tests` in `provider_commands.rs`, next to the other list tests (around line 1329):

```rust
#[test]
fn provider_views_reports_defaults_and_credentials_from_a_parsed_config() {
    let config: MycelConfig = toml::from_str(
        r#"
default_model = "local"

[providers.local]
type = "openai"
base_url = "http://127.0.0.1:11434/v1"
api_key = "test-key"

[models.local]
provider = "local"
model = "gpt-test"
max_context_size = 8192
max_output_size = 128

[providers.remote]
type = "anthropic"
base_url = "https://example.invalid/v1"
"#,
    )
    .expect("config");
    let views = provider_views(&config, &FakeEnvironment(Mutex::new(BTreeMap::new())));
    let local = views.iter().find(|view| view.id == "local").expect("local");
    assert_eq!(local.credential, CredentialStatus::Configured);
    assert!(local.is_default);
    assert_eq!(local.model_count, 1);
    let remote = views.iter().find(|view| view.id == "remote").expect("remote");
    assert_eq!(remote.credential, CredentialStatus::Missing);
    assert!(!remote.is_default);
    assert_eq!(remote.model_count, 0);
}
```

`FakeEnvironment(Mutex<BTreeMap<String, String>>)` is the module's existing test stub (`provider_commands.rs:1140`); constructing it empty makes the `remote` provider resolve `Missing` regardless of the runner's real environment. If the module's config fixtures build `MycelConfig` some other way than `toml::from_str`, follow the module's way.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mycel-cli provider_views_reports_defaults -- --nocapture`
Expected: FAIL to compile with "cannot find function `provider_views`"

- [ ] **Step 3: Write minimal implementation**

Move the body of `list()` (`provider_commands.rs:510-533`) into a free function; `list()` delegates:

```rust
/// Snapshot the configured providers for display. Free-standing so the
/// interactive loop can build dialog rows from an already-loaded config
/// without constructing the full command service. (｡◕‿↼) same rows, no ceremony
pub fn provider_views(
    config: &MycelConfig,
    environment: &dyn ProviderCommandEnvironment,
) -> Vec<ConfiguredProviderView> {
    config
        .providers
        .iter()
        .map(|(id, provider)| ConfiguredProviderView {
            id: id.clone(),
            provider_type: provider.provider_type,
            base_url: provider.base_url.clone(),
            model_count: config
                .models
                .values()
                .filter(|model| model.provider == *id)
                .count(),
            credential: credential_status(provider, environment),
            is_default: config.default_provider.as_deref() == Some(id)
                || config
                    .default_model
                    .as_ref()
                    .and_then(|model| config.models.get(model))
                    .is_some_and(|model| model.provider == *id),
        })
        .collect()
}
```

```rust
    pub fn list(&self) -> Result<Vec<ConfiguredProviderView>, ProviderCommandError> {
        Ok(provider_views(&self.load_config()?, self.environment.as_ref()))
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mycel-cli provider_ -- --nocapture` then `cargo test -p mycel-cli`
Expected: new test PASS, all existing `list()` tests still PASS (the delegation must not change behavior)

- [ ] **Step 5: Commit**

```bash
git add crates/mycel-cli/src/provider_commands.rs
git commit -m "refactor(cli): extract provider_views from ProviderCommandService::list"
```

---

### Task 2: Snapshot `provider_views` into `PreparedInteractive`

**Files:**
- Modify: `crates/mycel-cli/src/production.rs:2868` (struct `PreparedInteractive` — add field)
- Modify: `crates/mycel-cli/src/production.rs:~3343` (the `Ok(PreparedInteractive { ... })` literal — populate field; `config` is in scope there, see `config.models.keys()` two lines above)
- Modify: `crates/mycel-cli/src/production.rs:78` area (extend the `provider_commands::{...}` import with `provider_views`, `ConfiguredProviderView`)
- Test: `crates/mycel-cli/src/production.rs` (tests module)

**Interfaces:**
- Consumes: `provider_views(&MycelConfig, &dyn ProviderCommandEnvironment)` from Task 1; `ProcessProviderEnvironment` (already used at `production.rs:1043`).
- Produces: `PreparedInteractive.provider_views: Vec<ConfiguredProviderView>` — Task 3 reads it to build dialog rows. Snapshot semantics: captured once per prepare; every session transition re-prepares, so it refreshes after any provider mutation. Only an external hand-edit of `config.toml` mid-session can make it stale, and the next transition heals that.

- [ ] **Step 1: Write the failing test**

In the production tests module, using the existing `#[cfg(test)] prepare_interactive` helper (`production.rs:1057`) and the same `adapter(...)`/`TempDir`/`config()` fixtures the neighboring tests use (see `production.rs:9948` for the construction pattern):

```rust
#[test]
fn prepare_interactive_snapshots_provider_views() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("mycel");
    fs::create_dir_all(&home).expect("MYCEL_HOME");
    fs::write(home.join(CONFIG_FILE), config()).expect("provider config");
    let adapter = adapter(
        &temp,
        Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        }),
        Arc::new(ScriptedTransport::default()),
    );
    let prepared = adapter
        .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
        .expect("prepare");
    assert_eq!(prepared.provider_views.len(), 1);
    assert_eq!(prepared.provider_views[0].id, "local");
    assert_eq!(
        prepared.provider_views[0].credential,
        CredentialStatus::Configured,
    );
    assert!(prepared.provider_views[0].is_default);
}
```

The `config()` fixture (`production.rs:9658`) defines exactly one provider `local` with `api_key = "test-key"`, so `credential` resolves `Configured` from the config alone — the assertion cannot be flipped by a real API key sitting in the test runner's environment. Keep it that way: never assert `Environment(..)`/`Missing` on a fixture provider whose type's env var might genuinely be set on a dev box.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mycel-cli prepare_interactive_snapshots_provider_views -- --nocapture`
Expected: FAIL to compile with "no field `provider_views` on type `PreparedInteractive`"

- [ ] **Step 3: Write minimal implementation**

Add to the struct (after the `provider: String` field, `production.rs:~2890`):

```rust
    /// Configured-provider snapshot for the in-TUI manager dialog, taken at
    /// prepare time. Transitions re-prepare, so mutations refresh it.
    provider_views: Vec<ConfiguredProviderView>,
```

Populate in the literal (next to `provider: resolved.provider_id.clone()`, `production.rs:~3361`):

```rust
        provider_views: provider_views(&config, &ProcessProviderEnvironment),
```

Place the call before any move of `config` in that function (it currently only borrows for `model_aliases`; if the compiler objects about a move, compute `provider_views` into a local right after `model_aliases`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mycel-cli prepare_interactive_snapshots_provider_views` then `cargo test -p mycel-cli`
Expected: PASS, suite green

- [ ] **Step 5: Commit**

```bash
git add crates/mycel-cli/src/production.rs
git commit -m "feat(cli): snapshot provider views into PreparedInteractive"
```

---

### Task 3: Route bare `/provider` and `/provider list` to the dialog

**Files:**
- Modify: `crates/mycel-cli/src/production.rs:5060-5073` (the `/provider` arm of the slash handler)
- Modify: `crates/mycel-cli/src/production.rs:~3571` (struct `InteractiveLoopState` — add `provider_manager` field, near `dialogs: DialogHost`) and its `new(...)` constructor (initialize `None`)
- Modify: production.rs `crate::tui` import list — add `ProviderManagerAction`, `ProviderManagerReducer`, `ProviderRow` (re-exported via `tui/mod.rs:13`)
- Modify: `crates/mycel-cli/src/provider_command_runner.rs:564,573` — make `credential_name` and `provider_type_name` `pub(crate)` (row labels reuse them; do not duplicate the mappings)
- Test: `crates/mycel-cli/src/production.rs` (tests module)

**Interfaces:**
- Consumes: `prepared.provider_views` (Task 2), `prepared.provider: String` (active provider id), `ProviderManagerReducer::new(rows, active_provider)` (`tui/dialogs/management.rs:36`), `crate::provider_command_runner::{credential_name, provider_type_name}`.
- Produces: `InteractiveLoopState.provider_manager: Option<ProviderManagerReducer>`; method `fn open_provider_manager(&mut self, prepared: &PreparedInteractive)`. Row contract for Tasks 4-5: one non-add row per configured provider with `provider_ids == vec![view.id]` (exactly one id — Task 4's delete takes `ids.first()` on the strength of this), plus one trailing add row with `add_action: true, provider_ids: vec![]`.

- [ ] **Step 1: Write the failing test**

Unit-level, driving the slash handler through the same path the loop uses. Follow the construction pattern of whichever existing test drives `process_actions`/slash input with a real `InteractiveLoopState` (the tests around `production.rs:9948` build `prepared` + executor); if none constructs the full loop state, test `open_provider_manager` directly:

```rust
#[test]
fn bare_provider_command_opens_the_manager_dialog_instead_of_a_transition() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("mycel");
    fs::create_dir_all(&home).expect("MYCEL_HOME");
    fs::write(home.join(CONFIG_FILE), config()).expect("provider config");
    let adapter = adapter(
        &temp,
        Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        }),
        Arc::new(ScriptedTransport::default()),
    );
    let prepared = adapter
        .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
        .expect("prepare");
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("executor");
    let mut state = InteractiveLoopState::new(
        &executor,
        &prepared,
        TerminalSize { columns: 120, rows: 40 },
    );

    let handled = state.handle_session_command(&executor, &prepared, "/provider");
    assert!(handled);
    assert!(state.session_transition.is_none(), "list must not tear the session down");
    let manager = state.provider_manager.as_ref().expect("dialog open");
    assert_eq!(manager.rows.len(), 2, "one provider row + add row");
    assert_eq!(manager.rows[0].provider_ids, vec!["local".to_owned()]);
    assert!(manager.rows[0].label.contains("local"));
    assert!(manager.rows[0].label.contains("configured"));
    assert!(manager.rows[1].add_action);
    assert_eq!(manager.selected, 0, "selection starts on the active provider");

    state.provider_manager = None;
    let handled = state.handle_session_command(&executor, &prepared, "/provider list --json");
    assert!(handled);
    assert!(state.provider_manager.is_none());
    assert!(
        matches!(
            state.session_transition,
            Some(InteractiveSessionTransition::Provider { close_after: false, .. })
        ),
        "list --json keeps the transition path",
    );
}
```

`handle_session_command` (`production.rs:4641`) is the method containing the `/provider` arm; `InteractiveLoopState::new(&Runtime, &PreparedInteractive, TerminalSize)` is at `production.rs:3616`; `TerminalSize { columns: u16, rows: u16 }` is in `terminal/driver.rs:15`. All verified against `main` at `11e8b06f`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mycel-cli bare_provider_command_opens -- --nocapture`
Expected: FAIL to compile with "no field `provider_manager`"

- [ ] **Step 3: Write minimal implementation**

Make the two label fns in `provider_command_runner.rs` `pub(crate)` (lines 564 and 573, keep bodies verbatim).

Add the field to `InteractiveLoopState` (near `dialogs: DialogHost`, `production.rs:~3571`) and `provider_manager: None` in its constructor:

```rust
    /// Local provider-manager modal. RPC dialogs outrank it: input and render
    /// both check `dialogs` first. they ALL yield to approvals eventually XX
    provider_manager: Option<ProviderManagerReducer>,
```

Add the open method on `InteractiveLoopState`:

```rust
    fn open_provider_manager(&mut self, prepared: &PreparedInteractive) {
        let mut rows: Vec<ProviderRow> = prepared
            .provider_views
            .iter()
            .map(|view| ProviderRow {
                id: view.id.clone(),
                label: format!(
                    "{} · {} · {} model{} · {}{}",
                    view.id,
                    provider_type_name(view.provider_type),
                    view.model_count,
                    if view.model_count == 1 { "" } else { "s" },
                    credential_name(&view.credential),
                    if view.is_default { " · default" } else { "" },
                ),
                provider_ids: vec![view.id.clone()],
                add_action: false,
            })
            .collect();
        rows.push(ProviderRow {
            id: "add".to_owned(),
            label: "add a provider".to_owned(),
            provider_ids: Vec::new(),
            add_action: true,
        });
        self.provider_manager =
            Some(ProviderManagerReducer::new(rows, Some(prepared.provider.as_str())));
    }
```

Rewrite the `/provider` arm (`production.rs:5060-5073`):

```rust
        for command in ["/provider", "/providers"] {
            if let Some(arguments) = slash_arguments(input, command) {
                let words = arguments.split_whitespace().collect::<Vec<_>>();
                if matches!(words.as_slice(), [] | ["list"]) {
                    self.open_provider_manager(prepared);
                    return true;
                }
                match parse_interactive_provider_command(arguments) {
                    Ok((command, close_after)) => {
                        self.session_transition = Some(InteractiveSessionTransition::Provider {
                            command,
                            close_after,
                        });
                    }
                    Err(error) => self.status(error),
                }
                return true;
            }
        }
```

Leave `parse_interactive_provider_command` itself untouched — its `[] | ["list"]` arm becomes unreachable from the TUI but still serves its unit tests and keeps the usage string honest for every other arm.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mycel-cli bare_provider_command_opens` then `cargo test -p mycel-cli`
Expected: PASS. `interactive_provider_list_restores_terminal_and_resumes_the_session` (`production.rs:12559`) will now FAIL — that is expected and correct; it pins the old contract and Task 6 rewrites it. Every other test must be green. If anything else fails, stop and investigate before proceeding.

- [ ] **Step 5: Commit**

```bash
git add crates/mycel-cli/src/production.rs crates/mycel-cli/src/provider_command_runner.rs
git commit -m "feat(tui): open provider manager dialog for /provider and /provider list"
```

---

### Task 4: Dialog input interception and action drain

**Files:**
- Modify: `crates/mycel-cli/src/production.rs:6866-6906` (input dispatch in `interactive_terminal_body` — add interception after the `state.dialogs.is_active()` branch)
- Modify: `crates/mycel-cli/src/production.rs` (`InteractiveLoopState` impl — add `apply_provider_manager_input`)
- Test: `crates/mycel-cli/src/production.rs` (tests module)

**Interfaces:**
- Consumes: `ProviderManagerAction::{Add, Delete, Close}` (`tui/dialogs/management.rs:21`), `is_control_c` (`production.rs:7062`), `Command::Provider`, `ProviderArgs`, `ProviderCommand::Remove` (all already imported), row contract from Task 3 (`provider_ids` has exactly one id on deletable rows).
- Produces: `fn apply_provider_manager_input(&mut self, input: InputEvent, prepared: &PreparedInteractive) -> bool` — returns true when a session transition was requested, mirroring `process_actions`' exit contract so the loop breaks the same way.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn provider_dialog_delete_routes_remove_and_only_active_provider_exits() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("mycel");
    fs::create_dir_all(&home).expect("MYCEL_HOME");
    fs::write(home.join(CONFIG_FILE), config_two_providers()).expect("provider config");
    let adapter = adapter(
        &temp,
        Arc::new(RecordingConfig {
            source: config_two_providers(),
            paths: Mutex::new(Vec::new()),
        }),
        Arc::new(ScriptedTransport::default()),
    );
    let prepared = adapter
        .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
        .expect("prepare");
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("executor");
    let mut state = InteractiveLoopState::new(
        &executor,
        &prepared,
        TerminalSize { columns: 120, rows: 40 },
    );
    let mut decoder = InputDecoder::default();
    let feed = |state: &mut InteractiveLoopState, decoder: &mut InputDecoder, bytes: &[u8]| {
        let mut exit = false;
        for input in decoder.feed(bytes) {
            exit |= state.apply_provider_manager_input(input, &prepared);
        }
        exit
    };

    // esc closes, nothing else happens. a lone \x1b byte stays BUFFERED in
    // InputDecoder::feed (only flush or more bytes resolve it), so tests send
    // esc in its kitty CSI-u form, which decodes immediately.
    state.open_provider_manager(&prepared);
    assert!(!feed(&mut state, &mut decoder, b"\x1b[27u"));
    assert!(state.provider_manager.is_none());
    assert!(state.session_transition.is_none());

    // ctrl-c closes too (DialogHost convention)
    state.open_provider_manager(&prepared);
    assert!(!feed(&mut state, &mut decoder, &[0x03]));
    assert!(state.provider_manager.is_none());

    // delete the NON-active provider: transition with close_after false.
    // arrow movement must not report exit, so the feeds are split.
    state.open_provider_manager(&prepared);
    assert!(!feed(&mut state, &mut decoder, b"\x1b[B"));
    assert!(feed(&mut state, &mut decoder, b"dy"));
    match state.session_transition.take() {
        Some(InteractiveSessionTransition::Provider { command, close_after }) => {
            assert!(!close_after, "non-active delete resumes the session");
            assert!(
                matches!(
                    command,
                    Command::Provider(ProviderArgs {
                        command: ProviderCommand::Remove { ref provider_id },
                    }) if provider_id == "remote"
                ),
                "delete must target the selected row",
            );
        }
        other => panic!("expected provider transition, got {other:?}"),
    }
    assert!(state.provider_manager.is_none());

    // delete the ACTIVE provider: close_after true (fail-closed exit)
    state.open_provider_manager(&prepared);
    assert!(feed(&mut state, &mut decoder, b"dy"));
    assert!(matches!(
        state.session_transition.take(),
        Some(InteractiveSessionTransition::Provider { close_after: true, .. })
    ));

    // 'n' at the confirm keeps the dialog open and deletes nothing
    state.open_provider_manager(&prepared);
    assert!(!feed(&mut state, &mut decoder, b"dn"));
    assert!(state.provider_manager.is_some());
    assert!(state.session_transition.is_none());

    // Add: dialog closes, status hint lands, no transition
    state.open_provider_manager(&prepared);
    let manager = state.provider_manager.as_mut().expect("open");
    manager.selected = manager.rows.len() - 1;
    assert!(!feed(&mut state, &mut decoder, b"\r"));
    assert!(state.provider_manager.is_none());
    assert!(state.session_transition.is_none());
}
```

Add the fixture next to `config()` (`production.rs:9658`); both providers carry `api_key` so credential labels never depend on the runner's environment:

```rust
    fn config_two_providers() -> String {
        let mut source = config();
        source.push_str(
            r#"
[providers.remote]
type = "openai"
base_url = "https://example.invalid/v1"
api_key = "remote-key"

[models.remote]
provider = "remote"
model = "gpt-remote"
max_context_size = 8192
max_output_size = 128
"#,
        );
        source
    }
```

The active provider in this fixture is `local` (`default_model = "local"`), so the dialog opens with row 0 (`local`) selected; `\x1b[B` moves to `remote`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mycel-cli provider_dialog_delete_routes_remove -- --nocapture`
Expected: FAIL to compile with "no method named `apply_provider_manager_input`"

- [ ] **Step 3: Write minimal implementation**

Method on `InteractiveLoopState`:

```rust
    fn apply_provider_manager_input(
        &mut self,
        input: InputEvent,
        prepared: &PreparedInteractive,
    ) -> bool {
        if is_control_c(&input) {
            self.provider_manager = None;
            return false;
        }
        let Some(manager) = self.provider_manager.as_mut() else {
            return false;
        };
        manager.apply(input);
        let actions = std::mem::take(&mut manager.actions);
        for action in actions {
            match action {
                ProviderManagerAction::Close => {
                    self.provider_manager = None;
                }
                ProviderManagerAction::Add => {
                    self.provider_manager = None;
                    self.status(
                        "add a provider: /login (codex oauth) · /provider login kimi",
                    );
                }
                ProviderManagerAction::Delete(ids) => {
                    self.provider_manager = None;
                    let Some(provider_id) = ids.first().cloned() else {
                        continue;
                    };
                    // deleting the ACTIVE provider strands the session's model,
                    // so that one keeps the fail-closed exit >:[ on purpose
                    let close_after = provider_id == prepared.provider;
                    self.session_transition = Some(InteractiveSessionTransition::Provider {
                        command: Command::Provider(ProviderArgs {
                            command: ProviderCommand::Remove { provider_id },
                        }),
                        close_after,
                    });
                }
            }
        }
        self.session_transition.is_some()
    }
```

Interception in `interactive_terminal_body`, directly after the `state.dialogs.is_active()` branch (`production.rs:6869-6872`) and before the btw branches — RPC dialogs must win the keyboard:

```rust
                        if state.provider_manager.is_some() {
                            if state.apply_provider_manager_input(input, prepared) {
                                exit_requested = true;
                                break;
                            }
                            continue;
                        }
```

There is a second, pre-loop input drain after the startup flourish (`production.rs:6830`, `state.process_actions(...)`): typed-ahead input there cannot have the dialog open yet (nothing opens it before first dispatch), so it needs no interception — do not add one.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mycel-cli provider_dialog_delete_routes_remove` then `cargo test -p mycel-cli`
Expected: PASS (Task 6's rewrite of the 12559 test still pending; only that named test may be red)

- [ ] **Step 5: Commit**

```bash
git add crates/mycel-cli/src/production.rs
git commit -m "feat(tui): drain provider manager dialog actions with fail-closed active delete"
```

---

### Task 5: Render the dialog

**Files:**
- Modify: `crates/mycel-cli/src/production.rs:7474-7490` area (`interactive_center_view` — add branch AFTER the `state.dialogs.active` branch)
- Modify: `crates/mycel-cli/src/production.rs` (new free fn `provider_manager_view_lines`, next to `dialog_view_lines` at 7523)
- Test: `crates/mycel-cli/src/production.rs` (tests module)

**Interfaces:**
- Consumes: `push_wrapped(&mut Vec<String>, &str, usize)` (`production.rs:7739`), `ProviderManagerReducer { rows, selected, confirm, .. }`.
- Produces: `fn provider_manager_view_lines(manager: &ProviderManagerReducer, width: usize) -> Vec<String>` — plain unstyled lines, same shape `dialog_view_lines` returns.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn provider_manager_view_marks_selection_and_confirm_state() {
    let rows = vec![
        ProviderRow {
            id: "local".to_owned(),
            label: "local · openai · 1 model · configured · default".to_owned(),
            provider_ids: vec!["local".to_owned()],
            add_action: false,
        },
        ProviderRow {
            id: "add".to_owned(),
            label: "add a provider".to_owned(),
            provider_ids: Vec::new(),
            add_action: true,
        },
    ];
    let mut manager = ProviderManagerReducer::new(rows, Some("local"));
    let lines = provider_manager_view_lines(&manager, 120);
    let joined = lines.join("\n");
    assert!(joined.contains("providers"));
    assert!(joined.contains("> local · openai · 1 model · configured · default"));
    assert!(joined.contains("  add a provider"));
    assert!(joined.contains("esc closes"));
    manager.confirm = Some(vec!["local".to_owned()]);
    let joined = provider_manager_view_lines(&manager, 120).join("\n");
    assert!(joined.contains("delete local? y/n"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mycel-cli provider_manager_view_marks -- --nocapture`
Expected: FAIL to compile with "cannot find function `provider_manager_view_lines`"

- [ ] **Step 3: Write minimal implementation**

```rust
fn provider_manager_view_lines(manager: &ProviderManagerReducer, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    push_wrapped(&mut lines, "providers", width);
    lines.push(String::new());
    for (index, row) in manager.rows.iter().enumerate() {
        let marker = if index == manager.selected { ">" } else { " " };
        push_wrapped(&mut lines, &format!("{marker} {}", row.label), width);
    }
    lines.push(String::new());
    if let Some(ids) = &manager.confirm {
        push_wrapped(&mut lines, &format!("delete {}? y/n", ids.join(", ")), width);
    } else {
        push_wrapped(
            &mut lines,
            "up/down selects · enter adds · d deletes · esc closes",
            width,
        );
    }
    lines
}
```

Branch in `interactive_center_view`, placed AFTER the `state.dialogs.active` branch (`production.rs:7474`) so an approval mid-dialog takes the screen; copy the dialogs branch's viewport/cursor arithmetic exactly (`production.rs:7475-7490` — blank spacer, extend lines, cursor on last line, `viewport_start` clamp, early return):

```rust
    if let Some(manager) = &state.provider_manager {
        lines.push(String::new());
        lines.extend(provider_manager_view_lines(manager, width));
        let cursor_absolute_row = lines.len().saturating_sub(1);
        let cursor_absolute_column =
            visible_width(lines.last().map(String::as_str).unwrap_or("")) + 1;
        let viewport_start = lines.len().saturating_sub(height);
        let visible = lines.into_iter().skip(viewport_start).collect::<Vec<_>>();
        let cursor_row = cursor_absolute_row
            .saturating_sub(viewport_start)
            .saturating_add(1)
            .clamp(1, height);
        return (visible, cursor_row, cursor_absolute_column);
    }
```

Match the exact return-tuple shape of the dialogs branch above it (`production.rs:7486+`) — if that branch clamps or offsets the column differently, mirror it verbatim rather than this sketch.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mycel-cli provider_manager_view_marks` then `cargo test -p mycel-cli`
Expected: PASS (12559 still pending rewrite)

- [ ] **Step 5: Commit**

```bash
git add crates/mycel-cli/src/production.rs
git commit -m "feat(tui): render provider manager dialog in the center view"
```

---

### Task 6: Rewrite the old contract test end to end

**Files:**
- Modify: `crates/mycel-cli/src/production.rs:12558-12591` (`interactive_provider_list_restores_terminal_and_resumes_the_session` — rename + rewrite; it pins the teardown contract this plan removes)
- Test: same file (this task IS the test)

**Interfaces:**
- Consumes: the full wired path from Tasks 3-5 via `run_interactive_with_terminal` + `MemoryBackend::scripted` (`production.rs:12574` shows the pattern).
- Produces: `interactive_provider_list_opens_dialog_without_relaunching` — the new pinned contract.

- [ ] **Step 1: Rewrite the test (failing first is satisfied by 12559 being red since Task 3)**

Replace the body of the old test with:

```rust
    #[test]
    fn interactive_provider_list_opens_dialog_without_relaunching() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("mycel");
        fs::create_dir_all(&home).expect("MYCEL_HOME");
        fs::write(home.join(CONFIG_FILE), config()).expect("provider config");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/provider list\r".to_vec()),
            // esc in kitty CSI-u form: a lone \x1b would sit buffered in the
            // decoder and never close the dialog in a scripted backend
            BackendEvent::Input(b"\x1b[27u".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_interactive_with_terminal(
                &interactive(SessionSelection::New, PermissionMode::Auto),
                &mut driver,
            )
            .expect("interactive provider dialog");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(
            rendered.contains("local · openai · 1 model · configured · default"),
            "dialog row must render: {rendered:?}",
        );
        assert!(rendered.contains("add a provider"), "{rendered:?}");
        assert!(
            !rendered.contains("ID\tTYPE"),
            "the out-of-TUI list formatter must not run: {rendered:?}",
        );
        assert!(!rendered.contains("test-key"), "{rendered:?}");
        assert!(transport.requests.lock().expect("requests").is_empty());
    }
```

The `ID\tTYPE` assertion is the discriminating check: that header is emitted only by `format_provider_list` on the teardown path, so its absence proves the session was never torn down for the list. Keep the `test-key` redaction assertion from the old test — the dialog must not leak secrets either.

- [ ] **Step 2: Run the suite**

Run: `cargo test -p mycel-cli`
Expected: entire suite PASS — nothing pending anymore

- [ ] **Step 3: Run the remaining global gates**

Run: `cargo clippy -p mycel-cli -- -D warnings && cargo fmt --check && cargo check`
Expected: clean. Fix anything flagged before committing.

- [ ] **Step 4: Commit**

```bash
git add crates/mycel-cli/src/production.rs
git commit -m "test(cli): pin /provider list to the in-tui dialog contract"
```

---

## Post-plan verification (main loop, not a task for the executor)

- Manual surface check: run the release build in a real terminal, type `/provider` — dialog appears with no flash, esc dismisses, transcript scrollback intact. The PTY repro script (`scratchpad/repro_provider.py` from the 2026-08-29 debugging session) can re-verify: alt-screen exit count after `/provider` submit must be 0.
- `marko` on the full diff (>50 lines, non-trivial) before /ship.
- Update `ARCHITECTURE.md` if it documents the /provider flow.
