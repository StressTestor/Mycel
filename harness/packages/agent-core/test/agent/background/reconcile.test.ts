/**
 * BackgroundManager reconcile + persistence integration tests.
 */

import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'pathe';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  BackgroundTaskPersistence,
  type BackgroundTaskInfo,
} from '../../../src/agent/background';
import { createBackgroundManager } from './helpers';

let sessionDir: string;
let persistence: BackgroundTaskPersistence;

function persistedProcess(
  overrides: Partial<Extract<BackgroundTaskInfo, { kind: 'process' }>> = {},
): Extract<BackgroundTaskInfo, { kind: 'process' }> {
  return {
    taskId: 'bash-orphan00',
    kind: 'process',
    command: 'npm install',
    description: 'install',
    pid: 99999,
    startedAt: 1_700_000_000,
    endedAt: null,
    exitCode: null,
    status: 'running',
    ...overrides,
  };
}

beforeEach(async () => {
  sessionDir = join(
    tmpdir(),
    `kimi-bg-reconcile-${Date.now()}-${Math.random().toString(36).slice(2)}`,
  );
  await mkdir(sessionDir, { recursive: true });
  persistence = new BackgroundTaskPersistence(sessionDir);
});

afterEach(async () => {
  await rm(sessionDir, { recursive: true, force: true });
});

describe('BackgroundManager — loadFromDisk + reconcile', () => {
  it('loadFromDisk does nothing when persistence is not configured', async () => {
    const { manager } = createBackgroundManager();

    await manager.loadFromDisk();

    expect(manager.list(false)).toEqual([]);
  });

  it('reconciles a previously-running task as lost', async () => {
    await persistence.writeTask(persistedProcess());
    const { agent, manager } = createBackgroundManager({ sessionDir });

    await manager.loadFromDisk();
    await manager.reconcile();

    expect(manager.getTask('bash-orphan00')).toMatchObject({
      taskId: 'bash-orphan00',
      status: 'lost',
    });
    expect(await persistence.readTask('bash-orphan00')).toMatchObject({
      taskId: 'bash-orphan00',
      status: 'lost',
    });
    expect(agent.emittedEvents).toContainEqual({
      type: 'background.task.terminated',
      info: expect.objectContaining({
        taskId: 'bash-orphan00',
        status: 'lost',
      }),
    });
  });

  it('reconciles a running workflow manifest with its authoritative lost task record', async () => {
    const runId = 'wf-00000000-0000-4000-8000-000000000000';
    const manifestDir = join(sessionDir, 'workflows');
    const manifestPath = join(manifestDir, `${runId}.json`);
    await mkdir(manifestDir, { recursive: true });
    await writeFile(
      manifestPath,
      JSON.stringify({ version: 1, runId, status: 'running', endedAt: null, currentPhase: 1 }),
    );
    await persistence.writeTask({
      taskId: 'workflow-orphan00',
      kind: 'workflow',
      runId,
      workflowName: 'review-change',
      phaseCount: 2,
      agentCount: 3,
      source: 'inline',
      manifestPath,
      description: 'Review change',
      status: 'running',
      detached: true,
      startedAt: 1_700_000_000,
      endedAt: null,
    });
    const { manager } = createBackgroundManager({ sessionDir });

    await manager.loadFromDisk();
    await manager.reconcile();

    expect(manager.getTask('workflow-orphan00')).toMatchObject({
      kind: 'workflow',
      status: 'lost',
    });
    expect(JSON.parse(await readFile(manifestPath, 'utf8'))).toMatchObject({
      runId,
      status: 'lost',
      currentPhase: null,
      endedAt: expect.any(Number),
    });
  });

  it('does not reclassify already-terminal tasks', async () => {
    await persistence.writeTask(
      persistedProcess({
        taskId: 'bash-done0000',
        command: 'echo hi',
        description: 'echo',
        pid: 88888,
        endedAt: 1_700_000_010,
        exitCode: 0,
        status: 'completed',
      }),
    );
    await persistence.writeTask(
      persistedProcess({
        taskId: 'bash-running0',
        command: 'sleep 1000',
        description: 'sleep',
        pid: 77777,
      }),
    );
    const { agent, manager } = createBackgroundManager({ sessionDir });

    await manager.loadFromDisk();
    await manager.reconcile();

    expect(await persistence.readTask('bash-done0000')).toMatchObject({
      status: 'completed',
    });
    expect(await persistence.readTask('bash-running0')).toMatchObject({
      status: 'lost',
    });
    expect(agent.emittedEvents).toHaveLength(1);
    expect(agent.emittedEvents[0]).toMatchObject({
      type: 'background.task.terminated',
      info: { taskId: 'bash-running0', status: 'lost' },
    });
  });

  it('list(activeOnly=false) includes ghosts; list(true) excludes them', async () => {
    await persistence.writeTask(
      persistedProcess({
        taskId: 'bash-lost0000',
        command: 'x',
        description: 'd',
        pid: 1,
      }),
    );
    const { manager } = createBackgroundManager({ sessionDir });

    await manager.loadFromDisk();
    await manager.reconcile();

    expect(manager.list(true)).toEqual([]);
    expect(manager.list(false)).toEqual([
      expect.objectContaining({ taskId: 'bash-lost0000', status: 'lost' }),
    ]);
  });

  it('getTask returns ghost when the live process map has no entry', async () => {
    await persistence.writeTask(
      persistedProcess({
        taskId: 'bash-ghost000',
        command: 'x',
        description: 'd',
        pid: 1,
      }),
    );
    const { manager } = createBackgroundManager({ sessionDir });

    await manager.loadFromDisk();
    await manager.reconcile();

    expect(manager.getTask('bash-ghost000')).toMatchObject({
      taskId: 'bash-ghost000',
      status: 'lost',
    });
  });

  it('reconcile emits nothing when no ghosts were loaded', async () => {
    const { agent, manager } = createBackgroundManager({ sessionDir });

    await manager.loadFromDisk();
    await manager.reconcile();

    expect(agent.emittedEvents).toEqual([]);
  });

  it('does not emit duplicate termination events on a second reconcile pass', async () => {
    await persistence.writeTask(
      persistedProcess({
        taskId: 'bash-nodup000',
        command: 'sleep 9999',
        description: 'dedupe check',
        pid: 42,
      }),
    );
    const { agent, manager } = createBackgroundManager({ sessionDir });

    await manager.loadFromDisk();
    await manager.reconcile();
    await manager.reconcile();

    expect(
      agent.emittedEvents.filter(
        (event) => event.type === 'background.task.terminated',
      ),
    ).toHaveLength(1);
  });

  it('restores terminal ghost notifications into context', async () => {
    await persistence.writeTask(
      persistedProcess({
        taskId: 'bash-done0001',
        command: 'echo done',
        description: 'one-shot',
        pid: 42,
        endedAt: 1_700_000_010,
        exitCode: 0,
        status: 'completed',
      }),
    );
    const { agent, manager } = createBackgroundManager({ sessionDir });

    await manager.loadFromDisk();
    await manager.reconcile();

    expect(agent.context.appendUserMessage).toHaveBeenCalledWith(
      [
        expect.objectContaining({
          type: 'text',
          text: expect.stringContaining('task.completed'),
        }),
      ],
      expect.objectContaining({
        kind: 'background_task',
        taskId: 'bash-done0001',
        status: 'completed',
      }),
    );
    expect(agent.emittedEvents).toEqual([]);
  });
});
