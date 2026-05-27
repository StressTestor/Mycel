# 0004: skill interop

status: proposed

date: 2026-05-27

## context

mycel skills should move in basic form between Mycel, Hermes, and OpenClaw where possible. **confidence: directional. load-bearing.**

OpenClaw has native plugin manifests through `openclaw.plugin.json`, plus compatible bundle layout detection. **confidence: solid. source-checked 2026-05-27.**

Hermes Agent has a skill lifecycle with background review and active/stale/archive curation. **confidence: solid. source-checked 2026-05-27.**

mycel adds ecology-aware features that those exports should not pretend to support. **confidence: directional. load-bearing.**

## decision

support **both**:

- Mycel-native skill manifests for ecology-aware behavior. **confidence: directional. load-bearing.**
- agentskills.io-compatible or system-compatible export shapes for basic transfer. **confidence: directional.**

exports to Hermes or OpenClaw should degrade gracefully and drop unsupported ecology fields into metadata, notes, or sidecar files. **confidence: directional. load-bearing.**

## rationale

Mycel-native manifests are needed for antibodies, sclerotia, spores, kin-sharing, and substrate conditions. **confidence: directional. load-bearing.**

interop keeps useful skills portable even when ecological features are unavailable. **confidence: directional.**

graceful degradation is safer than pretending another runtime enforces Mycel policies. **confidence: directional. load-bearing.**

## consequences

- every Mycel-native skill needs an explicit export profile. **confidence: directional.**
- exported skills should declare lost features. **confidence: directional. load-bearing.**
- import should mark unknown ecology metadata as inert until the user upgrades or maps it. **confidence: directional.**
- adapters need fixture tests against real examples from Hermes and OpenClaw. **confidence: directional.**

## unresolved

- exact Mycel-native manifest schema. **confidence: directional. load-bearing.**
- whether agentskills.io compatibility is a strict target or a best-effort bridge. **confidence: directional.**
- how to represent antibodies and wake conditions in non-Mycel exports. **confidence: directional.**
- whether OpenClaw export should prefer plugin bundles, skills, or both. **confidence: directional.**
