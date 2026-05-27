# 0003: language and runtime

status: proposed

date: 2026-05-27

## context

mycel needs a local runtime that can evaluate launch conditions, store durable substrate state, integrate with Sentinel, and expose adapters for other agent systems. **confidence: directional. load-bearing.**

the user already has a Rust security project in Sentinel, published as `sentinel-guard`. **confidence: solid. load-bearing.**

reference systems use TypeScript for OpenClaw and Python for Hermes Agent. **confidence: solid. source-checked 2026-05-27.**

## decision

use **Rust for the core runtime**, with adapter packages in Python and TypeScript only where interop requires them. **confidence: directional. load-bearing.**

## rationale

Rust fits local-first state handling, typed policy evaluation, and Sentinel pairing. **confidence: directional. load-bearing.**

Python is useful for Hermes export and eval workflows. **confidence: directional.**

TypeScript is useful for OpenClaw plugin and manifest export. **confidence: directional.**

keeping adapters thin reduces the chance that interop concerns reshape the substrate model. **confidence: directional. load-bearing.**

## consequences

- initial implementation should start with a small Rust crate boundary, not a large framework. **confidence: directional.**
- adapter contracts should be schema-driven rather than direct internal bindings. **confidence: directional. load-bearing.**
- Python and TypeScript packages should not become required for core local operation. **confidence: directional. load-bearing.**
- cross-language tests will matter once adapters exist. **confidence: directional.**

## alternatives

| option | result |
| --- | --- |
| all Rust | clean core, weaker early interop ergonomics. **confidence: directional.** |
| all Python | faster experiments, weaker fit with Sentinel and policy runtime. **confidence: directional.** |
| all TypeScript | better OpenClaw alignment, weaker Sentinel alignment. **confidence: directional.** |
| hybrid with Rust core | best balance if schemas stay tight. **confidence: directional. load-bearing.** |

## unresolved

- exact crate split. **confidence: directional.**
- whether to expose an MCP server, CLI only, or both. **confidence: directional.**
- whether adapter packages live in this repo or separate repos after v0.8. **confidence: directional.**
