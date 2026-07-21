/**
 * `/candidates` (aliases `/candidate`, `/learned`) - read-only panel of lessons
 * the learning loop captured but no human has signed yet: "learned, not yet
 * trusted". Sourced from `mycel-substrate list-candidates` (derived from the
 * stored sentinel gate events).
 */

import type { SlashCommandHost } from '../dispatch';
import {
  candidateWouldOutcome,
  foldLine,
  mountPanel,
  paintToken,
  painters,
  relativeTime,
  sentinelActionColor,
  severityColor,
} from './panel';
import { resolveSubstratePaths, runSubstrateJson } from './substrate-runner';
import type { SentinelCandidate } from './types';

const MAX_ROWS = 50;

export interface CandidatesReportOptions {
  readonly candidates: readonly SentinelCandidate[];
  readonly nowMs?: number;
}

function wouldLabel(candidate: SentinelCandidate): string {
  return candidateWouldOutcome(candidate.antibody.severity, candidate.antibody.refusal_mode);
}

export function buildCandidatesReportLines(options: CandidatesReportOptions): string[] {
  const { accent, value, muted } = painters();
  const candidates = options.candidates;

  const lines: string[] = [accent('learned, not yet trusted')];

  if (candidates.length === 0) {
    lines.push(muted('  nothing captured yet. the loop has learned nothing to sign.'));
    return lines;
  }

  const shown = candidates.slice(0, MAX_ROWS);
  const toolWidth = Math.max('Tool'.length, ...shown.map((c) => c.source.tool_name.length));
  const signalWidth = Math.max('Signal'.length, ...shown.map((c) => c.source.action.length));
  const wouldWidth = Math.max('Would'.length, ...shown.map((c) => wouldLabel(c).length));

  lines.push(
    `  ${muted('Tool'.padEnd(toolWidth))}  ${muted('Signal'.padEnd(signalWidth))}  ${muted(
      'Would'.padEnd(wouldWidth),
    )}  ${muted('Rule')}`,
  );

  let refuse = 0;
  let warn = 0;
  let allow = 0;

  for (const candidate of shown) {
    const would = wouldLabel(candidate);
    if (would === 'refuse') refuse += 1;
    else if (would === 'warn') warn += 1;
    else allow += 1;

    const signal = paintToken(
      sentinelActionColor(candidate.source.action),
      candidate.source.action.padEnd(signalWidth),
    );
    const wouldPainted = paintToken(
      severityColor(candidate.antibody.severity),
      would.padEnd(wouldWidth),
    );
    const rule = candidate.metadata.matched_rule ?? '(no rule)';
    const age = relativeTime(Date.parse(candidate.source.timestamp) / 1000, options.nowMs);
    lines.push(
      `  ${value(candidate.source.tool_name.padEnd(toolWidth))}  ${signal}  ${wouldPainted}  ${muted(
        foldLine(rule),
      )}  ${muted(age)}`,
    );

    const reason = candidate.metadata.reason;
    if (reason !== null && reason.trim().length > 0) {
      lines.push(`    ${muted('why:')} ${muted(foldLine(reason))}`);
    }
  }

  if (candidates.length > shown.length) {
    lines.push(`  ${muted(`and ${candidates.length - shown.length} more…`)}`);
  }

  lines.push('');
  lines.push(
    `  ${value(`${candidates.length} learned, not yet trusted`)}${muted(
      ` · ${refuse} would-refuse · ${warn} would-warn · ${allow} log-only`,
    )}`,
  );
  lines.push(`  ${muted('promotion is manual:')} ${value('mycel-substrate antibody-add')}`);
  return lines;
}

export async function showCandidates(host: SlashCommandHost): Promise<void> {
  const { dbPath } = resolveSubstratePaths();
  const result = await runSubstrateJson<SentinelCandidate[]>('list-candidates', ['--db', dbPath]);

  if (!result.ok) {
    host.showError(`Candidates unavailable: ${result.failure.message}`);
    return;
  }
  if (!Array.isArray(result.data)) {
    host.showError('Candidates unavailable: unexpected output from mycel-substrate.');
    return;
  }

  const candidates = result.data;
  const title = candidates.length > 0 ? ` Candidates (${candidates.length}) ` : ' Candidates ';
  mountPanel(host, title, () => buildCandidatesReportLines({ candidates }));
}
