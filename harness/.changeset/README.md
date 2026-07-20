# Changesets

This workspace uses [Changesets](https://github.com/changesets/changesets) to version and release the Mycel CLI.

## Published package

Only one workspace package is published:

| Package | Directory | Artifact |
| --- | --- | --- |
| `mycel` | `apps/mycel` | CLI and bundled web application; installs the `mycel` command |

Every `@moonshot-ai/*` package and the VS Code app are private implementation packages in this fork. Do not put private package names in changeset frontmatter. When private package code changes behavior that ships in the Mycel bundle, assign the changeset to `mycel` and describe the user-visible effect.

## When to add a changeset

Add a changeset when a pull request changes Mycel's user-visible behavior, public CLI surface, configuration, bundled release artifact, or dependency ranges.

Changes that only affect tests, internal documentation, CI, or an implementation detail with no released behavior usually do not need one.

| Change | Changeset |
| --- | --- |
| User-visible fix or small behavior adjustment | `"mycel": patch` |
| Backward-compatible new capability | `"mycel": minor` |
| Breaking command, configuration, or behavior change | Ask a maintainer before using `major` |
| Private package change that enters the Mycel bundle | Assign the user-visible impact to `mycel` |
| Docs-only, tests-only, or CI-only change | Usually none |

## File format

Create a short kebab-case Markdown file in `.changeset/`:

```markdown
---
"mycel": minor
---

Add a concise user-facing description and a short usage hint.
```

Entries must be in English, must not include real tokens or internal identifiers, and should describe what a user can do rather than the internal implementation.

## Verify

From `harness/`, run:

```sh
pnpm exec changeset status
```

The command must complete without unknown-package or ignored-package errors.

## Release flow

After a changeset reaches `main`, `.github/workflows/release.yml` can create or update the release pull request. The release workflow versions `mycel`, updates its changelog, publishes through npm trusted publishing, and creates the corresponding GitHub release.

The changelog GitHub integration is configured for `StressTestor/Mycel` in `.changeset/config.json`.

## Notes

- Stage explicit changeset paths; do not blindly stage the whole directory when other work is present.
- Never add an agent identity or co-author line to changeset text or release commits.
- Private workspace packages are implementation details even when their source is bundled into `mycel`.
- A web UI change that ships in the CLI bundle still targets `mycel`; prefix its entry with `web: `.
