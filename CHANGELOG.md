# changelog

## v0.2.0

the branding release. mycel got a face, and the gate got sealed shut.

### security

- **sealed the same-session self-disarm.** the gate now governs every tool, not just Bash, and a compiled protected-path floor blocks any write to its own binary, config, or substrate. the floor runs before the db even opens and canonicalizes the target, so `~` / relative / symlink / case respells can't dodge it. a truncated or empty-schema db now fail-closes instead of allowing everything. the file-write and truncated-db disarm routes are both closed. a Bash-command write to those paths is still a documented residual, waiting on the structured-parse work.

### branding

- **meet mycel:** a friendly amanita mushroom wearing a "deny by default" patch, rooted in a glowing mycelial network. warm to use, poisonous to anything trying to disarm the gate.
- **the launch screen is a card now.** a shaded block-mushroom logo on the left (designed by kimi k3, running headless inside the harness it's branding), identity and status on the right, a red "deny by default" tagline, a tip that rotates each launch, and command hints.
- 🍄 marks mycel's turns in the transcript.
- README hero, favicon, and repo social preview all use the mascot. "deny by default" is the tagline.
- de-vendored the visual identity: mycel's own logo, no provider marketing banner on startup, a role-labeled transcript.

### features

- **governed `claude -p` subagents.** `mycel-delegate` runs a Claude subagent on your subscription, and every command it runs still passes `mycel-gate --claude` fail-closed. delegated work stays under the immunity gate.
- **codex subscription provider.**

### infra

- **fixed CI.** the workflow had never once parsed - an unquoted colon in a step name - so it failed at 0s on every branch since it was written. it runs now, and it's green.
- **VISION.md:** a 14-harness field survey, the honest measured state of the gate, and 12 scope-tiered bets.

full diff: https://github.com/StressTestor/Mycel/compare/v0.1.0...v0.2.0

## v0.1.0

mycel becomes its own coding harness. a fail-closed antibody gate and an m2 learning loop, grafted onto a de-vendored kimi-code body.
