# 0020: Foreign session import as antibody candidates

status: proposed

date: 2026-08-15

## context

Mycel has `mycel export` (session to ZIP) and no path in. Sessions from other
harnesses on the same machine are unreachable.

There is a lot of it. Measured 2026-08-15:

| source | session files | on disk |
|---|---|---|
| Claude Code (`~/.claude/projects`) | 5784 | 699 MB |
| Codex (`~/.codex`) | 1232 | 2.8 GB |
| opencode (`~/.local/share/opencode`) | 983 | 294 MB |
| Grok Build (`~/.grok/sessions`) | 784 | 118 MB |
| kimi-code (`~/.kimi-code`) | 20 | 152 MB |

The substrate currently learns from one live source: `mycel-observe` capturing
`PostToolUseFailure`, ingested as an inert candidate, promoted by a human,
enforced by `mycel-gate` thereafter. Everything before Mycel existed, and
everything that happens in another harness, is invisible to it.

Two framings were considered.

**Import conversations.** Faithful replay of foreign transcripts into Mycel
sessions. This requires per-provider fidelity for tool-call blocks, which every
harness names and nests differently, and it delivers searchable history that is
rarely reopened. High cost, low return.

**Import failures.** Mine the same transcripts for the events the substrate
already models: a command that failed, a permission that was denied, a
correction the operator made. This is what `mycel-observe` does live. The
historical corpus is the same signal, older.

## decision

Import foreign sessions as **inert antibody candidates**, never as sessions.

The target schema already fits. `Signature` is
`{ error_class, file_pattern, agent_role, tool_pattern, command_pattern, scope }`
and the gate matches it against a `ProposedRun` carrying those same fields. A
failed tool call in any transcript yields exactly those: the tool name, the
command, the error text, and usually a path. No conversational structure is
needed to populate them.

### scope

- `mycel import <provider> [--dry-run] [--since <date>] [--limit N]`
- One adapter per provider behind a single `SessionSource` trait: read a file,
  emit zero or more `ObservedFailure { tool, command, error, path, timestamp,
  provider, session_id }`.
- Adapters ship for Claude Code and Codex first; those are the two largest and
  Claude Code's shape (`{type, message:{role, content:[…]}}`) is already known.
  opencode, Grok and kimi follow behind the same trait.
- Each `ObservedFailure` becomes a candidate through the **existing** ingest
  path. Import is a new front door onto `mycel-substrate`, not a second store.

### provenance

`AntibodySource` gains an `Imported { provider, session_id }` variant. A
candidate must always be traceable to the transcript that produced it, and
imported candidates must be distinguishable from `FailedRun` ones observed
locally.

### deduplication

8800 sessions will repeat the same failure many times. `Signature` already has
matching logic (`field_matches`, `glob_field_matches`, `command_matches`) and
`Antibody` already carries `hit_count`. A repeat increments `hit_count` on the
existing candidate rather than creating a new row. Frequency across independent
sessions becomes the ranking signal for what a human reviews first.

### what does not change

Nothing auto-activates. Imported candidates land inert, exactly like observed
ones, and only a human promotes them. Import is bulk *proposal*, and the volume
makes the human gate more important, not less. `--dry-run` reports counts and a
sample without writing.

## non-goals

- Conversation replay, transcript browsing, or resuming a foreign session.
- Tool-call fidelity. The importer reads failures, so it never needs to
  reconstruct a faithful tool-use block.
- Importing Mycel's own sessions; those already flow through `mycel-observe`.
- Any network access. Every source is a local file.

## risks

- **Bulk promotion pressure.** A reviewer facing thousands of candidates may
  rubber-stamp. Mitigation: rank by cross-session `hit_count`, cap what a single
  import surfaces, and keep promotion one-at-a-time.
- **Sensitive content in transcripts.** Foreign sessions contain credentials,
  private repository contents, and third-party data. Candidates store a
  signature and a remediation string, not raw transcript text; `examples` must
  be redacted on the way in, and the importer must never copy a transcript into
  the substrate wholesale.
- **Format drift.** Each provider can change its on-disk shape without notice.
  Adapters fail per-file and report a count of skipped files rather than
  aborting a run.
- **False signatures.** A failure caused by a transient environment problem is
  not a rule. Imported candidates carry lower default `confidence` than locally
  observed ones, since no operator saw them happen.

## implementation notes

- Adapters are read-only and take a path; no adapter may write outside the
  substrate.
- A control-boundary change of this kind needs a test that exercises the real
  binary end to end, per the base prompt: fixture transcripts per provider,
  asserting candidate counts and that nothing is promoted.
- Redaction runs before persistence, with its own tests, and is verified against
  a fixture containing a synthetic credential.
