/**
 * De-moonshot: `mycel upgrade` no longer fetches a latest-version manifest.
 * It prints an actionable "disabled" message and exits 0 without any network.
 */

import { describe, expect, it, vi } from 'vitest';

import { handleUpgrade } from '#/cli/sub/upgrade';

describe('handleUpgrade (self-update disabled)', () => {
  it('prints a disabled message, makes no fetch, and returns 0', async () => {
    const fetchSpy = vi.spyOn(globalThis, 'fetch');
    const writes: string[] = [];
    const stdout = { write: (chunk: string) => (writes.push(chunk), true) };

    const code = await handleUpgrade('0.1.0', { stdout });

    expect(code).toBe(0);
    expect(fetchSpy).not.toHaveBeenCalled();
    const out = writes.join('');
    expect(out).toMatch(/does not self-update/i);
    expect(out).toMatch(/disabled/i);

    fetchSpy.mockRestore();
  });
});
