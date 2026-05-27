# Sentinel block log field inventory

date: 2026-05-27
sentinel-guard version: 0.1.0
sentinel source inspected: `/Users/joesephgrey/conductor/workspaces/sentinel/tripoli`

## Inspection summary

Sentinel currently emits one JSONL audit event type for policy decisions: `AuditEvent` in `src/audit_trail/mod.rs`. it is used from `src/evaluate/mod.rs` after hook input parses and policy evaluation succeeds.

there is no separate block-only log struct. block, warn, and allow decisions share the same event shape.

there is no explicit audit schema version or stability marker. fields below marked stable are stable enough for Mycel v0.1 ingestion because they are present in every current `AuditEvent` emission and have clear semantics in source. this should still be treated as a source contract Mycel pins by fixture.

## Stable fields

| field | type | meaning | presence | notes |
| --- | --- | --- | --- | --- |
| `timestamp` | string, RFC3339 timestamp | event creation time in UTC | always present | clear semantics; no schema version yet, so pin with fixtures |
| `tool_name` | string | tool name after Sentinel hook input normalization | always present | missing hook input becomes `"unknown"` before logging; no schema version yet |
| `action` | string enum | policy decision action: `block`, `warn`, or `allow` | always present | emitted from `Action` display implementation; should be documented as closed enum |
| `mode` | string enum | policy mode: usually `audit` or `enforce` | always present | read from policy config via `engine.mode()`; should be documented as closed enum |

## Unstable fields

| field | type | meaning | why unstable |
| --- | --- | --- | --- |
| `reason` | nullable string | policy-supplied explanation for the decision | freeform human text from policy rules or default messages; good for remediation hints, weak as a signature key |
| `matched_rule` | nullable string | formatted rule identity for the first matching rule | encodes rule kind and pattern in one string, for example `deny.paths: ~/.ssh/*`; useful, but not typed or schema-locked |

## Related non-log output

Sentinel also writes a hook response struct, `HookOutput`, to stdout with camelCase fields:

| field | type | meaning | notes |
| --- | --- | --- | --- |
| `permissionDecision` | optional string | Claude hook decision, currently `deny` for hard block and omitted for allow | response surface, not audit log |
| `reason` | optional string | reason returned to the hook caller | response surface, may diverge from audit `reason` |

## Recommendations for Mycel antibody schema

- direct `Signature` fields:
  - `tool_name` maps to `Signature.tool_pattern` as an exact tool-name pattern for v0.1.
- direct `Antibody` top-level fields:
  - `action` maps to `Antibody.severity`: `allow -> info`, `warn -> warn`, `block -> refuse`.
  - `action` maps to `Antibody.refusal_mode`: `block -> hard`, `warn -> soft`, `allow -> log_only`.
  - `timestamp` maps to source event time in the Sentinel source record, not to `Antibody.created_at`.
  - `mode` maps to Sentinel source metadata and can influence confidence during ingestion.
- metadata blob fields:
  - `reason` should be retained as source metadata and can seed `remediation` when present.
  - `matched_rule` should be retained as source metadata in v0.1.
- deferred to v0.2 ingestion:
  - parsing `matched_rule` into typed rule kind and pattern.
  - deriving `Signature.file_pattern` from `deny.paths`.
  - deriving command-oriented `Signature.error_class` or `Signature.tool_pattern` refinements from `deny.commands`.
  - deriving secret-rule classes from `deny.secrets`.
- Sentinel schema stability work before Mycel relies on it:
  - add an audit schema version.
  - split `matched_rule` into typed fields: `rule_namespace`, `rule_kind`, `rule_pattern`, and optional `rule_id`.
  - include normalized extracted paths and command when safe to log.
  - document `action` and `mode` as closed enums.
