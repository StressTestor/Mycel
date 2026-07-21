/**
 * `/delegate <task>` (alias `/handoff`) - hand a task to a governed `claude -p`
 * subagent; the gate stays closed. The task rides as a SINGLE argv element to a
 * `spawn` (no shell), so quotes/backticks/`$()` in the task are inert bytes.
 * Governance = every Bash the subagent runs passes mycel-gate --claude
 * fail-closed. Output is buffered-final (the script does not stream-json).
 */

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';

import { formatErrorMessage } from '#/tui/utils/event-payload';

import type { SlashCommandHost } from '../dispatch';
import { foldLine, mountPanel, painters } from './panel';
import { resolveSubstratePaths } from './substrate-runner';

const MAX_BODY_LINES = 40;
/** Bound a chatty subagent so it can't OOM the TUI. */
const MAX_OUTPUT_BYTES = 4 * 1024 * 1024;
/** Backstop so a hung delegate can't run forever with no exit. Generous - a
 * real delegated task can take minutes; this only kills a genuine hang. */
const DELEGATE_TIMEOUT_MS = 10 * 60 * 1000;

export interface DelegateResultOptions {
  readonly task: string;
  readonly stdout: string;
  readonly stderr?: string;
  readonly exitCode: number;
}

function decodeExitReason(exitCode: number, stderr: string): string {
  switch (exitCode) {
    case 2:
      return 'claude not on PATH - install Claude Code to delegate';
    case 3:
      return 'governance config missing - run install.sh (refuses to spawn ungoverned)';
    case 1:
      return 'no task reached the subagent';
    default:
      return foldLine(stderr).length > 0 ? foldLine(stderr) : `subagent exited with code ${exitCode}`;
  }
}

export function buildDelegateResultLines(options: DelegateResultOptions): string[] {
  const { accent, value, muted, error } = painters();
  const lines: string[] = [
    accent('delegate'),
    `  ${muted('task    ')}${value(foldLine(options.task))}`,
    `  ${muted('gate    ')}${value('mycel-gate --claude')} ${muted('· fail-closed')}`,
    `  ${muted('-')}`,
  ];

  if (options.exitCode !== 0) {
    lines.push(`  ${error(decodeExitReason(options.exitCode, options.stderr ?? ''))}`);
    lines.push(`  ${muted('-')}`);
    lines.push(`  ${muted('delegation failed')}`);
    return lines;
  }

  const bodyLines = options.stdout.split('\n').filter((line, index, all) => {
    // Drop a single trailing empty line from the final newline.
    if (line.length === 0 && index === all.length - 1) return false;
    return true;
  });

  if (bodyLines.length === 0) {
    lines.push(`  ${muted('(subagent returned no output)')}`);
  } else {
    const shown = bodyLines.slice(0, MAX_BODY_LINES);
    for (const line of shown) lines.push(`  ${value(foldLine(line))}`);
    if (bodyLines.length > shown.length) {
      lines.push(`  ${muted(`+${bodyLines.length - shown.length} more lines`)}`);
    }
  }

  lines.push(`  ${muted('-')}`);
  lines.push(`  ${muted(`done · subagent returned ${bodyLines.length} lines`)}`);
  return lines;
}

export function handleDelegateCommand(host: SlashCommandHost, args: string): Promise<void> {
  const task = args.trim();
  if (task.length === 0) {
    host.showError('Usage: /delegate <task>');
    return Promise.resolve();
  }

  const { delegateBinPath } = resolveSubstratePaths();
  if (!existsSync(delegateBinPath)) {
    host.showError(`mycel-delegate not found at ${delegateBinPath} - run install.sh (drive unmounted?)`);
    return Promise.resolve();
  }

  const spinner = host.showProgressSpinner('Delegating to a governed subagent… gate stays closed');

  return new Promise<void>((resolve) => {
    let settled = false;
    let stdout = '';
    let stderr = '';
    let truncated = false;

    const finish = (): void => {
      if (settled) return;
      settled = true;
      resolve();
    };

    let child;
    try {
      child = spawn(delegateBinPath, [task], {
        env: process.env,
        stdio: ['ignore', 'pipe', 'pipe'],
        timeout: DELEGATE_TIMEOUT_MS,
        killSignal: 'SIGTERM',
      });
    } catch (spawnError) {
      spinner.stop({ ok: false, label: 'could not launch mycel-delegate.' });
      host.showError(formatErrorMessage(spawnError));
      finish();
      return;
    }

    child.stdout?.setEncoding('utf8');
    child.stderr?.setEncoding('utf8');

    child.stdout?.on('data', (chunk: string) => {
      if (stdout.length < MAX_OUTPUT_BYTES) {
        stdout += chunk;
      } else {
        truncated = true;
      }
      const lastLine = chunk
        .split('\n')
        .map((line) => line.trim())
        .filter((line) => line.length > 0)
        .at(-1);
      if (lastLine !== undefined) spinner.setLabel(lastLine);
    });

    child.stderr?.on('data', (chunk: string) => {
      if (stderr.length < MAX_OUTPUT_BYTES) stderr += chunk;
    });

    child.on('error', (childError) => {
      spinner.stop({ ok: false, label: 'could not launch mycel-delegate.' });
      host.showError(formatErrorMessage(childError));
      finish();
    });

    child.on('close', (code, signal) => {
      if (settled) return;
      const timedOut = code === null && signal === 'SIGTERM';
      const exitCode = code ?? 1;
      if (timedOut) {
        spinner.stop({ ok: false, label: 'delegate timed out - killed after 10m.' });
      } else if (exitCode === 0) {
        spinner.stop({ ok: true, label: 'handed off · gate held.' });
      } else {
        spinner.stop({ ok: false, label: 'delegation failed.' });
      }
      const body = truncated ? `${stdout}\n…(output truncated)` : stdout;
      mountPanel(host, ' Delegate ', () =>
        buildDelegateResultLines({ task, stdout: body, stderr, exitCode }),
      );
      finish();
    });
  });
}
