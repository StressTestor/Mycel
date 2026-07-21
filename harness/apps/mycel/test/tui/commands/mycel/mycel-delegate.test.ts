import { EventEmitter } from 'node:events';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { SlashCommandHost } from '#/tui/commands/dispatch';
import type { SubstratePaths } from '#/tui/commands/mycel/substrate-runner';

const spawnMock = vi.hoisted(() => ({ spawn: vi.fn() }));
vi.mock('node:child_process', () => ({ spawn: spawnMock.spawn }));

const runner = vi.hoisted(() => ({ resolveSubstratePaths: vi.fn() }));
vi.mock('#/tui/commands/mycel/substrate-runner', () => ({
  resolveSubstratePaths: runner.resolveSubstratePaths,
}));

import { handleDelegateCommand } from '#/tui/commands/mycel/delegate';

class FakeStream extends EventEmitter {
  setEncoding(): this {
    return this;
  }
}
class FakeChild extends EventEmitter {
  stdout = new FakeStream();
  stderr = new FakeStream();
}

interface PanelLike {
  render: (width: number) => string[];
}

function makeHost() {
  const panels: PanelLike[] = [];
  const spinner = { stop: vi.fn(), setLabel: vi.fn() };
  const host = {
    state: {
      transcriptContainer: { addChild: vi.fn((panel: PanelLike) => panels.push(panel)) },
      ui: { requestRender: vi.fn() },
    },
    showError: vi.fn(),
    showProgressSpinner: vi.fn(() => spinner),
  } as unknown as SlashCommandHost & {
    showError: ReturnType<typeof vi.fn>;
    showProgressSpinner: ReturnType<typeof vi.fn>;
  };
  return { host, panels, spinner };
}

let existingBin: string;
let paths: SubstratePaths;

beforeEach(() => {
  vi.clearAllMocks();
  const dir = mkdtempSync(join(tmpdir(), 'mycel-delegate-'));
  existingBin = join(dir, 'mycel-delegate');
  writeFileSync(existingBin, '#!/bin/sh\n');
  paths = {
    dataDir: dir,
    substrateDir: join(dir, 'substrate'),
    binPath: join(dir, 'mycel-substrate'),
    gateBinPath: join(dir, 'mycel-gate'),
    delegateBinPath: existingBin,
    dbPath: join(dir, 'substrate', 'mycel.db'),
    auditPath: join(dir, 'substrate', 'audit.jsonl'),
    proposalsPath: join(dir, 'substrate', 'proposals.jsonl'),
    configPath: join(dir, 'config.toml'),
  };
  runner.resolveSubstratePaths.mockReturnValue(paths);
});

describe('handleDelegateCommand', () => {
  it('rejects an empty task without spawning', async () => {
    const { host } = makeHost();
    await handleDelegateCommand(host, '   ');
    expect(spawnMock.spawn).not.toHaveBeenCalled();
    expect(host.showError).toHaveBeenCalledWith('Usage: /delegate <task>');
  });

  it('soft-errors when the binary is missing', async () => {
    runner.resolveSubstratePaths.mockReturnValue({
      ...paths,
      delegateBinPath: join(tmpdir(), 'definitely-absent-mycel-delegate'),
    });
    const { host } = makeHost();
    await handleDelegateCommand(host, 'do a thing');
    expect(spawnMock.spawn).not.toHaveBeenCalled();
    expect(host.showError).toHaveBeenCalledWith(expect.stringContaining('mycel-delegate not found'));
  });

  it('passes the task as a single argv element (no shell) and renders output', async () => {
    const child = new FakeChild();
    spawnMock.spawn.mockReturnValue(child);
    const { host, panels, spinner } = makeHost();

    const task = '$(rm -rf /) ; echo pwned';
    const pending = handleDelegateCommand(host, task);

    // Injection safety: one argv element, no shell:true.
    expect(spawnMock.spawn).toHaveBeenCalledTimes(1);
    const [bin, argv, options] = spawnMock.spawn.mock.calls[0]!;
    expect(bin).toBe(existingBin);
    expect(argv).toEqual([task]);
    expect((options as { shell?: boolean }).shell).toBeUndefined();

    child.stdout.emit('data', 'hello\nworld\n');
    child.emit('close', 0);
    await pending;

    expect(spinner.stop).toHaveBeenCalledWith({ ok: true, label: 'handed off · gate held.' });
    expect(panels).toHaveLength(1);
    const rendered = panels[0]!.render(80).join('\n');
    expect(rendered).toContain('hello');
    expect(rendered).toContain('world');
  });

  it('decodes a non-zero exit into a failure panel', async () => {
    const child = new FakeChild();
    spawnMock.spawn.mockReturnValue(child);
    const { host, panels } = makeHost();

    const pending = handleDelegateCommand(host, 'do a thing');
    child.emit('close', 2);
    await pending;

    expect(panels).toHaveLength(1);
    expect(panels[0]!.render(80).join('\n')).toContain('claude not on PATH');
  });
});
