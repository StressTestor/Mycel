import { mkdir, rename, writeFile } from 'node:fs/promises';
import { dirname } from 'pathe';

import { errorMessage, isAbortError } from '../../loop/errors';
import type {
  QueuedSubagentRunResult,
  QueuedSubagentTask,
  SessionSubagentHost,
} from '../../session/subagent-host';
import {
  resolveWorkflowTaskPrompt,
  type ResolvedWorkflowPlan,
  type WorkflowPlanTask,
} from '../../tools/builtin/collaboration/workflow-plan';
import type {
  BackgroundTask,
  BackgroundTaskInfoBase,
  BackgroundTaskSink,
} from './task';

const DEFAULT_SUBAGENT_TYPE = 'coder';
const RESULT_PREVIEW_CHARS = 1_000;

interface WorkflowQueuedTask {
  readonly phaseIndex: number;
  readonly taskIndex: number;
  readonly task: WorkflowPlanTask;
}

interface WorkflowTaskResult {
  readonly id: string;
  readonly description: string;
  readonly phaseIndex: number;
  readonly agentId?: string;
  readonly status: 'completed' | 'failed' | 'aborted';
  readonly error?: string;
  readonly result?: string;
}

interface WorkflowManifestTaskResult {
  readonly id: string;
  readonly description: string;
  readonly phaseIndex: number;
  readonly agentId?: string;
  readonly status: 'completed' | 'failed' | 'aborted';
  readonly error?: string;
  readonly resultPreview?: string;
}

interface WorkflowRunManifest {
  readonly version: 1;
  readonly runId: string;
  readonly workflowName: string;
  readonly description: string;
  readonly source: ResolvedWorkflowPlan['source'];
  readonly sourcePath?: string;
  readonly contentSha256: string;
  readonly status: 'running' | 'completed' | 'failed' | 'timed_out' | 'aborted' | 'lost';
  readonly startedAt: number;
  readonly endedAt: number | null;
  readonly currentPhase: number | null;
  readonly phases: readonly { readonly title: string; readonly taskIds: readonly string[] }[];
  readonly results: readonly WorkflowManifestTaskResult[];
}

export interface WorkflowBackgroundTaskInfo extends BackgroundTaskInfoBase {
  readonly kind: 'workflow';
  readonly runId: string;
  readonly workflowName: string;
  readonly phaseCount: number;
  readonly agentCount: number;
  readonly source: ResolvedWorkflowPlan['source'];
  readonly manifestPath?: string;
}

export interface WorkflowBackgroundTaskOptions {
  readonly runId: string;
  readonly parentToolCallId: string;
  readonly manifestPath?: string;
  readonly subagentTimeoutMs: number;
  readonly timeoutMs: number;
}

export class WorkflowBackgroundTask implements BackgroundTask {
  readonly kind = 'workflow' as const;
  readonly idPrefix = 'workflow';
  readonly timeoutMs: number;

  constructor(
    private readonly resolved: ResolvedWorkflowPlan,
    readonly description: string,
    private readonly subagentHost: SessionSubagentHost,
    private readonly options: WorkflowBackgroundTaskOptions,
  ) {
    this.timeoutMs = options.timeoutMs;
  }

  async start(sink: BackgroundTaskSink): Promise<void> {
    const startedAt = Date.now();
    const results = new Map<string, string>();
    const resultRecords: WorkflowTaskResult[] = [];
    let manifest = this.manifest('running', startedAt, null, null, resultRecords);
    await this.writeManifest(manifest);

    try {
      for (const [phaseIndex, phase] of this.resolved.plan.phases.entries()) {
        sink.signal.throwIfAborted();
        manifest = this.manifest('running', startedAt, null, phaseIndex, resultRecords);
        await this.writeManifest(manifest);
        sink.appendOutput(
          `[workflow phase ${String(phaseIndex + 1)}/${String(this.resolved.plan.phases.length)}] ${phase.title}\n`,
        );

        const queued = phase.tasks.map((task, taskIndex): QueuedSubagentTask<WorkflowQueuedTask> => ({
          kind: 'spawn',
          data: { phaseIndex, taskIndex, task },
          profileName: task.subagent_type ?? DEFAULT_SUBAGENT_TYPE,
          parentToolCallId: this.options.parentToolCallId,
          prompt: workflowAgentPrompt(task, resolveWorkflowTaskPrompt(task.prompt, results)),
          description: task.description,
          swarmIndex: resultRecords.length + taskIndex + 1,
          swarmItem: task.id,
          runInBackground: false,
          detachFromParent: true,
          disabledTools: ['Agent', 'AgentSwarm', 'Workflow'],
          timeout: this.options.subagentTimeoutMs,
          signal: sink.signal,
        }));
        const phaseResults = await this.subagentHost.runQueued(queued);
        const normalized = phaseResults.map(normalizeResult);
        resultRecords.push(...normalized);
        for (const result of phaseResults) {
          if (result.status === 'completed' && result.result !== undefined) {
            results.set(result.task.data.task.id, result.result);
          }
        }
        manifest = this.manifest('running', startedAt, null, phaseIndex, resultRecords);
        await this.writeManifest(manifest);

        const failed = normalized.filter((result) => result.status !== 'completed');
        if (failed.length > 0) {
          const reason =
            `Workflow stopped after phase ${String(phaseIndex + 1)} (${phase.title}): ` +
            `${String(failed.length)} of ${String(normalized.length)} tasks did not complete.`;
          sink.appendOutput(`${renderWorkflowResult(this.resolved.plan.name, resultRecords)}\n`);
          await this.writeManifest(
            this.manifest('failed', startedAt, Date.now(), phaseIndex, resultRecords),
          );
          await sink.settle({ status: 'failed', stopReason: reason });
          return;
        }
      }

      sink.appendOutput(`${renderWorkflowResult(this.resolved.plan.name, resultRecords)}\n`);
      await this.writeManifest(
        this.manifest('completed', startedAt, Date.now(), null, resultRecords),
      );
      await sink.settle({ status: 'completed' });
    } catch (error) {
      const aborted = sink.signal.aborted && (isAbortError(error) || error === sink.signal.reason);
      await this.writeManifest(
        this.manifest(
          aborted ? workflowAbortManifestStatus(sink.signal) : 'failed',
          startedAt,
          Date.now(),
          null,
          resultRecords,
        ),
      );
      if (aborted) {
        await sink.settle({ status: 'killed' });
        return;
      }
      await sink.settle({ status: 'failed', stopReason: errorMessage(error) });
    }
  }

  toInfo(base: BackgroundTaskInfoBase): WorkflowBackgroundTaskInfo {
    return {
      ...base,
      kind: 'workflow',
      runId: this.options.runId,
      workflowName: this.resolved.plan.name,
      phaseCount: this.resolved.plan.phases.length,
      agentCount: this.resolved.plan.phases.reduce((count, phase) => count + phase.tasks.length, 0),
      source: this.resolved.source,
      manifestPath: this.options.manifestPath,
    };
  }

  private manifest(
    status: WorkflowRunManifest['status'],
    startedAt: number,
    endedAt: number | null,
    currentPhase: number | null,
    results: readonly WorkflowTaskResult[],
  ): WorkflowRunManifest {
    return {
      version: 1,
      runId: this.options.runId,
      workflowName: this.resolved.plan.name,
      description: this.resolved.plan.description,
      source: this.resolved.source,
      sourcePath: this.resolved.sourcePath,
      contentSha256: this.resolved.contentSha256,
      status,
      startedAt,
      endedAt,
      currentPhase,
      phases: this.resolved.plan.phases.map((phase) => ({
        title: phase.title,
        taskIds: phase.tasks.map((task) => task.id),
      })),
      results: results.map(toManifestResult),
    };
  }

  private async writeManifest(manifest: WorkflowRunManifest): Promise<void> {
    const path = this.options.manifestPath;
    if (path === undefined) return;
    await mkdir(dirname(path), { recursive: true, mode: 0o700 });
    const tempPath = `${path}.${String(process.pid)}.tmp`;
    await writeFile(tempPath, `${JSON.stringify(manifest, null, 2)}\n`, { mode: 0o600 });
    await rename(tempPath, path);
  }
}

function workflowAbortManifestStatus(signal: AbortSignal): 'timed_out' | 'aborted' {
  return signal.reason === 'Timed out' ? 'timed_out' : 'aborted';
}

function workflowAgentPrompt(task: WorkflowPlanTask, prompt: string): string {
  return [
    prompt,
    '',
    '<workflow-return-contract>',
    `You are workflow task "${task.id}". Your final response is returned to the workflow as data.`,
    'Return a technically complete result. Do not address the end user or ask them questions.',
    '</workflow-return-contract>',
  ].join('\n');
}

function normalizeResult(
  result: QueuedSubagentRunResult<WorkflowQueuedTask>,
): WorkflowTaskResult {
  return {
    id: result.task.data.task.id,
    description: result.task.data.task.description,
    phaseIndex: result.task.data.phaseIndex,
    agentId: result.agentId,
    status: result.status,
    error: result.error,
    result: result.result,
  };
}

function toManifestResult(result: WorkflowTaskResult): WorkflowManifestTaskResult {
  return {
    id: result.id,
    description: result.description,
    phaseIndex: result.phaseIndex,
    agentId: result.agentId,
    status: result.status,
    error: result.error,
    resultPreview:
      result.result === undefined ? undefined : result.result.slice(0, RESULT_PREVIEW_CHARS),
  };
}

function renderWorkflowResult(name: string, results: readonly WorkflowTaskResult[]): string {
  const lines = [`<workflow_result name="${escapeXml(name)}">`];
  for (const result of results) {
    const attrs = [
      `id="${escapeXml(result.id)}"`,
      `status="${result.status}"`,
      result.agentId === undefined ? undefined : `agent_id="${escapeXml(result.agentId)}"`,
    ].filter((value): value is string => value !== undefined);
    lines.push(`  <task ${attrs.join(' ')}>`);
    if (result.error !== undefined) lines.push(`    <error>${escapeXml(result.error)}</error>`);
    if (result.result !== undefined) {
      lines.push(`    <result>${escapeXml(result.result)}</result>`);
    }
    lines.push('  </task>');
  }
  lines.push('</workflow_result>');
  return lines.join('\n');
}

function escapeXml(value: string): string {
  return value
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&apos;');
}
