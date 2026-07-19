/**
 * De-moonshot guard: telemetry is removed. These tests prove the CLI/server
 * telemetry bootstraps never construct a network client (never call
 * `initializeTelemetry`) and that no other call site in `src/` does either.
 */

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  initializeTelemetry: vi.fn(),
  createKimiDeviceId: vi.fn(() => 'device-123'),
  resolveKimiHome: vi.fn(() => '/home/.mycel'),
}));

vi.mock('@moonshot-ai/kimi-telemetry', () => ({
  initializeTelemetry: mocks.initializeTelemetry,
  setTelemetryContext: vi.fn(),
  track: vi.fn(),
  withTelemetryContext: vi.fn(),
}));

vi.mock('@moonshot-ai/kimi-code-oauth', () => ({
  createKimiDeviceId: mocks.createKimiDeviceId,
  KIMI_CODE_PROVIDER_NAME: 'managed:kimi-code',
}));

vi.mock('@moonshot-ai/kimi-code-sdk', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@moonshot-ai/kimi-code-sdk')>();
  return {
    ...actual,
    resolveKimiHome: mocks.resolveKimiHome,
  };
});

describe('telemetry is removed (no network client is ever constructed)', () => {
  it('initializeServerTelemetry returns an inert client without initializing telemetry', async () => {
    const { initializeServerTelemetry } = await import('#/cli/telemetry');
    const client = initializeServerTelemetry({ version: '1.2.3' });

    expect(mocks.initializeTelemetry).not.toHaveBeenCalled();
    expect(client).toEqual(
      expect.objectContaining({
        track: expect.any(Function),
        withContext: expect.any(Function),
        setContext: expect.any(Function),
      }),
    );
    // Calling the client must not construct a sink.
    client.track('anything', {});
    expect(mocks.initializeTelemetry).not.toHaveBeenCalled();
  });

  it('initializeCliTelemetry is a no-op and initializes no telemetry', async () => {
    const { initializeCliTelemetry } = await import('#/cli/telemetry');
    expect(() =>
      initializeCliTelemetry({
        harness: { track: vi.fn(), homeDir: '/home/.mycel' } as never,
        bootstrap: { homeDir: '/home/.mycel', deviceId: 'd', firstLaunch: true },
        config: { telemetry: true },
        version: '1.2.3',
        uiMode: 'shell',
      }),
    ).not.toThrow();
    expect(mocks.initializeTelemetry).not.toHaveBeenCalled();
  });

  it('no source file constructs a telemetry sink via initializeTelemetry(', () => {
    const srcDir = join(dirname(fileURLToPath(import.meta.url)), '../../src');
    const hits: string[] = [];
    const walk = (dir: string): void => {
      for (const name of readdirSync(dir)) {
        const p = join(dir, name);
        if (statSync(p).isDirectory()) {
          walk(p);
          continue;
        }
        if (!p.endsWith('.ts')) continue;
        const text = readFileSync(p, 'utf8');
        if (/\binitializeTelemetry\s*\(/.test(text)) hits.push(p);
      }
    };
    walk(srcDir);
    expect(hits).toEqual([]);
  });
});
