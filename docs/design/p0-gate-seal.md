# P0: seal the self-disarmable, Bash-only gate

harden-design synthesis (25-agent adversarial pass, 2026-07-20). read-only design; the build is harden-pr-series.

## summary

The nine items collapse into five dependency-ordered changes plus two drops. Every review verified the same root cause against /Volumes/T7/Mycel: the gate wires matcher=\"Bash\" only and hardcodes ProposedRun.file_path=None, so Write/Edit/MCP never reach the gate and no file_pattern antibody can fire — and where a path IS fed, it's a raw, model-controlled string matched against an ANCHORED glob (glob_to_regex adds ^...$), so path-scoped protection is respell-evadable (relative/~/./symlink/case). The load-bearing seal is therefore four ordered pieces: (1) route write-class tools + extract file_path (merged from the catch-all-matcher and multi-key-extraction items), (2) canonicalize the path before matching, (3) a compiled-const protected-path floor evaluated PRE-db and read-only (merged from the two denylist items) so an exists-but-EMPTY db can't allow a self-disarm, (4) fail-closed on unextractable mutators; plus a trimmed scope/VISION-honesty item that fixes the source-of-truth (compiled floor authoritative, config ADD-only) and states the Bash-write + MCP-target gaps plainly. A correction runs through all of them: the \"evaluate the floor before any allow-rule / ordering is the mechanism\" framing describes an engine that doesn't exist — evaluate_run is most-restrictive-wins via min_by_key, with no store allow-rules — so the floor must be a standalone pre-store deny, and its Refuse emission can't reuse evaluation.refusal(). Dropped: the Bash tripwire (own review recommends omit — net-negative, bricks the agent's own PATH usage and misses the naive spelling on case-insensitive APFS) and the standalone regression-tests item (mis-scoped feature build with dead sentinel-guard refs and tautologically-green tests; its assertions fold into items 1-4). Ranking is dependency + value order: item 1 unblocks everything, item 2 is what makes the file_path floor real, item 3 is the actual anti-disarm, item 4 is the backstop, item 5 is honest documentation.

## dependency-ordered pieces

### 1. Route write-class tools through the gate + extract file_path (merge of "catch-all matcher" + "multi-key file_path extraction")  `[transform]`

**design:** Foundational seal. Today config wires matcher="Bash" only and main.rs hardcodes ProposedRun.file_path=None, so Write/Edit/MCP never reach the gate and no file_pattern antibody can ever fire. Two edits: (1) broaden routing — prefer catch-all matcher="" (matches() short-circuits empty before compiling a regex, so no name list to keep in sync and no typo-fail-open; anchored ^(Bash|Write|Edit)$ only if a measured latency regression forces it, and only behind a compiled floor); (2) populate file_path in main.rs from tool_input, keying `path` FIRST (the real v2 Write/Edit field per write.ts:45/edit.ts:45) then file_path/notebook_path as the --claude-dialect fallback. Reuse the existing file_path:Option<String> field — do NOT add a new ProposedRun field (it is built with explicit struct literals at ~12 sites across mycel-mcp/mycel-cli/sclerotia not in scope, all compile-break). Extraction is an explicit write-class allowlist, not generic: Read/Grep/Glob all carry `path` too and matching has no read/write discriminator, so generic extraction over-blocks reads.

**recommendation:** KEEP — ship FIRST, it unblocks everything. Transform: prefer matcher="", reuse file_path:Option<String>, key `path` first, scope extraction to write-class tools, route absent tool_input to deny not panic. Budget for dormant-antibody activation.

**footguns:**
- Dormant-antibody activation: the moment file_path is populated, every existing file_pattern antibody goes live at once and can newly deny/warn ops that were allowed (FINDINGS.md already flags file-only signatures as over-matching). Gate rollout or regression-test it — this is a behavior change to existing installs, not a pure add.
- Read over-block: extracting `path` for Read/Grep/Glob makes a tool_pattern-less file antibody deny reads of the protected dir; scope extraction to write tools OR require tool_pattern on file antibodies.
- Fail-open via panic: HookPayload.tool_input is Option; a Write with NO tool_input must route to structural deny, never through an unwrap/index that panics (exit 101 = fail-OPEN under --claude, which only blocks on exit 2).
- Catch-all latency: spawns mycel-gate + opens sqlite on EVERY tool call including Read; small but unmeasured, and a missing/corrupt db now fail-closed-blocks Read/Grep too (whole-toolset coupling — document as an install invariant).
- MCP staleness: catch-all routes mcp__* writes through the gate, but reading only `path` catches Github create_or_update_file while missing Supabase apply_migration/execute_sql (query field) — those stay tool_name-only-blockable.

### 2. Canonicalize the write path before matching (evasion-proofing)  `[transform]`

**design:** Required for item 1 to actually protect anything. glob_to_regex anchors ^...$ on the RAW string (lib.rs:1294), and the gate feeds a raw, model-controlled, un-normalized path, so a file-scoped rule like **/.mycel/** is trivially dodged by relative, ./, ~, $HOME, /private-vs-/var aliasing, symlinked-parent, or case/NFC-NFD respelling — the flagship e2e test passes for exactly one hardcoded spelling and gives false confidence. Add a pure resolver: lexically normalize (expand ~ against injected HOME, join relative onto payload cwd, collapse ./.. without escaping root), then resolve_deepest_existing (canonicalize the deepest EXISTING ancestor of a not-yet-created target and re-append the lexical tail — defeats symlinked parents), then fold (NFC + FS-conditional case) applied SYMMETRICALLY to both sides inside Signature::matches. Deny if either the lexical or canonical form matches. Any resolution error on a protected-looking path denies (fail-closed).

**recommendation:** KEEP — ship immediately after item 1; without it the file_path floor is defeatable by string form and the security claim is theater. Transform: fold inside matches(), use payload cwd, deepest-existing-ancestor resolution, FS-conditional case.

**footguns:**
- Shared matcher: Signature::matches is the ONE matcher for gate/mcp/cli/sclerotia; fold must be applied inside matches() to both sides, NOT baked into the stored pattern at load (that regresses the non-gate callers that pass raw values), and you cannot canonicalize a glob root like **/.mycel/** — only concrete-path rules canonicalize.
- Agent cwd: relative `path` canonicalizes against the GATE process cwd, not the project cwd; the payload carries cwd (top-level, snake-cased) — use it, and treat the gate's own current_dir() as the untrusted fallback.
- New-file ENOENT: canonicalize on a new file/dir errors; deepest-existing-ancestor walk is required or you either false-block every new-file Write or reopen a symlink/TOCTOU gap on the appended tail.
- No cross-call cache: mycel-gate is a fresh subprocess per PreToolUse, so "resolve roots once" doesn't exist — canonicalize + ancestor walk + N stats run on every call; keep the walk O(depth).
- Case-fold is FS-dependent: unconditional ASCII-lowercase false-denies distinct paths on case-sensitive volumes (Linux CI, case-sensitive APFS) and mis-fires WARN antibodies — make it volume-conditional; TOCTOU remains a documented residual.

### 3. Compiled-in immovable protected-path denylist, pre-DB, cheapest path (merge of "compiled floor" + "cheapest gate path")  `[transform]`

**design:** The actual anti-disarm. evaluate_run does .unwrap_or(Allow) on an empty antibody set (lib.rs:411), so an exists-but-EMPTY db allows every write — that (not "missing db", which already fail-closes at the exists() guard) is the load-bearing hole. Add a compiled-const protected floor in mycel-core: a standalone pre-store deny check the gate runs BEFORE opening the db, so it holds regardless of substrate state. Config may only ADD (union onto the &'static floor; property-test that no config input shrinks it below the compiled set). Open the db SQLITE_OPEN_READ_ONLY (no create, no migrate) so a gate can never recreate a deleted db and takes no write lock. Roots resolve from the SAME logic that picked the db, INCLUDING the --db override — deriving only from MYCEL_HOME/HOME leaves the actual --db substrate unprotected (the tests themselves pass --db).

**recommendation:** KEEP — the core self-protection. Transform: standalone pre-store check (not "before allow-rule"), narrow rules to the mycel tree, derive roots from the real --db-inclusive resolution, read-only open, keep db-missing block on fall-through. Depends on item 2's canonicalizer.

**footguns:**
- "Before any allow-rule / ordering is the mechanism" describes a nonexistent engine: evaluate_run is most-restrictive-wins via min_by_key on rank (Refuse=0<Warn<Allow), there are no store allow-rules to precede. Implement as a standalone pre-store deny; its Refuse emission can't reuse evaluation.refusal() (which requires a matching store antibody) or it trips the internal-error exit-5 path — mint a synthetic EvaluationMatch with all six required fields incl a phantom antibody_id Uuid.
- Scope rules TIGHT to ~/.mycel|$MYCEL_HOME: broad .git/hooks/** and shell-rc rules break husky/lefthook/pre-commit and dotfiles repos on day one; make any raw-command scan write-aware or `cat ~/.bashrc` gets hard-refused.
- Keep the db-missing block on the NON-protected fall-through: if the denylist "not protected -> allow" returns before the exists() check, `rm mycel.db` disarms every non-protected write.
- Relative/new-file path: same canonicalization dependency as item 2 — a project-relative .git/config resolves against the wrong cwd without agent-cwd; existing test gate.rs:234 sends relative "x" expecting allow, so naive fail-closed-on-ENOENT breaks it.
- Native protected hit must emit exit-0 + permissionDecision:"deny" JSON (the Refuse dialect), NOT a GateError (which maps to bare exit 3/4/5 with no deny JSON on the native lane); read_only open must surface an Err on a wrong-schema/zero-byte db, not silently allow.
- Perf is oversold: only PROTECTED-target writes get cheaper; every ordinary edit and Bash call still opens the db and runs the antibody matcher.

### 4. Fail-closed on unextractable known mutators  `[transform]`

**design:** Backstop that survives a schema drift or renamed key: if tool_name is a known write mutator but no path can be extracted, DENY rather than fall through to allow (inverts today's non_bash_tool_with_no_command_allows). Shares ONE extract_path helper with item 1 so the structural check and the antibody path-matching agree on what "extractable" means (empty/whitespace/non-string = not extractable). A mutator whose target the gate can't read is a blind spot, and blind spots block.

**recommendation:** KEEP (medium value) — fold into item 1's extraction as the fail-closed default rather than a separate lane. Transform: handle None/non-string at the Option layer, resolve the dialect split against the real native schema, soften the path-antibody claims.

**footguns:**
- None/absent tool_input and non-string path (array/number) must map to deny at the Option layer, never into a Value-typed helper that could panic — panic fail-OPENS under --claude.
- Dialect split is incoherent: Write/Edit/MultiEdit/NotebookEdit are ALL Claude-dialect tools with no native-dialect write schema in-repo, so gating only two behind --claude is unfounded; verify what the native/kimi dialect actually emits before treating Write/Edit as always-on mutators, or the "always" branch is dead code / blanket-denies foreign schemas.
- Don't oversell the motivating examples: glob_matches is exact-match without wildcards, so `.env` / `~/.ssh/*` never fire on real absolute paths even after file_path is populated — the fail-closed value is real, the "this unlocks path antibodies" framing is not.
- Re-cased/renamed TOOL name (write, FileWrite) still falls through to allow — this closes the renamed-KEY hole, not the renamed-tool hole; state it so coverage isn't mistaken as complete.

### 5. Scope boundaries: honest VISION ledger + reject config-driven denylist (Approach C)  `[transform]`

**design:** Keep the two load-bearing decisions, drop the ceremony. (1) Source-of-truth: the protected floor is a compiled-in const; config is ADD-only (a config-editable denylist sits at a path the very Write-vector it defends can edit — self-removable, fails its own threat model). (2) Honest coverage tagging in VISION.md and the PR body: state precisely what the seal covers (Bash + Write/Edit path floor) and what it does NOT (Bash-command writes still substring-only = bet 2; MCP write targets under per-server schemas = P1), so the gaps are disclosed not silently read as covered.

**recommendation:** KEEP (trimmed) — the reject-config-C decision and honest VISION row are worth a small item; strip the ceremony. Correct the ordering framing and write the VISION row as the honest partial. Depends on item 2.

**footguns:**
- The "compiled floor evaluated before any store allow-rule / ordering is the mechanism" rationale is wrong for the real min_by_key engine — same correction as item 3; don't propagate it into a code comment a future contributor implements against.
- Do NOT scope canonicalization as "Bash-only": the Write/Edit floor compares raw strings, so it's evadable by ~/abs/relative/.. respelling with no Bash — the VISION row "Write/Edit path floor sealed" over-claims until item 2 lands. Write the row as the true partial.
- Catch-all matcher changes NON-Bash verdicts (tool_pattern/broad-glob antibodies now fire on Read/list) — the non-regression clause must assert non-Bash tools too.
- Cut the triple-verbatim restatement + "ledger credibility is the product" prose; the property test floor∪config⊇floor holds by construction — thin substance dressed as a deliverable.

## dropped

- **Bash-lane self-protection tripwire (labeled best-effort path-token check)** - Own review inverts its value: with install.sh putting ~/.mycel/bin on PATH, a fail-closed DENY on the substring `.mycel/bin/` fires on the agent's OWN legit mycel usage (session-brick, worse than the vector it guards), and macOS APFS case-insensitivity lets the exact naive spelling it targets (`cp evil ~/.Mycel/bin/mycel-gate`) slip a case-sensitive scan. Reviewer recommends option (b) omit; net-negative. Real Bash-write coverage is the structured-parse work (bet 2). Keep only the one-sentence VISION note that the tripwire was considered and deliberately declined.
- **Regression tests + fail-closed continuity for the Write/Edit/NotebookEdit lanes (standalone item)** - Mis-scoped/oversell: titled a regression-test suite but is actually a feature build (extraction + canonicalizer + floor + reordering + config lanes are all NEW code). sentinel-guard isn't wired to the gate at all (mycel-gate depends only on mycel-core) so its listed policy/matcher files are dead refs. Several named RED-first tests are tautologically green today (protected_write_denies_with_missing_db fires EXIT_DB before stdin is even read) and case-fold-denies bakes in the dev Mac's answer (breaks on Linux CI). Its genuine value — the deny-matrix, spelling-collapse, empty-db, and harness-continuity assertions — folds into the test plans of items 1-4 rather than standing as its own PR.

---
last updated 2026-07-20. design only; implementation tracked via harden-pr-series PRs.