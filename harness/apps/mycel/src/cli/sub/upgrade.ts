interface WritableLike {
  write(chunk: string): boolean;
}

export interface UpgradeDeps {
  readonly stdout: WritableLike;
}

/**
 * De-moonshot: Mycel does not self-update. Update checks and auto-install were
 * removed (local-first, no phone-home). `mycel upgrade` explains how to update
 * manually instead of fetching a latest-version manifest.
 */
export async function handleUpgrade(
  _currentVersion: string,
  overrides: Partial<UpgradeDeps> = {},
): Promise<number> {
  const stdout = overrides.stdout ?? process.stdout;
  stdout.write(
    [
      'Mycel does not self-update. Update checks are disabled (local-first, no phone-home).',
      'To update, re-run the installer or pull the repository and rebuild.',
      '',
    ].join('\n'),
  );
  return 0;
}
