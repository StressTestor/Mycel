/**
 * Startup header shown at the top of the TUI. An identity line (🍄 mycel,
 * version, work dir) and a status line (model, mcp, session), then Mycel's
 * voice: a red "deny by default" tagline, a rotating on-brand tip, and command
 * hints. Compact, no border, but not lifeless.
 */

import type { Component } from '@moonshot-ai/pi-tui';
import { truncateToWidth } from '@moonshot-ai/pi-tui';
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

    // voice: a red accent stripe + tagline, a rotating on-brand tip, and
    // command hints. this is what keeps the launch screen from reading dead.
    const tagline = ' ' + red('▎') + ' ' + redBold('deny by default.');
    const tip = indent + dim(tipForSession(this.state.sessionId));
    const hint =
      indent + [primary('/help'), primary('/status'), primary('/model')].join(dim('  ·  '));

    return ['', line1, line2, '', tagline, tip, '', hint, ''].map((line) =>
      truncateToWidth(line, safeWidth, '…'),
    );
  }
}
