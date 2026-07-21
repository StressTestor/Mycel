/**
 * `/substrate` (alias `/marrow`) - read-only health panel for the Mycel
 * substrate: "the marrow - what persists across sessions". Shows active
 * antibodies, pending candidates (un-distilled sentinel events), audit-trail
 * size, and the last maintenance pass. Sourced from `mycel-substrate status`.
 */

import type { SlashCommandHost } from '../dispatch';
import { homeRelative, humanBytes, mountPanel, painters, relativeTime } from './panel';
import { resolveSubstratePaths, runSubstrateJson } from './substrate-runner';
import type { MaintenanceSummary, SubstrateStatus } from './types';

export interface SubstrateStatusReportOptions {
  readonly antibodyCount: number;
  readonly candidateCount: number;
  readonly auditBytes: number;
  readonly auditLines: number;
  readonly lastMaintenance: MaintenanceSummary | null;
  readonly dbPath: string;
  readonly error?: string;
  readonly nowMs?: number;
}

const LABEL_WIDTH = 16;

export function buildSubstrateStatusReportLines(options: SubstrateStatusReportOptions): string[] {
  const { accent, value, muted, warning, error } = painters();
  const label = (text: string): string => muted(text.padEnd(LABEL_WIDTH));
  const dbShort = homeRelative(options.dbPath);

  const lines: string[] = [accent('>_ Substrate  (the marrow - what persists across sessions)'), ''];

  if (options.error !== undefined) {
    lines.push(`  ${error(options.error)}`);
    lines.push('');
    lines.push(`  ${muted('db')}  ${muted(dbShort)}`);
    return lines;
  }

  // Antibodies.
  const antibodyValue =
    options.antibodyCount > 0
      ? `${value(`${options.antibodyCount} active`)}  ${muted('learned refusals, enforced')}`
      : muted('none yet');
  lines.push(`  ${label('Antibodies')}${antibodyValue}`);

  // Candidates (un-distilled sentinel gate events).
  const candidateValue =
    options.candidateCount > 0
      ? `${value(`${options.candidateCount} pending`)}  ${muted('gate events awaiting distillation')}`
      : muted('none pending');
  lines.push(`  ${label('Candidates')}${candidateValue}`);

  // Audit log (raw gate trail).
  const auditValue =
    options.auditBytes > 0 || options.auditLines > 0
      ? `${value(`${humanBytes(options.auditBytes)} · ${options.auditLines} lines`)}  ${muted(
          'raw gate trail (audit.jsonl)',
        )}`
      : muted('empty');
  lines.push(`  ${label('Audit log')}${auditValue}`);

  // Last maintenance.
  if (options.lastMaintenance === null) {
    lines.push(`  ${label('Last maintenance')}${warning('never run')}`);
  } else {
    const m = options.lastMaintenance;
    const when = relativeTime(m.ts, options.nowMs);
    const detail = `decayed ${m.decayed} · distilled ${m.distilled} · retained ${m.retained} · preserved ${m.preserved}`;
    lines.push(`  ${label('Last maintenance')}${value(when)}  ${muted(detail)}`);
  }

  lines.push('');
  lines.push(`  ${muted('db')}  ${muted(dbShort)}`);
  return lines;
}

function failureCopy(kind: string, message: string): string {
  switch (kind) {
    case 'missing-binary':
      return 'substrate binary not found - run install.sh';
    case 'timeout':
      return 'substrate status timed out';
    case 'malformed-output':
      return 'could not read substrate status (unexpected output)';
    case 'nonzero-exit':
      // The CLI prints a clear "substrate db missing … run install.sh" here.
      return message.includes('db missing')
        ? 'substrate not initialized - run install.sh'
        : `could not read substrate status: ${message}`;
    default:
      return `could not read substrate status: ${message}`;
  }
}

export async function showSubstrateStatus(host: SlashCommandHost): Promise<void> {
  const { dbPath } = resolveSubstratePaths();
  const result = await runSubstrateJson<SubstrateStatus>('status', ['--db', dbPath]);

  if (!result.ok) {
    const copy = failureCopy(result.failure.kind, result.failure.message);
    mountPanel(host, ' Substrate ', () =>
      buildSubstrateStatusReportLines({
        antibodyCount: 0,
        candidateCount: 0,
        auditBytes: 0,
        auditLines: 0,
        lastMaintenance: null,
        dbPath,
        error: copy,
      }),
    );
    return;
  }

  const status = result.data;
  mountPanel(host, ' Substrate ', () =>
    buildSubstrateStatusReportLines({
      antibodyCount: status.antibody_count,
      candidateCount: status.sentinel_event_count,
      auditBytes: status.audit_bytes,
      auditLines: status.audit_lines,
      lastMaintenance: status.last_maintenance,
      dbPath,
    }),
  );
}
