import { homedir } from 'node:os';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const originalEnv = { ...process.env };

async function loadPaths() {
  vi.resetModules();
  return import('#/utils/paths');
}

beforeEach(() => {
  delete process.env['MYCEL_HOME'];
  delete process.env['KIMI_CODE_HOME'];
});

afterEach(() => {
  process.env = { ...originalEnv };
  vi.restoreAllMocks();
});

describe('home directory resolution', () => {
  it('MYCEL_HOME wins for the home dir', async () => {
    process.env['MYCEL_HOME'] = '/tmp/mycel-wins';
    process.env['KIMI_CODE_HOME'] = '/tmp/legacy-should-lose';
    const { getDataDir } = await loadPaths();
    expect(getDataDir()).toBe('/tmp/mycel-wins');
  });

  it('legacy KIMI_CODE_HOME still works when MYCEL_HOME unset and warns once on stderr', async () => {
    process.env['KIMI_CODE_HOME'] = '/tmp/legacy-home';
    const spy = vi.spyOn(process.stderr, 'write').mockReturnValue(true);
    const { getDataDir } = await loadPaths();
    expect(getDataDir()).toBe('/tmp/legacy-home');
    const written = spy.mock.calls.map((c) => String(c[0])).join('');
    expect(written).toMatch(/KIMI_CODE_HOME/);
    expect(written.toLowerCase()).toMatch(/deprecat/);
  });

  it('defaults the home dir to ~/.mycel', async () => {
    const { getDataDir } = await loadPaths();
    expect(getDataDir()).toBe(`${homedir()}/.mycel`);
    expect(getDataDir().endsWith('/.mycel')).toBe(true);
  });
});
