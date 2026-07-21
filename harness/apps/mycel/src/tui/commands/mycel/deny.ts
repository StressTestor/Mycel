/**
 * `/deny <command-pattern>` (aliases `/refuse`, `/block`) - teach the gate to
 * refuse a command now by writing a curated hard-refuse antibody into the
 * substrate. Everything after `/deny ` is one raw value passed as a single
 * `--command-pattern` argv element (no shell, no injection).
 */

import type { SlashCommandHost } from '../dispatch';
import { foldLine, mountPanel, painters } from './panel';
import { resolveSubstratePaths, runSubstrate } from './substrate-runner';
import type { AntibodyAddResult } from './types';

const REMEDIATION =
  'Denied by operator via /deny. Do not run this command; use an approved alternative.';

export interface DenyConfirmOptions {
  readonly pattern: string;
  readonly id: string;
  readonly scope: string;
}

export function buildDenyConfirmLines(options: DenyConfirmOptions): string[] {
  const { accent, value, muted, error } = painters();
  const shortId = options.id.length > 8 ? options.id.slice(0, 8) : options.id;
  return [
    accent('taught the gate to refuse this.'),
    '',
    `  ${muted('pattern   ')}${value(foldLine(options.pattern))}`,
    `  ${muted('verdict   ')}${error('refuse')} ${muted('· hard refusal (fails closed)')}`,
    `  ${muted('scope     ')}${value(options.scope)}`,
    `  ${muted('antibody  ')}${muted(shortId)}`,
    '',
    `  ${muted('Next matching command is blocked before it runs. Review with')} ${value(
      'mycel-substrate list-antibodies',
    )}`,
  ];
}

export async function handleDenyCommand(host: SlashCommandHost, args: string): Promise<void> {
  const pattern = args.trim();
  if (pattern.length === 0) {
    host.showStatus('usage: /deny <command-pattern>', 'warning');
    return;
  }

  const { dbPath } = resolveSubstratePaths();
  const result = await runSubstrate('antibody-add', [
    '--db',
    dbPath,
    '--command-pattern',
    pattern,
    '--remediation',
    REMEDIATION,
    '--severity',
    'refuse',
    '--refusal-mode',
    'hard',
  ]);

  if (!result.ok) {
    host.showError(`Failed to teach the gate: ${result.failure.message}`);
    return;
  }

  // Exit 0: the write happened. Parse the id if we can; never crash on an
  // unexpected stdout shape.
  let parsed: AntibodyAddResult | null = null;
  try {
    parsed = JSON.parse(result.stdout) as AntibodyAddResult;
  } catch {
    parsed = null;
  }
  const id = parsed?.id ?? '(unknown)';
  mountPanel(host, ' Antibody ', () => buildDenyConfirmLines({ pattern, id, scope: 'project' }));
}
