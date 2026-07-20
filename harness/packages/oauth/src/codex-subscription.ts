/**
 * Experimental adapter for reusing Codex CLI's ChatGPT subscription login.
 *
 * Authentication is delegated to `codex app-server`; Mycel never reads or
 * copies Codex's refresh token. The Responses backend itself is private and
 * undocumented, so all compatibility-specific behavior stays in this file.
 */

import { execFile, spawn } from 'node:child_process';
import { createInterface } from 'node:readline';

import { OAuthConnectionError, OAuthUnauthorizedError } from './errors';
import type { BearerTokenProvider, ProviderRequestAuth } from './toolkit';
import { isRecord } from './utils';

export const CODEX_SUBSCRIPTION_PROVIDER_NAME = 'managed:codex';
export const CODEX_SUBSCRIPTION_BASE_URL = 'https://chatgpt.com/backend-api/codex';
export const CODEX_SUBSCRIPTION_OAUTH_STORAGE = 'codex';

const REFRESH_WINDOW_MS = 5 * 60 * 1000;
const STATUS_CACHE_MS = 30_000;
const APP_SERVER_TIMEOUT_MS = 15_000;

export interface CodexSubscriptionOAuthRef {
  readonly storage: typeof CODEX_SUBSCRIPTION_OAUTH_STORAGE;
  readonly key: string;
  readonly oauthHost?: string | undefined;
}

export interface CodexAuthStatus {
  readonly authMethod?: string | undefined;
  readonly authToken?: string | undefined;
  readonly requiresOpenaiAuth?: boolean | undefined;
}

export interface CodexSubscriptionTokenProviderOptions {
  readonly readAuthStatus?: ((forceRefresh: boolean) => Promise<CodexAuthStatus>) | undefined;
  readonly resolveCodexVersion?: (() => Promise<string>) | undefined;
}

interface CachedCodexAuth {
  readonly accessToken: string;
  readonly accountId: string;
  readonly fedramp: boolean;
  readonly expiresAt: number | undefined;
  readonly fetchedAt: number;
}

let installedCodexVersion: Promise<string> | undefined;

export function isCodexSubscriptionOAuthRef(
  value: { readonly storage?: string | undefined } | undefined,
): value is CodexSubscriptionOAuthRef {
  return value?.storage === CODEX_SUBSCRIPTION_OAUTH_STORAGE;
}

export function isCodexSubscriptionBaseUrl(value: string | undefined): boolean {
  if (value === undefined) return false;
  try {
    const url = new URL(value);
    return (
      url.protocol === 'https:' &&
      url.hostname === 'chatgpt.com' &&
      url.port === '' &&
      url.username === '' &&
      url.password === '' &&
      url.pathname.replace(/\/+$/, '') === '/backend-api/codex' &&
      url.search === '' &&
      url.hash === ''
    );
  } catch {
    return false;
  }
}

export function createCodexSubscriptionTokenProvider(
  options: CodexSubscriptionTokenProviderOptions = {},
): BearerTokenProvider {
  return new CodexSubscriptionTokenProvider(options);
}

export async function getCachedCodexSubscriptionAccessToken(): Promise<string | undefined> {
  try {
    const status = await readCodexChatGPTAuth(false);
    return status.requiresOpenaiAuth !== false && status.authMethod === 'chatgpt'
      ? status.authToken
      : undefined;
  } catch (error) {
    if (error instanceof OAuthUnauthorizedError) return undefined;
    throw error;
  }
}

class CodexSubscriptionTokenProvider implements BearerTokenProvider {
  private readonly readAuthStatus: (forceRefresh: boolean) => Promise<CodexAuthStatus>;
  private readonly version: () => Promise<string>;
  private cached: CachedCodexAuth | undefined;
  private normalInFlight: Promise<CachedCodexAuth> | undefined;
  private forceInFlight: Promise<CachedCodexAuth> | undefined;

  constructor(options: CodexSubscriptionTokenProviderOptions) {
    this.readAuthStatus = options.readAuthStatus ?? readCodexChatGPTAuth;
    this.version = options.resolveCodexVersion ?? resolveInstalledCodexVersion;
  }

  async getAccessToken(options?: { readonly force?: boolean }): Promise<string> {
    return (await this.resolveAuth(options?.force === true)).accessToken;
  }

  async getRequestAuth(options?: { readonly force?: boolean }): Promise<ProviderRequestAuth> {
    const [auth, version] = await Promise.all([
      this.resolveAuth(options?.force === true),
      this.version(),
    ]);
    const headers: Record<string, string> = {
      'ChatGPT-Account-ID': auth.accountId,
      originator: 'mycel',
      version,
    };
    if (auth.fedramp) headers['X-OpenAI-Fedramp'] = 'true';
    return { apiKey: auth.accessToken, headers };
  }

  private async resolveAuth(force: boolean): Promise<CachedCodexAuth> {
    if (force) {
      this.forceInFlight ??= (this.normalInFlight ?? Promise.resolve())
        .then(() => this.fetchAuth(true))
        .finally(() => {
          this.forceInFlight = undefined;
        });
      this.cached = await this.forceInFlight;
      return this.cached;
    }

    if (this.cached !== undefined && !needsRefresh(this.cached)) return this.cached;
    if (this.forceInFlight !== undefined) return this.forceInFlight;
    this.normalInFlight ??= this.fetchAuth(false).finally(() => {
      this.normalInFlight = undefined;
    });
    this.cached = await this.normalInFlight;
    return this.cached;
  }

  private async fetchAuth(force: boolean): Promise<CachedCodexAuth> {
    const status = await this.readAuthStatus(force);
    if (
      status.requiresOpenaiAuth === false ||
      status.authMethod !== 'chatgpt' ||
      nonEmpty(status.authToken) === undefined
    ) {
      throw loginRequired('Codex is not logged in with a reusable ChatGPT subscription token.');
    }
    const accessToken = status.authToken as string;
    const claims = jwtClaims(accessToken);
    const authClaims = isRecord(claims?.['https://api.openai.com/auth'])
      ? claims['https://api.openai.com/auth']
      : undefined;
    const accountId = nonEmpty(authClaims?.['chatgpt_account_id']);
    if (accountId === undefined) {
      throw loginRequired('Codex did not return a ChatGPT workspace identifier.');
    }
    return {
      accessToken,
      accountId,
      fedramp: authClaims?.['chatgpt_account_is_fedramp'] === true,
      expiresAt: typeof claims?.['exp'] === 'number' ? claims['exp'] * 1000 : undefined,
      fetchedAt: Date.now(),
    };
  }
}

function needsRefresh(auth: CachedCodexAuth): boolean {
  return (
    auth.fetchedAt <= Date.now() - STATUS_CACHE_MS ||
    (auth.expiresAt !== undefined && auth.expiresAt <= Date.now() + REFRESH_WINDOW_MS)
  );
}

function jwtClaims(token: string): Record<string, unknown> | undefined {
  const payload = token.split('.')[1];
  if (payload === undefined || payload.length === 0) return undefined;
  try {
    const value = JSON.parse(Buffer.from(payload, 'base64url').toString('utf8')) as unknown;
    return isRecord(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

async function readCodexChatGPTAuth(forceRefresh: boolean): Promise<CodexAuthStatus> {
  const child = spawn('codex', ['app-server', '--stdio'], {
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  child.stderr.resume();

  let settled = false;
  let rejectProcessFailure: ((error: OAuthConnectionError) => void) | undefined;
  const processFailure = new Promise<never>((_resolve, reject) => {
    rejectProcessFailure = reject;
  });
  const failWhileRunning = (message: string, cause: unknown): void => {
    if (settled) return;
    rejectProcessFailure?.(new OAuthConnectionError(message, { cause }));
  };
  child.on('error', (error) => {
    failWhileRunning('Could not start Codex app-server.', error);
  });
  child.stdin.on('error', (error) => {
    failWhileRunning('Could not write to Codex app-server.', error);
  });
  const stop = (): void => {
    if (settled) return;
    settled = true;
    child.stdin.end();
    child.kill();
  };

  try {
    const lines = createInterface({ input: child.stdout, crlfDelay: Infinity });
    const messages = lines[Symbol.asyncIterator]();
    writeRpc(child.stdin, {
      method: 'initialize',
      id: 1,
      params: {
        clientInfo: { name: 'mycel', title: 'Mycel', version: '1' },
      },
    });
    await Promise.race([readRpcResponse(messages, 1), processFailure]);
    writeRpc(child.stdin, { method: 'initialized' });
    writeRpc(child.stdin, {
      method: 'getAuthStatus',
      id: 2,
      params: { includeToken: true, refreshToken: forceRefresh },
    });
    const response = await Promise.race([readRpcResponse(messages, 2), processFailure]);
    if (!isRecord(response['result'])) {
      throw new OAuthConnectionError('Codex app-server returned an invalid auth response.');
    }
    return {
      authMethod: nonEmpty(response['result']['authMethod']),
      authToken: nonEmpty(response['result']['authToken']),
      requiresOpenaiAuth:
        typeof response['result']['requiresOpenaiAuth'] === 'boolean'
          ? response['result']['requiresOpenaiAuth']
          : undefined,
    };
  } catch (error) {
    if (error instanceof OAuthConnectionError || error instanceof OAuthUnauthorizedError) {
      throw error;
    }
    throw new OAuthConnectionError('Could not read ChatGPT auth from Codex app-server.', {
      cause: error,
    });
  } finally {
    stop();
  }
}

function writeRpc(stream: NodeJS.WritableStream, message: Record<string, unknown>): void {
  try {
    stream.write(`${JSON.stringify(message)}\n`);
  } catch (error) {
    throw new OAuthConnectionError('Could not write to Codex app-server.', { cause: error });
  }
}

async function readRpcResponse(
  messages: AsyncIterator<string>,
  id: number,
): Promise<Record<string, unknown>> {
  return withTimeout(async () => {
    for (;;) {
      const next = await messages.next();
      if (next.done === true) {
        throw new OAuthConnectionError('Codex app-server exited before returning auth.');
      }
      let parsed: unknown;
      try {
        parsed = JSON.parse(next.value);
      } catch {
        continue;
      }
      if (!isRecord(parsed) || parsed['id'] !== id) continue;
      if (parsed['error'] !== undefined) {
        throw new OAuthConnectionError(
          id === 2
            ? 'Codex app-server rejected the deprecated `getAuthStatus` compatibility request; this Codex version may no longer support subscription token export.'
            : `Codex app-server rejected auth request ${id}.`,
        );
      }
      return parsed;
    }
  }, APP_SERVER_TIMEOUT_MS);
}

function withTimeout<T>(operation: () => Promise<T>, timeoutMs: number): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new OAuthConnectionError('Timed out waiting for Codex app-server auth.'));
    }, timeoutMs);
    operation().then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error: unknown) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

function nonEmpty(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim().length > 0 ? value : undefined;
}

function loginRequired(message: string): OAuthUnauthorizedError {
  return new OAuthUnauthorizedError(`${message} Run \`codex login\` and try again.`);
}

function resolveInstalledCodexVersion(): Promise<string> {
  installedCodexVersion ??= new Promise((resolve, reject) => {
    execFile('codex', ['--version'], { encoding: 'utf8' }, (error, stdout) => {
      if (error !== null) {
        reject(
          new OAuthConnectionError('Could not run `codex --version`; install or upgrade Codex.', {
            cause: error,
          }),
        );
        return;
      }
      const match = /\b(\d+\.\d+\.\d+)\b/.exec(stdout);
      if (match?.[1] === undefined) {
        reject(new OAuthConnectionError('Could not determine the installed Codex version.'));
        return;
      }
      resolve(match[1]);
    });
  });
  return installedCodexVersion;
}
