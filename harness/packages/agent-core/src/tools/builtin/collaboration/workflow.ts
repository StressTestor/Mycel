import { randomUUID } from 'node:crypto';
import { join } from 'pathe';
import { z } from 'zod';

import { WorkflowBackgroundTask, type BackgroundManager } from '../../../agent/background';
import type { BuiltinTool } from '../../../agent/tool';
import { resolveKimiHome } from '../../../config/path';
import type { SessionSubagentHost } from '../../../session/subagent-host';
import { DEFAULT_SUBAGENT_TIMEOUT_MS } from '../../../session/subagent-host';
import { ToolAccesses } from '../../../loop/tool-access';
import type { ExecutableToolContext, ExecutableToolResult, ToolExecution } from '../../../loop/types';
import { toInputJsonSchema } from '../../support/input-schema';
import { matchesGlobRuleSubject } from '../../support/rule-match';
import WORKFLOW_DESCRIPTION from './workflow.md?raw';
import {
  resolveWorkflowPlan,
  WorkflowPlanSchema,
  type WorkflowArgValue,
  type WorkflowPlan,
} from './workflow-plan';

const WORKFLOW_NAME = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const DEFAULT_WORKFLOW_TIMEOUT_MS = 12 * 60 * 60 * 1000;

export const WorkflowToolInputSchema = z
  .object({
    plan: WorkflowPlanSchema.optional().describe('Inline declarative workflow plan.'),
    name: z
      .string()
      .trim()
      .min(1)
      .max(80)
      .regex(WORKFLOW_NAME)
      .optional()
      .describe('Saved workflow name loaded from <MYCEL_HOME>/workflows/<name>.json.'),
    args: z
      .record(z.string(), z.union([z.string(), z.number(), z.boolean()]))
      .optional()
      .describe('Values substituted into {{arg:key}} placeholders. Unused values are rejected.'),
    timeout_ms: z
      .number()
      .int()
      .min(1_000)
      .max(24 * 60 * 60 * 1000)
      .optional()
      .describe('Whole-workflow timeout in milliseconds; defaults to 12 hours.'),
  })
  .strict()
  .superRefine((input, ctx) => {
    if ((input.plan === undefined) === (input.name === undefined)) {
      ctx.addIssue({
        code: 'custom',
        message: 'Provide exactly one of plan or name.',
      });
    }
  });

export type WorkflowToolInput = z.infer<typeof WorkflowToolInputSchema>;

export interface WorkflowToolOptions {
  readonly kimiHomeDir?: string;
  readonly sessionDir?: string;
  /** Optional session-specific worker ceiling; the parent agent is not counted. */
  readonly maxAgents?: number;
  readonly subagentTimeoutMs?: number;
  readonly workflowTimeoutMs?: number;
}

export class WorkflowTool implements BuiltinTool<WorkflowToolInput> {
  readonly name = 'Workflow' as const;
  readonly description: string;
  readonly parameters: Record<string, unknown> = toInputJsonSchema(WorkflowToolInputSchema);

  constructor(
    private readonly subagentHost: SessionSubagentHost,
    private readonly backgroundManager: BackgroundManager,
    private readonly options: WorkflowToolOptions = {},
  ) {
    this.description =
      options.maxAgents === undefined
        ? WORKFLOW_DESCRIPTION
        : `${WORKFLOW_DESCRIPTION}\n\nThis programmatic session permits at most ${String(options.maxAgents)} workflow subagents in total. The parent Mycel agent is not counted.`;
  }

  resolveExecution(args: WorkflowToolInput): ToolExecution {
    const workflowName = args.name ?? args.plan?.name ?? 'workflow';
    return {
      accesses: ToolAccesses.none(),
      description: `Launching workflow: ${workflowName}`,
      display: {
        kind: 'agent_call',
        agent_name: `workflow (${workflowName})`,
        prompt: args.plan?.description ?? `Saved workflow ${workflowName}`,
        background: true,
      },
      approvalRule: this.name,
      matchesRule: (ruleArgs) => matchesGlobRuleSubject(ruleArgs, workflowName),
      execute: (context) => this.execution(args, context),
    };
  }

  private async execution(
    args: WorkflowToolInput,
    context: ExecutableToolContext,
  ): Promise<ExecutableToolResult> {
    try {
      context.signal.throwIfAborted();
      const resolved = await resolveWorkflowPlan({
        plan: args.plan,
        name: args.name,
        args: args.args as Readonly<Record<string, WorkflowArgValue>> | undefined,
        kimiHomeDir: resolveKimiHome(this.options.kimiHomeDir),
        maxTasks: this.options.maxAgents,
      });
      const runId = `wf-${randomUUID()}`;
      const manifestPath =
        this.options.sessionDir === undefined
          ? undefined
          : join(this.options.sessionDir, 'workflows', `${runId}.json`);
      const task = new WorkflowBackgroundTask(
        resolved,
        resolved.plan.description,
        this.subagentHost,
        {
          runId,
          parentToolCallId: context.toolCallId,
          manifestPath,
          subagentTimeoutMs: this.options.subagentTimeoutMs ?? DEFAULT_SUBAGENT_TIMEOUT_MS,
          timeoutMs: args.timeout_ms ?? this.options.workflowTimeoutMs ?? DEFAULT_WORKFLOW_TIMEOUT_MS,
        },
      );
      context.signal.throwIfAborted();
      const taskId = this.backgroundManager.registerTask(task, { detached: true });
      return {
        output: formatWorkflowLaunch(taskId, runId, resolved.plan, manifestPath),
      };
    } catch (error) {
      return {
        output: error instanceof Error ? error.message : String(error),
        isError: true,
      };
    }
  }
}

function formatWorkflowLaunch(
  taskId: string,
  runId: string,
  plan: WorkflowPlan,
  manifestPath?: string,
): string {
  const agentCount = plan.phases.reduce((count, phase) => count + phase.tasks.length, 0);
  return [
    `Workflow "${plan.name}" launched in the background.`,
    `task_id: ${taskId}`,
    `run_id: ${runId}`,
    `phases: ${String(plan.phases.length)}`,
    `agents: ${String(agentCount)}`,
    manifestPath === undefined ? undefined : `manifest: ${manifestPath}`,
    `Use TaskOutput with task_id "${taskId}" to inspect progress or TaskStop to cancel it.`,
  ]
    .filter((line): line is string => line !== undefined)
    .join('\n');
}
