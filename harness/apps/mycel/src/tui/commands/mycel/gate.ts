/**
 * `/gate` (aliases `/guard`, `/doorman`) — read-only status panel: is the
 * mycel-gate PreToolUse guard armed? Derives an honest arming state from the
 * config.toml hook wiring, the substrate-db presence, and the active antibody
 * count. The protected-path floor is a compiled constant, stated as a fact.
 */

import { existsSync, readFileSync } from 'node:fs';

import { parse as parseToml } from 'smol-toml';

import type { SlashCommandHost } from '../dispatch';
import { boldToken, homeRelative, mountPanel, painters } from './panel';
import { resolveSubstratePaths, runSubstrateJson } from './substrate-runner';
import type { Antibody } from './types';

/** Compiled floor from crates/mycel-core/src/pathguard.rs `floor_roots()`. */
const FLOOR_ROOTS = ['bin/', 'config.toml', 'substrate/'] as const;

export type GateStatus = 'armed' | 'tripwire' | 'fail-open' | 'disarmed' | 'unknown';
export type GateFailMode = 'closed' | 'open' | 'unknown';
export type GateConfigProblem = 'missing' | 'parse';

export interface GateWiring {
  readonly configReadable: boolean;
  readonly configProblem?: GateConfigProblem;
  readonly hookWired: boolean;
  /** Raw matcher: '' or null => catch-all. null when not wired. */
  readonly matcher: string | null;
  readonly failMode: GateFailMode;
}

export interface GateAntibodyInfo {
  readonly count: number;
  readonly refuse: number;
  readonly warn: number;
  readonly info: number;
}

export interface GateReportOptions {
  readonly wiring: GateWiring;
  readonly dbPresent: boolean;
  readonly dbPath: string;
  readonly antibodies: GateAntibodyInfo | { readonly error: string };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** Parse config.toml and extract the mycel-gate PreToolUse wiring. */
export function readGateWiring(configPath: string): GateWiring {
  let raw: string;
  try {
    raw = readFileSync(configPath, 'utf8');
  } catch {
    return { configReadable: false, configProblem: 'missing', hookWired: false, matcher: null, failMode: 'unknown' };
  }
  let parsed: unknown;
  try {
    parsed = parseToml(raw);
  } catch {
    return { configReadable: false, configProblem: 'parse', hookWired: false, matcher: null, failMode: 'unknown' };
  }

  const hooks = isRecord(parsed) ? parsed['hooks'] : undefined;
  if (!Array.isArray(hooks)) {
    return { configReadable: true, hookWired: false, matcher: null, failMode: 'unknown' };
  }
  const hook = hooks.find(
    (entry) =>
      isRecord(entry) &&
      entry['event'] === 'PreToolUse' &&
      typeof entry['command'] === 'string' &&
      entry['command'].includes('mycel-gate'),
  );
  if (!isRecord(hook)) {
    return { configReadable: true, hookWired: false, matcher: null, failMode: 'unknown' };
  }
  const matcher = typeof hook['matcher'] === 'string' ? hook['matcher'] : '';
  const failModeRaw = hook['fail_mode'];
  const failMode: GateFailMode =
    failModeRaw === 'closed' ? 'closed' : failModeRaw === 'open' ? 'open' : 'unknown';
  return { configReadable: true, hookWired: true, matcher, failMode };
}

/** Derive the arming state. Honest and composite — not a single boolean. */
export function deriveGateStatus(wiring: GateWiring, dbPresent: boolean): GateStatus {
  if (!wiring.configReadable) return 'unknown';
  if (!wiring.hookWired) return 'disarmed';
  if (wiring.failMode === 'open') return 'fail-open';
  if (wiring.failMode === 'closed') return dbPresent ? 'armed' : 'tripwire';
  return 'unknown';
}

const STATUS_META: Record<GateStatus, { label: string; token: 'success' | 'error' | 'warning' }> = {
  armed: { label: 'ARMED', token: 'success' },
  tripwire: { label: 'ARMED — TRIPWIRE', token: 'warning' },
  'fail-open': { label: 'ARMED — FAIL-OPEN', token: 'warning' },
  disarmed: { label: 'DISARMED', token: 'error' },
  unknown: { label: 'STATUS UNKNOWN', token: 'warning' },
};

export function buildGateReportLines(options: GateReportOptions): string[] {
  const { accent, value, muted, error, success } = painters();
  const { wiring, dbPresent, dbPath, antibodies } = options;
  const status = deriveGateStatus(wiring, dbPresent);

  const rows: string[] = [];
  const label = (text: string): string => muted(text.padEnd(13));

  const configHint =
    wiring.configProblem === 'parse' ? 'unknown (config unparseable)' : 'unknown (config unreadable)';

  // Guard hook row.
  if (!wiring.configReadable) {
    rows.push(`${label('Guard hook')}${muted(configHint)}`);
    rows.push(`${label('Matcher')}${muted('unknown')}`);
    rows.push(`${label('Fail mode')}${muted('unknown')}`);
  } else if (!wiring.hookWired) {
    rows.push(`${label('Guard hook')}${error('not wired')}`);
    rows.push(`${label('Matcher')}${muted('n/a')}`);
    rows.push(`${label('Fail mode')}${muted('n/a')}`);
  } else {
    rows.push(`${label('Guard hook')}${value('mycel-gate')}  ${muted('PreToolUse')}`);
    const matcherEmpty = wiring.matcher === null || wiring.matcher === '';
    rows.push(
      matcherEmpty
        ? `${label('Matcher')}${value('catch-all ("")')}  ${muted('governs every tool call')}`
        : `${label('Matcher')}${value(wiring.matcher ?? '')}`,
    );
    const failText =
      wiring.failMode === 'closed'
        ? `${value('closed')}  ${muted('(nonzero exit blocks the operation)')}`
        : wiring.failMode === 'open'
          ? `${value('open')}  ${muted('(degraded — tool proceeds on gate error)')}`
          : muted('unknown');
    rows.push(`${label('Fail mode')}${failText}`);
  }

  // Substrate db row.
  const dbShort = homeRelative(dbPath);
  rows.push(
    dbPresent
      ? `${label('Substrate db')}${success('present')}  ${muted(dbShort)}`
      : `${label('Substrate db')}${error('MISSING')}  ${muted(dbShort)}`,
  );

  // Antibodies row.
  if ('error' in antibodies) {
    rows.push(`${label('Antibodies')}${muted(`unavailable (${antibodies.error})`)}`);
  } else {
    rows.push(
      `${label('Antibodies')}${value(`${antibodies.count} active`)}  ${muted(
        `(${antibodies.refuse} refuse, ${antibodies.warn} warn)`,
      )}`,
    );
  }

  // Protected floor (compiled, static).
  rows.push(`${label('Protected')}${value(FLOOR_ROOTS.join('  '))}`);
  rows.push(`${label('')}${muted('compiled floor, cannot be disabled by config')}`);

  const meta = STATUS_META[status];
  const statusLine = `${muted('Status'.padEnd(13))}${boldToken(meta.token, meta.label)}`;

  return [
    accent('The doorman — fail-closed, deny by default'),
    statusLine,
    '',
    ...rows,
    '',
    `  ${muted('from config.toml — on-disk config, not live-session state')}`,
  ];
}

export async function showGateStatus(host: SlashCommandHost): Promise<void> {
  const paths = resolveSubstratePaths();
  const wiring = readGateWiring(paths.configPath);
  const dbPresent = existsSync(paths.dbPath);

  // Read-only: do NOT run list-antibodies when the db is absent — that would
  // auto-create the db as a side effect of a status panel.
  let antibodies: GateAntibodyInfo | { error: string };
  if (!dbPresent) {
    antibodies = { error: 'db missing' };
  } else {
    const result = await runSubstrateJson<Antibody[]>('list-antibodies', ['--db', paths.dbPath], {
      binPath: paths.binPath,
    });
    if (!result.ok) {
      antibodies = { error: describeAntibodyFailure(result.failure.kind, result.failure.message) };
    } else if (!Array.isArray(result.data)) {
      antibodies = { error: 'malformed output' };
    } else {
      antibodies = summarize(result.data);
    }
  }

  mountPanel(host, ' Gate ', () => buildGateReportLines({ wiring, dbPresent, dbPath: paths.dbPath, antibodies }));
}

function summarize(antibodies: readonly Antibody[]): GateAntibodyInfo {
  const info = { count: antibodies.length, refuse: 0, warn: 0, info: 0 };
  for (const antibody of antibodies) info[antibody.severity] += 1;
  return info;
}

function describeAntibodyFailure(kind: string, message: string): string {
  switch (kind) {
    case 'missing-binary':
      return 'mycel-substrate not found';
    case 'timeout':
      return 'timed out';
    case 'malformed-output':
      return 'malformed output';
    default:
      return message;
  }
}
