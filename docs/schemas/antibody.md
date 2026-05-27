# antibody schema

status: locked for v0.1 design

date: 2026-05-27

## Rust shape

```rust
struct Antibody {
  id: Uuid,
  signature: Signature,
  source: AntibodySource,      // sentinel_block | failed_run | manual
  severity: Severity,          // info | warn | refuse
  confidence: Confidence,      // solid | directional | vibes
  refusal_mode: RefusalMode,   // hard | soft | log_only
  remediation: String,
  examples: Vec<String>,       // ≤3 minimal repros
  created_at: DateTime,
  expires_at: Option<DateTime>,
  hit_count: u32,
}

struct Signature {
  error_class: Option<String>,
  file_pattern: Option<Glob>,
  agent_role: Option<String>,
  tool_pattern: Option<String>,
  scope: SignatureScope,        // project | global | personal
}
```

minimum viable signature: one or more signature fields populated.

matching rule: multiple populated fields are an AND match. empty fields are wildcards.

## Sentinel-derived antibody source fields

lineage: see `docs/schemas/sentinel-fields.md`.

Sentinel v0.1 source fields stable enough for first-class ingestion:

| Sentinel field | Mycel mapping | notes |
| --- | --- | --- |
| `timestamp` | source event time | keep distinct from `Antibody.created_at` |
| `tool_name` | `Signature.tool_pattern` | exact tool-name pattern in v0.1 |
| `action` | `severity`, `refusal_mode` | `block -> refuse/hard`, `warn -> warn/soft`, `allow -> info/log_only` |
| `mode` | source metadata | useful for confidence and replay context |

Sentinel fields retained as source metadata in v0.1:

| Sentinel field | metadata use | notes |
| --- | --- | --- |
| `reason` | remediation seed | freeform policy text |
| `matched_rule` | rule lineage | formatted string; parse into typed fields only after Sentinel schema work |

## Deferred Sentinel ingestion fields

- `matched_rule` rule kind and rule pattern.
- derived `Signature.file_pattern` from `deny.paths`.
- derived command classes from `deny.commands`.
- derived secret classes from `deny.secrets`.
- normalized extracted command and paths, once Sentinel can emit them safely.

## Source notes

Sentinel currently emits one JSONL audit event type for block, warn, and allow decisions. there is no separate block-only struct.

Sentinel should add an audit schema version and typed rule identity before Mycel treats rule patterns as first-class source fields. **confidence: directional. load-bearing.**
