/**
 * CLI-owned data path helpers.
 *
 * These paths are for local app data such as logs and input history. Config
 * files are owned by Core/SDK and intentionally do not live behind this module.
 */

import { createHash } from 'node:crypto';
import { homedir } from 'node:os';
import { join } from 'node:path';

import {
  KIMI_CODE_BANNER_DIR_NAME,
  KIMI_CODE_BANNER_STATE_FILE_NAME,
  KIMI_CODE_BIN_DIR_NAME,
  KIMI_CODE_CACHE_DIR_NAME,
  KIMI_CODE_DATA_DIR_NAME,
  KIMI_CODE_INPUT_HISTORY_DIR_NAME,
  KIMI_CODE_LOG_DIR_NAME,
  KIMI_CODE_UPDATE_INSTALL_LOCK_FILE_NAME,
  KIMI_CODE_UPDATE_INSTALL_STATE_FILE_NAME,
  KIMI_CODE_UPDATE_DIR_NAME,
  KIMI_CODE_UPDATE_ROLLOUT_LOG_FILE_NAME,
  KIMI_CODE_UPDATE_STATE_FILE_NAME,
  LEGACY_HOME_ENV,
  MYCEL_HOME_ENV,
} from '#/constant/app';

// Warn at most once per process when the legacy home env is used, so a run that
// resolves the data dir many times (logs, cache, bin, ...) doesn't spam stderr.
let warnedLegacyHome = false;

/**
 * Return the root data directory for Mycel.
 *
 * Priority: `MYCEL_HOME` env var > legacy `KIMI_CODE_HOME` (deprecated, warns
 * once) > `~/.mycel`.
 */
export function getDataDir(): string {
  const mycelHome = process.env[MYCEL_HOME_ENV];
  if (mycelHome) {
    return mycelHome;
  }
  const legacyHome = process.env[LEGACY_HOME_ENV];
  if (legacyHome) {
    if (!warnedLegacyHome) {
      warnedLegacyHome = true;
      process.stderr.write(
        `${LEGACY_HOME_ENV} is deprecated; set ${MYCEL_HOME_ENV} instead. ` +
          `Honoring ${LEGACY_HOME_ENV} for now.\n`,
      );
    }
    return legacyHome;
  }
  return join(homedir(), KIMI_CODE_DATA_DIR_NAME);
}

/**
 * Return the diagnostic log directory: `<dataDir>/logs/`.
 */
export function getLogDir(): string {
  return join(getDataDir(), KIMI_CODE_LOG_DIR_NAME);
}

/**
 * Return the CLI cache directory: `<dataDir>/cache/`.
 */
export function getCacheDir(): string {
  return join(getDataDir(), KIMI_CODE_CACHE_DIR_NAME);
}

/**
 * Return the managed tools directory: `<dataDir>/bin/`.
 */
export function getBinDir(): string {
  return join(getDataDir(), KIMI_CODE_BIN_DIR_NAME);
}

/**
 * Return the update cache file: `<dataDir>/updates/latest.json`.
 */
export function getUpdateStateFile(): string {
  return join(getDataDir(), KIMI_CODE_UPDATE_DIR_NAME, KIMI_CODE_UPDATE_STATE_FILE_NAME);
}

/**
 * Return the update install state file: `<dataDir>/updates/install.json`.
 */
export function getUpdateInstallStateFile(): string {
  return join(getDataDir(), KIMI_CODE_UPDATE_DIR_NAME, KIMI_CODE_UPDATE_INSTALL_STATE_FILE_NAME);
}

/**
 * Return the update install lock file: `<dataDir>/updates/install.lock`.
 */
export function getUpdateInstallLockFile(): string {
  return join(getDataDir(), KIMI_CODE_UPDATE_DIR_NAME, KIMI_CODE_UPDATE_INSTALL_LOCK_FILE_NAME);
}

/**
 * Return the rollout decision log: `<dataDir>/updates/rollout.log`.
 */
export function getUpdateRolloutLogFile(): string {
  return join(getDataDir(), KIMI_CODE_UPDATE_DIR_NAME, KIMI_CODE_UPDATE_ROLLOUT_LOG_FILE_NAME);
}

/**
 * Return the banner display state file: `<dataDir>/cache/banner/state.json`.
 */
export function getBannerStateFile(): string {
  return join(getCacheDir(), KIMI_CODE_BANNER_DIR_NAME, KIMI_CODE_BANNER_STATE_FILE_NAME);
}

/**
 * Return the user input history file for a given working directory.
 * Layout: `<share_dir>/user-history/<md5(cwd)>.jsonl`.
 */
export function getInputHistoryFile(workDir: string): string {
  const hash = createHash('md5').update(workDir, 'utf-8').digest('hex');
  return join(getDataDir(), KIMI_CODE_INPUT_HISTORY_DIR_NAME, `${hash}.jsonl`);
}
