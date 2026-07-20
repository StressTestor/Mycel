import { visibleWidth } from '@moonshot-ai/pi-tui';
import chalk from 'chalk';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { WelcomeComponent } from '#/tui/components/chrome/welcome';
import type { AppState } from '#/tui/types';

const TRUECOLOR_PATTERN = /\[38;2;(\d+);(\d+);(\d+)m/g;

const appState: AppState = {
  version: '1.2.3',
  workDir: '/tmp/project',
  additionalDirs: [],
  sessionId: '9f08abcd-1234',
  sessionTitle: null,
  model: 'kimi-k2',
  permissionMode: 'manual',
  thinkingEffort: 'off',
  contextUsage: 0,
  contextTokens: 0,
  maxContextTokens: 0,
  isCompacting: false,
  isReplaying: false,
  streamingPhase: 'idle',
  streamingStartTime: 0,
  planMode: false,
  inputMode: 'prompt',
  swarmMode: false,
  theme: 'dark',
  editorCommand: null,
  notifications: { enabled: true, condition: 'unfocused' },
  upgrade: { autoInstall: true },
  availableModels: {},
  availableProviders: {},
  mcpServersSummary: null,
};

function truecolorCodes(text: string): Set<string> {
  const codes = new Set<string>();
  for (const match of text.matchAll(TRUECOLOR_PATTERN)) {
    codes.add(`${match[1]},${match[2]},${match[3]}`);
  }
  return codes;
}

/** Strip ANSI truecolor codes to inspect the plain rendered text. */
function plain(text: string): string {
  return text.replace(/\[[0-9;]*m/g, '');
}

/** The two header rows (identity + status) of the rendered header. */
function headerOf(lines: string[]): string {
  return [lines[1], lines[2]].join('\n');
}

describe('WelcomeComponent', () => {
  const previousChalkLevel = chalk.level;

  beforeEach(() => {
    chalk.level = 3;
  });

  afterEach(() => {
    chalk.level = previousChalkLevel;
  });

  it('renders the compact identity and status lines', () => {
    const lines = new WelcomeComponent(appState).render(80);

    // Blank line above and below the two header lines.
    expect(lines).toHaveLength(4);
    expect(lines[0]).toBe('');
    expect(lines[3]).toBe('');
    expect(plain(lines[1]!)).toBe('╭ mycel 1.2.3  /tmp/project');
    expect(plain(lines[2]!)).toBe('╰ model kimi-k2 · session 9f08…');
  });

  it('includes the mcp segment only when a summary is present', () => {
    const withMcp = new WelcomeComponent({
      ...appState,
      mcpServersSummary: '2 servers',
    }).render(80);
    expect(plain(withMcp[2]!)).toBe('╰ model kimi-k2 · mcp 2 servers · session 9f08…');
  });

  it('shows the login warning for the model when logged out', () => {
    const loggedOut = new WelcomeComponent({ ...appState, model: '' }).render(80);
    expect(plain(loggedOut[2]!)).toBe('╰ model not set, run /login or /provider · session 9f08…');
  });

  it('renders the header in a small number of theme colors', () => {
    const codes = truecolorCodes(headerOf(new WelcomeComponent(appState).render(80)));

    // Just the brand primary and the dim shade (plus warning only when logged out).
    expect(codes.size).toBeLessThanOrEqual(2);
  });

  it('keeps every line within the requested width on narrow terminals', () => {
    for (const width of [0, 1, 2, 4, 10, 39, 80]) {
      for (const line of new WelcomeComponent(appState).render(width)) {
        expect(visibleWidth(line)).toBeLessThanOrEqual(width);
      }
    }
  });
});
