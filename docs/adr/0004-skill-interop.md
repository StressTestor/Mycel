# 0004: skill interop

status: proposed

date: 2026-05-27

## context

mycel skills should move in basic form between Mycel, Hermes, and OpenClaw where possible. **confidence: directional. load-bearing.**

OpenClaw has native plugin manifests through `openclaw.plugin.json`, plus compatible bundle layout detection. **confidence: solid. source-checked 2026-05-27.**

Hermes Agent has a skill lifecycle with background review and active/stale/archive curation. **confidence: solid. source-checked 2026-05-27.**

mycel adds ecology-aware features that exported runtimes must avoid claiming without enforcement. **confidence: directional. load-bearing.**

## decision

pull interop adapter design into v0.1 as a parallel track, then harden it in v0.8.

support both:

- Mycel-native skill manifests for ecology-aware behavior.
- agentskills.io-compatible or system-compatible export shapes for basic transfer.

exports to Hermes or OpenClaw should degrade gracefully and drop unsupported ecology fields into metadata, notes, or sidecar files. **confidence: directional. load-bearing.**

## rationale

Mycel-native manifests are needed for antibodies, sclerotia, spores, kin-sharing, and substrate conditions. **confidence: directional. load-bearing.**

interop keeps useful skills portable even when ecological features are unavailable.

graceful degradation is safer than pretending another runtime enforces Mycel policies. **confidence: directional. load-bearing.**

## consequences

- every Mycel-native skill needs an explicit export profile.
- exported skills should declare lost features. **confidence: directional. load-bearing.**
- import should mark unknown ecology metadata as inert until the user upgrades or maps it.
- adapters need fixture tests against real examples from Hermes and OpenClaw.
- v0.1 needs an interop loss matrix before Mycel-native schemas settle. **confidence: directional. load-bearing.**

## unresolved

- exact Mycel-native manifest schema.
- whether agentskills.io compatibility is a strict target or a best-effort bridge.
- how to represent antibodies and wake conditions in non-Mycel exports.
- whether OpenClaw export should prefer plugin bundles, skills, or both.
