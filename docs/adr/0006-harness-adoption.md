# 0006: harness adoption

status: accepted

date: 2026-07-19

## context

mycel needed an agent body: something that runs sessions, drives models, executes tools, and gives the substrate real traffic to gate. building one from scratch was never the plan; ADR-0003 assumed mycel would attach to external harnesses through adapters.

meanwhile we forked MoonshotAI/kimi-code (MIT, TypeScript) to add fail-closed hook gating (`fail_mode = "closed"`, upstream PR #1906). that work proved two things: the hook contract is a real enforcement surface, and we understand the codebase well enough to own it. **confidence: solid. verified in-session 2026-07-18/19.**

## decision

adopt the kimi-code fork as mycel's harness, grafted into this repo at `harness/` with full git history (subtree from fork commit 97f6b5e9).

- **fork = harness, rust = brain.** the substrate, antibodies, evaluation engine, and policy stay in `crates/` (rust). the harness is the TS body. no substrate logic gets rewritten in typescript.
- **split from upstream.** `harness/` diverges permanently. `StressTestor/kimi-code` stays frozen as the PR #1906 head; upstream commits can be cherry-picked in by choice, never by obligation.
- **model-agnostic.** phone-home stripped (telemetry, update checks, marketplace CDN default). no vendor default model. all providers first-class: kimi OAuth, anthropic, openai-compatible (local ollama included), google-genai.
- **enforcement boundary: mycel-gate.** a small rust bin linking mycel-core, wired as a PreToolUse hook with `fail_mode = "closed"`. gate crashed / hung / missing db = command blocked. the gate never creates the substrate db - a deleted db reads as "guard disarmed, block everything", and only the installer initializes it.
- **conversation boundary: MCP.** mycel-mcp exposed to the harness via mcp.json so the model can query and propose (never activate) antibodies.

## amendment to ADR-0003

ADR-0003 said "adapter packages in Python and TypeScript only where interop requires them." the harness is a full TS layer, not a thin adapter. the amendment is scoped: the SUBSTRATE and POLICY core stays rust (the sentinel-pairing rationale survives untouched); the harness is a consumer of that core through the same contracts (hook JSON, MCP) any external harness would use. claude code consumes the substrate through the identical gate contract - the harness is privileged in ownership, not in interface. **confidence: solid by construction.**

## consequences

- two toolchains in one repo (cargo + pnpm). CI runs both.
- upstream security fixes require deliberate cherry-picks; we own every line now.
- the harness's internal `@moonshot-ai/*` package names remain (unpublished here; renaming is churn without behavior).
- `harness/docs` and code comments are english-only; the upstream bilingual docs obligation ended with the split.
- kimi-code's MIT license and attribution are preserved under `harness/`.

## alternatives rejected

| option | why not |
| --- | --- |
| stay a thin fork, attach via hooks only | hooks cannot reach agent lifecycle (hibernate, transfer, spawn) - the later roadmap needs harness ownership |
| rewrite substrate in TS inside the harness | throws away working, tested rust; weakens sentinel pairing; ADR-0003's core rationale still holds |
| greenfield TS harness | months of work to reach what the fork already does |
