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

matching rule: multiple populated fields are an AND match. empty fields are wildcards. an empty or whitespace-only string counts as unpopulated (a wildcard), the same as a `None` field.

## signature specificity (v0.1.1)

status: locked for v0.1.1 hardening

A signature must carry enough populated, non-empty fields to justify its
severity. This answers the load-bearing open question "what is the minimum
antibody signature that catches repeat failures without blocking adjacent valid
work?" and bounds the over-matching the v0.1 adversarial suite found.

Populated-field count ignores empty and whitespace-only strings (they are
wildcards), and `scope` is not counted (it is always present).

| populated fields | refuse | warn | info |
| ---: | --- | --- | --- |
| 0 | rejected | rejected | rejected |
| 1 | demoted to warn | allowed | allowed |
| ≥ 2 (`MIN_REFUSE_SIGNATURE_FIELDS`) | allowed | allowed | allowed |

- **0 fields → rejected.** An all-wildcard signature is rejected at insertion
  (`MycelError::EmptySignature`); a refuse/hard wildcard would gate every run.
  This extends the original all-empty guard to present-but-empty fields.
- **1 field + refuse → demoted to warn/soft.** A single field is too broad to
  justify a hard refusal that turns a whole tool or path into a permanent
  tripwire. The record still persists as an advisory warn rather than being
  dropped, because single-field warn and info signatures (for example a
  Sentinel block on one tool name) are legitimate.
- **≥ 2 fields → severity is preserved.** An AND match across two or more
  populated fields is specific enough to carry refuse.

The rule is enforced wherever antibodies persist: `AntibodyStore::insert_antibody`,
`AntibodyStore::update_antibody`, and therefore the MCP `insert_antibodies`
path, which delegates to the store. Demotion is silent and deterministic; it
rewrites only the persisted `severity` and `refusal_mode`.

For Sentinel ingestion, an `AuditEvent` whose `tool_name` is empty or
whitespace-only is rejected (`MycelError::EmptyToolName`) instead of being
normalized into a refuse-capable antibody with an empty (wildcard) `tool_pattern`.

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
