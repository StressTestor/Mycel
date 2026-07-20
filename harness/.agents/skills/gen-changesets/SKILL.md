---
name: gen-changesets
description: Use when generating changesets in the Mycel repository, including package selection, bundle impact, bump levels, major confirmation, and changelog wording.
---

# Generate changesets

Mycel uses Changesets to manage the published CLI package:

- `mycel`: the CLI and its bundled web application

All other workspace packages are private implementation packages. Their source can enter the Mycel bundle, but their package names must not appear in release changeset frontmatter.

## Core rules

1. **Inspect the actual diff.** Use `git status` and `git diff --name-only` to identify the changed packages and user-facing behavior.
2. **Target the released package.** When private package code changes behavior shipped by the CLI, list `mycel` and describe the released effect.
3. **Do not list private packages.** Internal `@moonshot-ai/*` packages and the VS Code app are not published by this fork.
4. **Map bundled web changes to Mycel.** Changes under `apps/kimi-web` ship in Mycel's `dist-web`; target `mycel` and prefix the entry with `web: `.
5. **Skip non-release-only work.** Docs, tests, CI, and internal refactors without released behavior usually need no changeset.
6. **Never choose `major` without explicit user confirmation.**

## Workflow

1. Read the diff and identify the behavior that reaches `apps/mycel`.
2. Choose the smallest accurate bump.
3. Create one short kebab-case file under `.changeset/`.
4. Split unrelated release effects into separate changesets.
5. Run `pnpm exec changeset status` from `harness/`.

Format:

```markdown
---
"mycel": patch
---

Describe the user-visible change in one concise English sentence.
```

## Bump levels

| Level | Use it for |
| --- | --- |
| `patch` | Bug fixes, release/build fixes, and small improvements to existing behavior |
| `minor` | A substantial backward-compatible capability users could not use before |
| `major` | Breaking command, configuration, or behavior changes, only after confirmation |

When a change is technically new but small, prefer `patch`. Reserve `minor` for a meaningful new capability.

## Wording rules

- Write changelog entries in English.
- State what changed for the user, not which files, classes, or functions changed.
- Keep the entry to one short sentence. A new feature may add one short usage hint.
- Do not mention private package names, PR numbers, commit hashes, real tokens, account identifiers, or private endpoints.
- Use neutral placeholders such as `example.test` or `YOUR_API_KEY` when an example is necessary.
- Do not claim a behavior the diff and tests do not prove.

## Examples

A user-visible fix implemented in a private runtime package:

```markdown
---
"mycel": patch
---

Fix interrupted model responses leaving later requests in an invalid state.
```

A new CLI capability:

```markdown
---
"mycel": minor
---

Add the `/foo` command to list active sessions. Run `/foo` to use it.
```

A bundled web fix:

```markdown
---
"mycel": patch
---

web: Fix the conversation not scrolling to the newest message after sending.
```

An internal-only test or refactor:

```text
No changeset.
```

## Red flags

- A frontmatter package is anything other than `mycel`.
- A private package change enters the bundle, but its released effect is not represented.
- A web change lacks the `web: ` prefix.
- A new capability has no short usage hint.
- The wording exposes real internal identifiers or describes implementation details instead of user behavior.
- A `major` bump was written without user confirmation.
- `pnpm exec changeset status` reports an unknown package.
