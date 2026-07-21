/**
 * Shared launcher for the managed Mycel binaries.
 *
 * Resolves the `mycel-substrate` / `mycel-gate` / `mycel-delegate` binaries and
 * the substrate db/audit/proposals paths from MYCEL_HOME (via the app's
 * getDataDir/getBinDir), then runs a substrate subcommand through `execFile`
 * with an ARGV ARRAY — never a shell string. Agent- and user-authored text
 * (patterns, remediation, tasks) rides as a single argv element, so shell
 * metacharacters are inert: no injection is possible.
 *
 * Every call returns a typed result-or-soft-failure. Nothing throws out of here;
 * callers render the failure as a panel/error and the TUI keeps running.
 */

import { execFile } from 'node:child_process';
import { existsSync } from 'node:fs';

import { join } from 'pathe';

import { getBinDir, getDataDir } from '#/utils/paths';

const SUBSTRATE_BIN_NAME = 'mycel-substrate';
const GATE_BIN_NAME = 'mycel-gate';
const DELEGATE_BIN_NAME = 'mycel-delegate';
const SUBSTRATE_DIR_NAME = 'substrate';
const SUBSTRATE_DB_FILE_NAME = 'mycel.db';
const SUBSTRATE_AUDIT_FILE_NAME = 'audit.jsonl';
const SUBSTRATE_PROPOSALS_FILE_NAME = 'proposals.jsonl';
const CONFIG_FILE_NAME = 'config.toml';

/** A tiny sqlite read should never take long; a timeout signals a real problem. */
const DEFAULT_TIMEOUT_MS = 5_000;
const DEFAULT_MAX_BUFFER = 8 * 1024 * 1024;

export interface SubstratePaths {
  readonly dataDir: string;
  readonly substrateDir: string;
  readonly binPath: string;
  readonly gateBinPath: string;
  readonly delegateBinPath: string;
  readonly dbPath: string;
  readonly auditPath: string;
  readonly proposalsPath: string;
  readonly configPath: string;
}

/** Resolve every managed path the family needs from the current MYCEL_HOME. */
export function resolveSubstratePaths(): SubstratePaths {
  const dataDir = getDataDir();
  const binDir = getBinDir();
  const substrateDir = join(dataDir, SUBSTRATE_DIR_NAME);
  return {
    dataDir,
    substrateDir,
    binPath: join(binDir, SUBSTRATE_BIN_NAME),
    gateBinPath: join(binDir, GATE_BIN_NAME),
    delegateBinPath: join(binDir, DELEGATE_BIN_NAME),
    dbPath: join(substrateDir, SUBSTRATE_DB_FILE_NAME),
    auditPath: join(substrateDir, SUBSTRATE_AUDIT_FILE_NAME),
    proposalsPath: join(substrateDir, SUBSTRATE_PROPOSALS_FILE_NAME),
    configPath: join(dataDir, CONFIG_FILE_NAME),
  };
}

export type SubstrateFailureKind =
  | 'missing-binary'
  | 'timeout'
  | 'nonzero-exit'
  | 'spawn-error'
  | 'malformed-output';

export interface SubstrateFailure {
  readonly kind: SubstrateFailureKind;
  /** One-line, user-facing summary (already folded). */
  readonly message: string;
  /** Optional extra context: trimmed stderr or a stdout preview. */
  readonly detail?: string;
}

export type SubstrateRunResult =
  | { readonly ok: true; readonly stdout: string; readonly stderr: string }
  | { readonly ok: false; readonly failure: SubstrateFailure };

export interface RunSubstrateOptions {
  /** Override the resolved binary path (tests / non-default installs). */
  readonly binPath?: string;
  readonly timeoutMs?: number;
  readonly maxBuffer?: number;
}

function fold(text: string): string {
  return text.trim().replaceAll(/\s+/g, ' ');
}

/**
 * Run `mycel-substrate <subcommand> <...args>` with an argv array. Resolves to a
 * typed result; never rejects.
 */
export function runSubstrate(
  subcommand: string,
  args: readonly string[] = [],
  options: RunSubstrateOptions = {},
): Promise<SubstrateRunResult> {
  const binPath = options.binPath ?? resolveSubstratePaths().binPath;
  const timeout = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const maxBuffer = options.maxBuffer ?? DEFAULT_MAX_BUFFER;

  if (!existsSync(binPath)) {
    return Promise.resolve({
      ok: false,
      failure: {
        kind: 'missing-binary',
        message: `mycel-substrate not found at ${binPath} — run install.sh (drive unmounted?).`,
      },
    });
  }

  return new Promise((resolve) => {
    execFile(
      binPath,
      [subcommand, ...args],
      { encoding: 'utf8', timeout, maxBuffer },
      (error, stdout, stderr) => {
        if (error === null) {
          resolve({ ok: true, stdout, stderr });
          return;
        }
        const err = error as NodeJS.ErrnoException & {
          killed?: boolean;
          signal?: NodeJS.Signals | null;
        };
        // ENOENT/EACCES: the binary vanished between existsSync and spawn.
        if (err.code === 'ENOENT' || err.code === 'EACCES') {
          resolve({
            ok: false,
            failure: {
              kind: 'missing-binary',
              message: `mycel-substrate could not be launched at ${binPath} — run install.sh.`,
            },
          });
          return;
        }
        // A timeout (or maxBuffer overflow) kills the child; either way, surface it.
        if (err.killed === true || err.signal === 'SIGTERM') {
          resolve({
            ok: false,
            failure: { kind: 'timeout', message: 'mycel-substrate timed out.' },
          });
          return;
        }
        const stderrLine = fold(stderr ?? '');
        resolve({
          ok: false,
          failure: {
            kind: 'nonzero-exit',
            message:
              stderrLine.length > 0 ? stderrLine : 'mycel-substrate exited with an error.',
            detail: stderrLine,
          },
        });
      },
    );
  });
}

export type SubstrateJsonResult<T> =
  | { readonly ok: true; readonly data: T }
  | { readonly ok: false; readonly failure: SubstrateFailure };

/** Run a substrate subcommand and JSON.parse its stdout into `T`. */
export async function runSubstrateJson<T>(
  subcommand: string,
  args: readonly string[] = [],
  options: RunSubstrateOptions = {},
): Promise<SubstrateJsonResult<T>> {
  const result = await runSubstrate(subcommand, args, options);
  if (!result.ok) return result;
  try {
    return { ok: true, data: JSON.parse(result.stdout) as T };
  } catch {
    return {
      ok: false,
      failure: {
        kind: 'malformed-output',
        message: 'Could not read output from mycel-substrate (unexpected format).',
        detail: fold(result.stdout).slice(0, 80),
      },
    };
  }
}
