/**
 * Startup card shown at the top of the TUI. A rounded panel with a block-art
 * mushroom logo on the left and, on the right, an identity line (mycel, version,
 * work dir), a status line (model, mcp, session), a red "deny by default"
 * tagline, and a rotating on-brand tip. Command hints sit just below the card.
 */

import type { Component } from '@moonshot-ai/pi-tui';
import { truncateToWidth, visibleWidth } from '@moonshot-ai/pi-tui';
import chalk from 'chalk';

import { effectiveModelAlias } from '@moonshot-ai/kimi-code-sdk';

import type { AppState } from '#/tui/types';
import { currentTheme } from '#/tui/theme';

/** Shorten a session id to a stable prefix, e.g. `9f08…`. Strips a leading
 * `session_` / `session-` so the prefix is the real id, not the word. */
function shortSessionId(sessionId: string): string {
  const id = sessionId.replace(/^session[_-]/, '');
  return id.length > 4 ? `${id.slice(0, 4)}…` : id;
}

/**
 * On-brand launch tips. One shows per session, chosen deterministically from
 * the session id, so it is stable within a session and rotates across launches.
 */
export const LAUNCH_TIPS = [
  'a deleted substrate blocks by default. it never resets.',
  'the brain outlives the body: swap models, keep memory.',
  'learned rules stay inert until a human promotes them.',
  'friendly to use, poisonous to anything that disarms it.',
  'the gate blocks first and asks never. fail-closed.',
  "a write over the gate's own binary is denied.",
] as const;

function tipForSession(sessionId: string): string {
  let hash = 0;
  for (const ch of sessionId) hash = (hash + ch.charCodeAt(0)) % LAUNCH_TIPS.length;
  return LAUNCH_TIPS[hash] ?? LAUNCH_TIPS[0];
}

export class WelcomeComponent implements Component {
  private state: AppState;

  constructor(state: AppState) {
    this.state = state;
  }

  invalidate(): void {}

  render(width: number): string[] {
    const safeWidth = Math.max(0, width);
    const primaryBold = chalk.bold.hex(currentTheme.palette.primary);
    const primary = chalk.hex(currentTheme.palette.primary);
    const dim = chalk.hex(currentTheme.palette.textDim);
    const warn = chalk.hex(currentTheme.palette.warning);
    // amanita red - the mascot's cap, brought in as the voice accent.
    const red = chalk.hex(currentTheme.palette.error);
    const redBold = chalk.bold.hex(currentTheme.palette.error);

    const isLoggedOut = !this.state.model;
    const activeModel = this.state.availableModels[this.state.model];
    const effectiveActiveModel = activeModel === undefined ? undefined : effectiveModelAlias(activeModel);
    const modelValue =
      effectiveActiveModel?.displayName ?? effectiveActiveModel?.model ?? this.state.model;

    // model segment: dim normally, warning text when logged out.
    const modelSegment = isLoggedOut
      ? dim('model ') + warn('not set, run /login or /provider')
      : dim(`model ${modelValue}`);

    // narrow fallback: no room for the logo card, keep it to id + status.
    if (safeWidth < 34) {
      const title = '🍄 ' + primaryBold('mycel') + ' ' + dim(this.state.version);
      return ['', title, '   ' + modelSegment, ''].map((line) =>
        truncateToWidth(line, safeWidth, '…'),
      );
    }

    const segments = [modelSegment];
    if (this.state.mcpServersSummary) {
      segments.push(dim(`mcp ${this.state.mcpServersSummary}`));
    }
    segments.push(dim(`session ${shortSessionId(this.state.sessionId)}`));

    // Mycel's logo: a block mushroom, cap in amanita red, stem in cream. Each
    // row is 8 cells wide so the text column stays aligned.
    const cream = chalk.hex('#e8d8b0');
    const logo = [
      red(' ▄████▄ '),
      red('████████'),
      red('▀██████▀'),
      cream('  ▐██▌  '),
      cream('  ▐██▌  '),
    ];
    const LOGO_W = 8;

    // text column: identity, status, a blank, the tagline, and a rotating tip.
    const textRows = [
      primaryBold('mycel') + ' ' + dim(`${this.state.version}  ${this.state.workDir}`),
      segments.join(dim(' · ')),
      '',
      redBold('deny by default.'),
      dim(tipForSession(this.state.sessionId)),
    ];

    // rounded card: logo on the left, text on the right - the way the field does
    // it (Claude, Codex, Kimi, Grok all lead with a logo). dim border so the
    // color comes from the mushroom, the blue name, and the red tagline.
    const textWidth = Math.max(1, safeWidth - (2 + 2 + LOGO_W + 2));
    const lines: string[] = ['', dim('╭' + '─'.repeat(safeWidth - 2) + '╮')];
    for (let i = 0; i < logo.length; i++) {
      const truncated = truncateToWidth(textRows[i] ?? '', textWidth, '…');
      const rightPad = Math.max(0, textWidth - visibleWidth(truncated));
      lines.push(dim('│') + '  ' + logo[i] + '  ' + truncated + ' '.repeat(rightPad) + dim('│'));
    }
    lines.push(dim('╰' + '─'.repeat(safeWidth - 2) + '╯'));

    // command hints below the card.
    const hint =
      '   ' + [primary('/help'), primary('/status'), primary('/model')].join(dim('  ·  '));
    lines.push('', hint, '');

    return lines.map((line) => truncateToWidth(line, safeWidth, '…'));
  }
}
