/**
 * Compact startup header shown at the top of the TUI.
 * Renders two dim lines: an identity line (mycel, version, work dir) and a
 * status line (model, mcp, session). A small hyphal branch mark (┣ / ┗) sits
 * on the left edge as Mycel's identity - a branch, not a logo block.
 */

import type { Component } from '@moonshot-ai/pi-tui';
import { truncateToWidth } from '@moonshot-ai/pi-tui';
import chalk from 'chalk';

import { effectiveModelAlias } from '@moonshot-ai/kimi-code-sdk';

import type { AppState } from '#/tui/types';
import { currentTheme } from '#/tui/theme';

/** Shorten a session id to a stable prefix, e.g. `9f08…`. */
function shortSessionId(sessionId: string): string {
  return sessionId.length > 4 ? `${sessionId.slice(0, 4)}…` : sessionId;
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
    const dim = chalk.hex(currentTheme.palette.textDim);
    const warn = chalk.hex(currentTheme.palette.warning);

    const isLoggedOut = !this.state.model;
    const activeModel = this.state.availableModels[this.state.model];
    const effectiveActiveModel = activeModel === undefined ? undefined : effectiveModelAlias(activeModel);
    const modelValue =
      effectiveActiveModel?.displayName ?? effectiveActiveModel?.model ?? this.state.model;

    // model segment: dim normally, warning text when logged out.
    const modelSegment = isLoggedOut
      ? dim('model ') + warn('not set, run /login or /provider')
      : dim(`model ${modelValue}`);

    // Hyphal branch mark on the left edge: ┣ opens the branch, ┗ closes it.
    const branchOpen = primaryBold('┣') + ' ';
    const branchClose = primaryBold('┗') + ' ';

    if (safeWidth < 24) {
      const title = branchOpen + primaryBold('mycel') + ' ' + dim(this.state.version);
      return ['', title, branchClose + modelSegment, ''].map((line) =>
        truncateToWidth(line, safeWidth, '…'),
      );
    }

    const line1 =
      branchOpen + primaryBold('mycel') + ' ' + dim(`${this.state.version}  ${this.state.workDir}`);

    const segments = [modelSegment];
    if (this.state.mcpServersSummary) {
      segments.push(dim(`mcp ${this.state.mcpServersSummary}`));
    }
    segments.push(dim(`session ${shortSessionId(this.state.sessionId)}`));
    const line2 = branchClose + segments.join(dim(' · '));

    return ['', line1, line2, ''].map((line) => truncateToWidth(line, safeWidth, '…'));
  }
}
