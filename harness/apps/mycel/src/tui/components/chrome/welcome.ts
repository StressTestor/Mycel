/**
 * Startup card shown at the top of the TUI. A rounded panel holding an identity
 * line (🍄 mycel, version, work dir), a status line (model, mcp, session), and
 * Mycel's voice: a red "deny by default" tagline and a rotating on-brand tip.
 * Command hints sit just below the card.
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
  'a deleted substrate reads as a disarmed guard. it blocks, never resets.',
  'the brain outlives the body. swap the model, keep the memory.',
  'learned rules stay inert until you promote them. no autoimmune surprises.',
  'friendly to use, poisonous to anything trying to disarm the gate.',
  'the gate blocks first and asks never. fail-closed, always.',
  "a write over the gate's own binary is denied. it won't disarm itself.",
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

    // Mycel's mark: the mushroom sits on the identity line; the status line
    // indents to align under the name. deny by default.
    const mark = '🍄 ';
    const indent = '   ';

    if (safeWidth < 24) {
      const title = mark + primaryBold('mycel') + ' ' + dim(this.state.version);
      return ['', title, indent + modelSegment, ''].map((line) =>
        truncateToWidth(line, safeWidth, '…'),
      );
    }

    const line1 =
      mark + primaryBold('mycel') + ' ' + dim(`${this.state.version}  ${this.state.workDir}`);

    const segments = [modelSegment];
    if (this.state.mcpServersSummary) {
      segments.push(dim(`mcp ${this.state.mcpServersSummary}`));
    }
    segments.push(dim(`session ${shortSessionId(this.state.sessionId)}`));
    const line2 = indent + segments.join(dim(' · '));

    // voice inside the card: a red accent stripe + tagline and a rotating tip.
    const tagline = ' ' + red('▎') + ' ' + redBold('deny by default.');
    const tip = indent + dim(tipForSession(this.state.sessionId));
    const content = [line1, line2, '', tagline, tip];

    // rounded card. dim border so the colorful content (mushroom, blue name,
    // red tagline) carries the screen; the frame just holds it together.
    const innerWidth = Math.max(1, safeWidth - 4);
    const pad = '  ';
    const lines: string[] = ['', dim('╭' + '─'.repeat(safeWidth - 2) + '╮')];
    for (const c of content) {
      const truncated = truncateToWidth(c, innerWidth, '…');
      const rightPad = Math.max(0, innerWidth - visibleWidth(truncated));
      lines.push(dim('│') + pad + truncated + ' '.repeat(rightPad) + dim('│'));
    }
    lines.push(dim('╰' + '─'.repeat(safeWidth - 2) + '╯'));

    // command hints below the card.
    const hint =
      indent + [primary('/help'), primary('/status'), primary('/model')].join(dim('  ·  '));
    lines.push('', hint, '');

    return lines.map((line) => truncateToWidth(line, safeWidth, '…'));
  }
}
