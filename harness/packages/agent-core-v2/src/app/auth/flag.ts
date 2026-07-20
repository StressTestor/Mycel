/**
 * `auth` domain (cross-cutting) — experimental flag for Codex subscription authentication.
 */

import { parseBooleanEnv } from '#/_base/utils/env';

export const CODEX_SUBSCRIPTION_AUTH_FLAG_ID = 'codex_subscription_auth';
export const CODEX_SUBSCRIPTION_AUTH_FLAG_ENV =
  'KIMI_CODE_EXPERIMENTAL_CODEX_SUBSCRIPTION_AUTH';
export const EXPERIMENTAL_MASTER_FLAG_ENV = 'KIMI_CODE_EXPERIMENTAL_FLAG';

export function isCodexSubscriptionAuthEnabled(input: {
  readonly getEnv: (name: string) => string | undefined;
  readonly experimental: Readonly<Record<string, boolean>> | undefined;
}): boolean {
  if (parseBooleanEnv(input.getEnv(EXPERIMENTAL_MASTER_FLAG_ENV)) === true) {
    return true;
  }
  const envOverride = parseBooleanEnv(input.getEnv(CODEX_SUBSCRIPTION_AUTH_FLAG_ENV));
  if (envOverride !== undefined) {
    return envOverride;
  }
  return input.experimental?.[CODEX_SUBSCRIPTION_AUTH_FLAG_ID] ?? false;
}
