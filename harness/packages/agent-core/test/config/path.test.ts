import { homedir } from 'node:os';
import { join } from 'pathe';

import { afterEach, describe, expect, it } from 'vitest';

import { resolveConfigPath, resolveKimiHome } from '../../src/config/path';

describe('resolveKimiHome precedence', () => {
  const prevMycel = process.env['MYCEL_HOME'];
  const prevLegacy = process.env['KIMI_CODE_HOME'];

  afterEach(() => {
    for (const [key, prev] of [
      ['MYCEL_HOME', prevMycel],
      ['KIMI_CODE_HOME', prevLegacy],
    ] as const) {
      if (prev === undefined) delete process.env[key];
      else process.env[key] = prev;
    }
  });

  it('explicit homeDir wins over both env vars', () => {
    process.env['MYCEL_HOME'] = '/m';
    process.env['KIMI_CODE_HOME'] = '/k';
    expect(resolveKimiHome('/explicit')).toBe('/explicit');
  });

  it('prefers MYCEL_HOME over legacy KIMI_CODE_HOME', () => {
    process.env['MYCEL_HOME'] = '/m';
    process.env['KIMI_CODE_HOME'] = '/k';
    expect(resolveKimiHome()).toBe('/m');
  });

  it('honors legacy KIMI_CODE_HOME when MYCEL_HOME is unset', () => {
    delete process.env['MYCEL_HOME'];
    process.env['KIMI_CODE_HOME'] = '/k';
    expect(resolveKimiHome()).toBe('/k');
  });

  it('defaults to ~/.mycel when neither env is set', () => {
    delete process.env['MYCEL_HOME'];
    delete process.env['KIMI_CODE_HOME'];
    expect(resolveKimiHome()).toBe(join(homedir(), '.mycel'));
  });

  it('resolveConfigPath places config.toml under the resolved home', () => {
    delete process.env['MYCEL_HOME'];
    delete process.env['KIMI_CODE_HOME'];
    process.env['MYCEL_HOME'] = '/m';
    expect(resolveConfigPath({})).toBe('/m/config.toml');
  });
});
