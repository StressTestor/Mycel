/* eslint-disable import/first -- vi.mock setup must run before the imports it stubs out. */
import { EventEmitter } from 'node:events';
import { PassThrough, Writable } from 'node:stream';
import type { ChildProcessWithoutNullStreams } from 'node:child_process';

import { afterEach, describe, expect, it, vi } from 'vitest';

const childProcessMocks = vi.hoisted(() => ({
  spawn: vi.fn(),
}));

vi.mock('node:child_process', async (importOriginal) => {
  const original = await importOriginal<typeof import('node:child_process')>();
  return { ...original, spawn: childProcessMocks.spawn };
});

import {
  createCodexSubscriptionTokenProvider,
  getCachedCodexSubscriptionAccessToken,
  isCodexSubscriptionBaseUrl,
  isCodexSubscriptionOAuthRef,
  OAuthConnectionError,
  OAuthUnauthorizedError,
} from '../src';

afterEach(() => {
  childProcessMocks.spawn.mockReset();
});

function accessToken(options: {
  readonly accountId?: string | undefined;
  readonly expiresAt?: number | undefined;
  readonly fedramp?: boolean | undefined;
} = {}): string {
  const payload = {
    exp: options.expiresAt ?? Math.floor(Date.now() / 1000) + 3600,
    'https://api.openai.com/auth': {
      chatgpt_account_id: options.accountId ?? 'workspace-123',
      chatgpt_account_is_fedramp: options.fedramp ?? false,
    },
  };
  return [
    Buffer.from('{}').toString('base64url'),
    Buffer.from(JSON.stringify(payload)).toString('base64url'),
    'signature',
  ].join('.');
}

describe('Codex subscription auth', () => {
  it('turns Codex app-server auth into Responses request auth', async () => {
    const token = accessToken();
    const readAuthStatus = vi.fn(async () => ({
      authMethod: 'chatgpt',
      authToken: token,
    }));
    const provider = createCodexSubscriptionTokenProvider({
      readAuthStatus,
      resolveCodexVersion: async () => '0.144.6',
    });

    const auth = await provider.getRequestAuth?.();

    expect(auth?.apiKey).toBe(token);
    expect(auth?.headers).toEqual({
      'ChatGPT-Account-ID': 'workspace-123',
      originator: 'mycel',
      version: '0.144.6',
    });
    expect(readAuthStatus).toHaveBeenCalledWith(false);
  });

  it('caches a fresh token and asks Codex to force refresh after a 401', async () => {
    const readAuthStatus = vi.fn(async () => ({
      authMethod: 'chatgpt',
      authToken: accessToken(),
    }));
    const provider = createCodexSubscriptionTokenProvider({
      readAuthStatus,
      resolveCodexVersion: async () => '0.144.6',
    });

    await provider.getAccessToken();
    await provider.getAccessToken();
    await provider.getAccessToken({ force: true });

    expect(readAuthStatus.mock.calls).toEqual([[false], [true]]);
  });

  it('includes the FedRAMP routing header when the token requires it', async () => {
    const provider = createCodexSubscriptionTokenProvider({
      readAuthStatus: async () => ({
        authMethod: 'chatgpt',
        authToken: accessToken({ fedramp: true }),
      }),
      resolveCodexVersion: async () => '0.144.6',
    });

    const auth = await provider.getRequestAuth?.();

    expect(auth?.headers?.['X-OpenAI-Fedramp']).toBe('true');
  });

  it('fails closed for API-key login or a token without a workspace', async () => {
    const apiKeyProvider = createCodexSubscriptionTokenProvider({
      readAuthStatus: async () => ({ authMethod: 'apikey', authToken: 'sk-test' }),
    });
    const missingWorkspaceProvider = createCodexSubscriptionTokenProvider({
      readAuthStatus: async () => ({
        authMethod: 'chatgpt',
        authToken: accessToken({ accountId: '' }),
      }),
    });

    await expect(apiKeyProvider.getAccessToken()).rejects.toBeInstanceOf(OAuthUnauthorizedError);
    await expect(missingWorkspaceProvider.getAccessToken()).rejects.toBeInstanceOf(
      OAuthUnauthorizedError,
    );
  });

  it('recognizes only the explicit codex credential source', () => {
    expect(isCodexSubscriptionOAuthRef({ storage: 'codex' })).toBe(true);
    expect(isCodexSubscriptionOAuthRef({ storage: 'file' })).toBe(false);
    expect(isCodexSubscriptionOAuthRef(undefined)).toBe(false);
  });

  it('accepts only the normalized Codex subscription backend', () => {
    expect(isCodexSubscriptionBaseUrl('https://chatgpt.com/backend-api/codex')).toBe(true);
    expect(isCodexSubscriptionBaseUrl('https://chatgpt.com/backend-api/codex/')).toBe(true);
    expect(isCodexSubscriptionBaseUrl('https://example.test/backend-api/codex')).toBe(false);
    expect(isCodexSubscriptionBaseUrl('https://chatgpt.com/backend-api/codex?next=1')).toBe(false);
    expect(isCodexSubscriptionBaseUrl('https://chatgpt.com@evil.example/codex')).toBe(false);
  });

  it('contains a broken app-server stdin pipe as an OAuth connection error', async () => {
    const stdout = new PassThrough();
    const stderr = new PassThrough();
    let writes = 0;
    const stdin = new Writable({
      write(_chunk, _encoding, callback) {
        writes += 1;
        if (writes === 1) {
          callback();
          queueMicrotask(() => {
            stdout.write(`${JSON.stringify({ id: 1, result: {} })}\n`);
          });
          return;
        }
        callback(Object.assign(new Error('write EPIPE'), { code: 'EPIPE' }));
      },
    });
    const child = Object.assign(new EventEmitter(), {
      stdin,
      stdout,
      stderr,
      kill: vi.fn(() => true),
    }) as unknown as ChildProcessWithoutNullStreams;
    childProcessMocks.spawn.mockReturnValue(child);

    await expect(getCachedCodexSubscriptionAccessToken()).rejects.toMatchObject({
      name: OAuthConnectionError.name,
      message: 'Could not write to Codex app-server.',
    });
    expect(child.kill).toHaveBeenCalledOnce();
  });
});
