import { createKimiDeviceId } from '@moonshot-ai/kimi-code-oauth';
import {
  resolveKimiHome,
  type KimiConfig,
  type TelemetryClient,
} from '@moonshot-ai/kimi-code-sdk';

import type { PromptHarness } from './prompt-session';
import {
  setTelemetryContext,
  track,
  withTelemetryContext,
} from '@moonshot-ai/kimi-telemetry';

export interface CliTelemetryBootstrap {
  readonly homeDir: string;
  readonly deviceId: string;
  readonly firstLaunch: boolean;
}

export interface InitializeCliTelemetryOptions {
  readonly harness: PromptHarness;
  readonly bootstrap: CliTelemetryBootstrap;
  readonly config: Pick<KimiConfig, 'defaultModel' | 'telemetry'>;
  readonly version: string;
  readonly uiMode: string;
  readonly model?: string;
  readonly sessionId?: string;
}

export function createCliTelemetryBootstrap(): CliTelemetryBootstrap {
  let firstLaunch = false;
  const homeDir = resolveKimiHome();
  const deviceId = createKimiDeviceId(homeDir, {
    onFirstLaunch: () => {
      firstLaunch = true;
    },
  });
  return { homeDir, deviceId, firstLaunch };
}

/**
 * De-moonshot: telemetry is removed. Mycel never constructs a telemetry
 * network client, so this bootstrap is a no-op. The options are accepted so
 * call sites stay unchanged; nothing is emitted.
 */
export function initializeCliTelemetry(_options: InitializeCliTelemetryOptions): void {
  // telemetry: removed — no `initializeTelemetry`, no `first_launch` event.
}

export interface InitializeServerTelemetryOptions {
  readonly version: string;
}

/**
 * De-moonshot: telemetry is removed. The `mycel web` / `mycel server run` host
 * still expects a {@link TelemetryClient} to hand to core, so we return one
 * whose methods are the (never-initialized, therefore inert) telemetry module
 * functions. Because {@link initializeCliTelemetry} and this function never call
 * `initializeTelemetry`, no sink is ever constructed and nothing leaves the
 * process.
 */
export function initializeServerTelemetry(
  _options: InitializeServerTelemetryOptions,
): TelemetryClient {
  return {
    track,
    withContext: withTelemetryContext,
    setContext: setTelemetryContext,
  };
}
