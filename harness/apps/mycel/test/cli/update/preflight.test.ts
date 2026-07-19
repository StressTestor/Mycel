/**
 * De-moonshot guard: `runUpdatePreflight` is a network-free no-op. It never
 * fetches a latest-version manifest and always returns `continue`.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';

import { runUpdatePreflight } from '#/cli/update/preflight';

afterEach(() => {
  vi.restoreAllMocks();
});

describe('runUpdatePreflight (update checks disabled)', () => {
  it('returns "continue" and performs no network fetch', async () => {
    const fetchSpy = vi.spyOn(globalThis, 'fetch');

    const result = await runUpdatePreflight('1.2.3', { track: vi.fn() });

    expect(result).toBe('continue');
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it('returns "continue" with no arguments', async () => {
    const fetchSpy = vi.spyOn(globalThis, 'fetch');
    expect(await runUpdatePreflight()).toBe('continue');
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});
