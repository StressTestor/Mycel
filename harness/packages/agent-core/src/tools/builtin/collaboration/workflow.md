Run a bounded, declarative multi-agent workflow in the background.

Use this tool only when the user explicitly asks for multi-agent orchestration, invokes a saved
workflow, or enables an orchestration mode such as swarm/Hyphae. Ordinary tasks should not fan
out implicitly.

Provide exactly one source:

- `plan`: an inline version-1 plan. Phases run in order; tasks inside a phase run concurrently.
- `name`: a saved JSON plan from `<MYCEL_HOME>/workflows/<name>.json`.

Task prompts may use `{{arg:key}}` for caller-provided scalar arguments and `{{result:task_id}}`
for output from a task in an earlier phase. Missing, unused, unknown, same-phase, and forward
references fail before work is launched. A failed or timed-out task stops the workflow after its
current phase; it never silently passes verification.

The tool always backgrounds itself. Do not combine it with other tool calls in the same model
response. It returns task/run identifiers immediately; completion is delivered through the normal
background-task notification path.
