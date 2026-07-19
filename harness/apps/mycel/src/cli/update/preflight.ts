/**
 * De-moonshot: update checks are removed. Mycel does not phone home for a
 * latest-version manifest, does not auto-install, and does not prompt. This
 * preflight is a network-free no-op kept only so the startup path keeps a
 * stable seam. Update checks: disabled (mycel).
 */

export type UpdatePreflightResult = 'continue' | 'exit';

/**
 * Always returns `continue` without any network call. Arguments are accepted
 * so existing call sites stay unchanged; they are ignored.
 */
export async function runUpdatePreflight(
  _currentVersion?: string,
  _options?: unknown,
): Promise<UpdatePreflightResult> {
  return 'continue';
}
