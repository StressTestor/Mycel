import { mkdirSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'pathe';

// Home-dir precedence, kept in lockstep with the app layer (apps/mycel
// getDataDir) and agent-core-v2 bootstrap: explicit arg > MYCEL_HOME > legacy
// KIMI_CODE_HOME > ~/.mycel. The app prints the KIMI_CODE_HOME deprecation
// warning once; core honors the legacy var silently to avoid double-warning.
export function resolveKimiHome(homeDir?: string | undefined): string {
  return (
    homeDir ??
    process.env['MYCEL_HOME'] ??
    process.env['KIMI_CODE_HOME'] ??
    join(homedir(), '.mycel')
  );
}

export function resolveConfigPath(input: {
  readonly homeDir?: string | undefined;
  readonly configPath?: string | undefined;
}): string {
  return input.configPath ?? join(resolveKimiHome(input.homeDir), 'config.toml');
}

export function ensureKimiHome(homeDir: string): void {
  mkdirSync(homeDir, { recursive: true, mode: 0o700 });
}
