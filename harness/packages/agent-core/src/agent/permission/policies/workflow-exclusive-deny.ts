import type { PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult } from '../types';

export class WorkflowExclusiveDenyPermissionPolicy implements PermissionPolicy {
  readonly name = 'workflow-exclusive-deny';

  evaluate(context: PermissionPolicyContext): PermissionPolicyResult | undefined {
    const workflowCount = context.toolCalls.filter((toolCall) => toolCall.name === 'Workflow').length;
    if (workflowCount === 0) return;
    if (workflowCount === 1 && context.toolCalls.length === 1) return;

    return {
      kind: 'deny',
      message:
        workflowCount > 1
          ? 'Workflow must be called one run at a time. Launch one Workflow, wait for its result, then launch another.'
          : 'Workflow must be the only tool call in a model response. Retry with one Workflow call by itself.',
      reason: {
        workflow_tool_calls: workflowCount,
        tool_calls: context.toolCalls.length,
      },
    };
  }
}
