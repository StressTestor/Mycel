# eval fixture format

status: v0.1 harness format

date: 2026-05-28

The v0.1 harness uses typed fixture records with three sections:

| field | type | purpose |
| --- | --- | --- |
| `antibodies` | array of antibody records | seed registry for fail-pattern immunity |
| `evaluations` | array of proposed-run fixtures | expected refuse, warn, or allow outcomes |
| `sentinel_events_jsonl` | JSONL string or file | Sentinel `AuditEvent` ingestion fixtures |

Evaluation fixtures carry:

| field | type | purpose |
| --- | --- | --- |
| `name` | string | stable fixture id |
| `label` | `safe` or `unsafe` | false-positive accounting |
| `gate_scope` | `agent_launch`, `tool_invocation`, or `substrate_mutation` | v0.1 gate-scope coverage |
| `run` | proposed run | fields matched against antibody signatures |
| `expected` | `refuse`, `warn`, or `allow` | required evaluator outcome |
| `evaluated_at` | RFC3339 timestamp | time used for expiry checks |
| `tags` | array of strings | optional metrics grouping, including `expiry` |

Example:

```json
{
  "name": "tool-shell-blocks",
  "label": "unsafe",
  "gate_scope": "tool_invocation",
  "run": {
    "error_class": null,
    "file_path": null,
    "agent_role": null,
    "tool_name": "shell",
    "scope": "project"
  },
  "expected": "refuse",
  "evaluated_at": "2026-05-28T09:00:00Z",
  "tags": []
}
```
