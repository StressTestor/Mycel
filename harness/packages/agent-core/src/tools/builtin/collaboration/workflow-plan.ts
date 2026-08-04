import { createHash } from 'node:crypto';
import { open } from 'node:fs/promises';
import { join } from 'pathe';
import { z } from 'zod';

const WORKFLOW_NAME = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const WORKFLOW_TASK_ID = /^[a-z][a-z0-9_-]*$/;
const ARG_REFERENCE = /\{\{arg:([a-zA-Z][a-zA-Z0-9_-]*)\}\}/g;
const RESULT_REFERENCE = /\{\{result:([a-z][a-z0-9_-]*)\}\}/g;
const VALID_RESERVED_REFERENCE = /\{\{(?:arg:[a-zA-Z][a-zA-Z0-9_-]*|result:[a-z][a-z0-9_-]*)\}\}/g;
const RESERVED_REFERENCE_START = /\{\{(?:arg|result):/;

export const MAX_WORKFLOW_PHASES = 32;
export const MAX_WORKFLOW_TASKS = 128;
export const MAX_WORKFLOW_TASKS_PER_PHASE = 64;
export const MAX_EXPANDED_WORKFLOW_PROMPT_CHARS = 200_000;
export const MAX_SAVED_WORKFLOW_BYTES = 1024 * 1024;
export const MAX_WORKFLOW_ARGS = 64;
export const MAX_WORKFLOW_ARG_BYTES = 100_000;

export const WorkflowPlanTaskSchema = z
  .object({
    id: z
      .string()
      .trim()
      .min(1)
      .max(64)
      .regex(WORKFLOW_TASK_ID, 'Task id must start with a letter and use letters, digits, _ or -.'),
    description: z.string().trim().min(1).max(160),
    prompt: z.string().trim().min(1).max(100_000),
    subagent_type: z.string().trim().min(1).max(64).optional(),
  })
  .strict();

export type WorkflowPlanTask = z.infer<typeof WorkflowPlanTaskSchema>;

export const WorkflowPlanPhaseSchema = z
  .object({
    title: z.string().trim().min(1).max(120),
    tasks: z.array(WorkflowPlanTaskSchema).min(1).max(MAX_WORKFLOW_TASKS_PER_PHASE),
  })
  .strict();

export type WorkflowPlanPhase = z.infer<typeof WorkflowPlanPhaseSchema>;

export const WorkflowPlanSchema = z
  .object({
    version: z.literal(1),
    name: z.string().trim().min(1).max(80).regex(WORKFLOW_NAME),
    description: z.string().trim().min(1).max(240),
    phases: z.array(WorkflowPlanPhaseSchema).min(1).max(MAX_WORKFLOW_PHASES),
  })
  .strict()
  .superRefine((plan, ctx) => {
    const taskIds = new Map<string, number>();
    let taskCount = 0;
    for (const [phaseIndex, phase] of plan.phases.entries()) {
      taskCount += phase.tasks.length;
      for (const task of phase.tasks) {
        const previousPhase = taskIds.get(task.id);
        if (previousPhase !== undefined) {
          ctx.addIssue({
            code: 'custom',
            message: `Task id "${task.id}" is duplicated; first used in phase ${String(previousPhase + 1)}.`,
            path: ['phases', phaseIndex, 'tasks'],
          });
          continue;
        }
        taskIds.set(task.id, phaseIndex);
      }
    }
    if (taskCount > MAX_WORKFLOW_TASKS) {
      ctx.addIssue({
        code: 'custom',
        message: `Workflow supports at most ${String(MAX_WORKFLOW_TASKS)} tasks.`,
        path: ['phases'],
      });
    }
    for (const [phaseIndex, phase] of plan.phases.entries()) {
      for (const [taskIndex, task] of phase.tasks.entries()) {
        for (const resultId of references(task.prompt, RESULT_REFERENCE)) {
          const resultPhase = taskIds.get(resultId);
          if (resultPhase === undefined) {
            ctx.addIssue({
              code: 'custom',
              message: `Task "${task.id}" references unknown result "${resultId}".`,
              path: ['phases', phaseIndex, 'tasks', taskIndex, 'prompt'],
            });
          } else if (resultPhase >= phaseIndex) {
            ctx.addIssue({
              code: 'custom',
              message:
                `Task "${task.id}" may only reference results from earlier phases; ` +
                `"${resultId}" is in phase ${String(resultPhase + 1)}.`,
              path: ['phases', phaseIndex, 'tasks', taskIndex, 'prompt'],
            });
          }
        }
        if (RESERVED_REFERENCE_START.test(task.prompt.replaceAll(VALID_RESERVED_REFERENCE, ''))) {
          ctx.addIssue({
            code: 'custom',
            message:
              `Task "${task.id}" contains malformed reserved placeholder syntax. ` +
              'Use {{arg:key}} or {{result:task_id}}.',
            path: ['phases', phaseIndex, 'tasks', taskIndex, 'prompt'],
          });
        }
      }
    }
  });

export type WorkflowPlan = z.infer<typeof WorkflowPlanSchema>;
export type WorkflowArgValue = string | number | boolean;

export interface ResolvedWorkflowPlan {
  readonly plan: WorkflowPlan;
  readonly source: 'inline' | 'saved';
  readonly sourcePath?: string;
  readonly contentSha256: string;
}

export async function resolveWorkflowPlan(input: {
  readonly plan?: WorkflowPlan;
  readonly name?: string;
  readonly args?: Readonly<Record<string, WorkflowArgValue>>;
  readonly kimiHomeDir: string;
  /** Optional session-specific worker ceiling; the parent agent is not counted. */
  readonly maxTasks?: number;
}): Promise<ResolvedWorkflowPlan> {
  const loaded =
    input.plan === undefined
      ? await loadSavedWorkflow(input.name!, input.kimiHomeDir)
      : {
          plan: input.plan,
          source: 'inline' as const,
          contentSha256: sha256(JSON.stringify(input.plan)),
        };
  const plan = applyWorkflowArgs(loaded.plan, input.args ?? {});
  const parsedPlan = WorkflowPlanSchema.parse(plan);
  enforceSessionTaskLimit(parsedPlan, input.maxTasks);
  return {
    ...loaded,
    // Re-parse after argument substitution so an argument can never smuggle a
    // new forward/unknown result reference past the original plan validation.
    plan: parsedPlan,
  };
}

function enforceSessionTaskLimit(plan: WorkflowPlan, maxTasks: number | undefined): void {
  if (maxTasks === undefined) return;
  if (!Number.isSafeInteger(maxTasks) || maxTasks < 1 || maxTasks > MAX_WORKFLOW_TASKS) {
    throw new RangeError(
      `Workflow agent limit must be an integer from 1 to ${String(MAX_WORKFLOW_TASKS)}.`,
    );
  }
  const taskCount = plan.phases.reduce((count, phase) => count + phase.tasks.length, 0);
  if (taskCount > maxTasks) {
    throw new Error(
      `This session permits at most ${String(maxTasks)} workflow subagents; ` +
        `plan "${plan.name}" declares ${String(taskCount)}.`,
    );
  }
}

export function resolveWorkflowTaskPrompt(
  prompt: string,
  results: ReadonlyMap<string, string>,
): string {
  const expanded = prompt.replaceAll(RESULT_REFERENCE, (_match, id: string) => {
    const result = results.get(id);
    if (result === undefined) {
      throw new Error(`Workflow result "${id}" is unavailable.`);
    }
    return result;
  });
  if (expanded.length > MAX_EXPANDED_WORKFLOW_PROMPT_CHARS) {
    throw new Error(
      `Expanded workflow prompt exceeds ${String(MAX_EXPANDED_WORKFLOW_PROMPT_CHARS)} characters.`,
    );
  }
  return expanded;
}

function applyWorkflowArgs(
  plan: WorkflowPlan,
  args: Readonly<Record<string, WorkflowArgValue>>,
): WorkflowPlan {
  const referenced = new Set<string>();
  const argEntries = Object.entries(args);
  if (argEntries.length > MAX_WORKFLOW_ARGS) {
    throw new Error(`Workflow supports at most ${String(MAX_WORKFLOW_ARGS)} arguments.`);
  }
  const argBytes = argEntries.reduce((total, [key, value]) => {
    return total + Buffer.byteLength(key) + Buffer.byteLength(String(value));
  }, 0);
  if (argBytes > MAX_WORKFLOW_ARG_BYTES) {
    throw new Error(
      `Workflow arguments exceed ${String(MAX_WORKFLOW_ARG_BYTES)} UTF-8 bytes.`,
    );
  }
  for (const [key, value] of argEntries) {
    if (typeof value === 'string' && RESERVED_REFERENCE_START.test(value)) {
      throw new Error(`Workflow argument "${key}" may not contain reserved placeholders.`);
    }
  }
  const phases = plan.phases.map((phase) => ({
    ...phase,
    tasks: phase.tasks.map((task) => ({
      ...task,
      prompt: task.prompt.replaceAll(ARG_REFERENCE, (_match, key: string) => {
        referenced.add(key);
        if (!(key in args)) {
          throw new Error(`Workflow argument "${key}" is required but was not provided.`);
        }
        return String(args[key]);
      }),
    })),
  }));
  const unused = Object.keys(args).filter((key) => !referenced.has(key));
  if (unused.length > 0) {
    throw new Error(`Unused workflow argument${unused.length === 1 ? '' : 's'}: ${unused.join(', ')}.`);
  }
  return { ...plan, phases };
}

async function loadSavedWorkflow(
  name: string,
  kimiHomeDir: string,
): Promise<Omit<ResolvedWorkflowPlan, 'plan'> & { readonly plan: WorkflowPlan }> {
  const sourcePath = join(kimiHomeDir, 'workflows', `${name}.json`);
  let text: string;
  let handle: Awaited<ReturnType<typeof open>> | undefined;
  try {
    handle = await open(sourcePath, 'r');
    const buffer = Buffer.alloc(MAX_SAVED_WORKFLOW_BYTES + 1);
    const { bytesRead } = await handle.read(buffer, 0, buffer.length, 0);
    if (bytesRead > MAX_SAVED_WORKFLOW_BYTES) {
      throw new Error(
        `Saved workflow exceeds ${String(MAX_SAVED_WORKFLOW_BYTES)} bytes.`,
      );
    }
    text = buffer.toString('utf8', 0, bytesRead);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`Could not read saved workflow "${name}" at ${sourcePath}: ${message}`, {
      cause: error,
    });
  } finally {
    await handle?.close();
  }
  let json: unknown;
  try {
    json = JSON.parse(text);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`Saved workflow "${name}" is not valid JSON: ${message}`, { cause: error });
  }
  const parsed = WorkflowPlanSchema.safeParse(json);
  if (!parsed.success) {
    throw new Error(`Saved workflow "${name}" is invalid: ${z.prettifyError(parsed.error)}`);
  }
  if (parsed.data.name !== name) {
    throw new Error(
      `Saved workflow file "${name}.json" declares name "${parsed.data.name}"; the names must match.`,
    );
  }
  return {
    plan: parsed.data,
    source: 'saved',
    sourcePath,
    contentSha256: sha256(text),
  };
}

function references(value: string, expression: RegExp): string[] {
  return Array.from(value.matchAll(expression), (match) => match[1]!);
}

function sha256(value: string): string {
  return createHash('sha256').update(value).digest('hex');
}
