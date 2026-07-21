import { visibleWidth } from '@moonshot-ai/pi-tui';
import chalk from 'chalk';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { LAUNCH_TIPS, WelcomeComponent } from '#/tui/components/chrome/welcome';
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

describe('WelcomeComponent', () => {
  const previousChalkLevel = chalk.level;

  beforeEach(() => {
    chalk.level = 3;
  });

  afterEach(() => {
    chalk.level = previousChalkLevel;
  });

  it('renders a bordered card with identity, status, voice, and hints below', () => {
    const body = plain(new WelcomeComponent(appState).render(80).join('\n'));

    // rounded card border + the block mushroom logo, then all the content.
    expect(body).toContain('╭');
    expect(body).toContain('╰');
    expect(body).toContain('████'); // the block-art mushroom logo
    expect(body).toContain('mycel 1.2.3  /tmp/project');
    expect(body).toContain('model kimi-k2 · session 9f08…');
    expect(body).toContain('deny by default.');
    expect(LAUNCH_TIPS.some((tip) => body.includes(tip))).toBe(true);
    // command hints, below the card.
    expect(body).toContain('/help');
    expect(body).toContain('/status');
    expect(body).toContain('/model');
  });

  it('picks a stable tip for a given session id', () => {
    const a = plain(new WelcomeComponent(appState).render(80).join('\n'));
    const b = plain(new WelcomeComponent(appState).render(80).join('\n'));
    expect(a).toBe(b);
  });

  it('includes the mcp segment only when a summary is present', () => {
    const withMcp = plain(
      new WelcomeComponent({ ...appState, mcpServersSummary: '2 servers' }).render(80).join('\n'),
    );
    expect(withMcp).toContain('mcp 2 servers');
    const without = plain(new WelcomeComponent(appState).render(80).join('\n'));
    expect(without).not.toContain('mcp ');
  });

  it('shows the login warning for the model when logged out', () => {
    const loggedOut = plain(
      new WelcomeComponent({ ...appState, model: '' }).render(80).join('\n'),
    );
    expect(loggedOut).toContain('not set, run /login or /provider');
  });

  it('uses more than one theme color (brand primary and amanita red)', () => {
    const codes = truecolorCodes(new WelcomeComponent(appState).render(80).join('\n'));
    expect(codes.size).toBeGreaterThanOrEqual(2);
  });

  it('keeps every line within the requested width on narrow terminals', () => {
    for (const width of [0, 1, 2, 4, 10, 39, 80]) {
      for (const line of new WelcomeComponent(appState).render(width)) {
        expect(visibleWidth(line)).toBeLessThanOrEqual(width);
      }
    }
  });
});
