/**
 * Shared, immune-flavored panel helpers for the Mycel command family.
 *
 * Every builder returns a `string[]` where each element is exactly ONE boxed row
 * (see UsagePanelComponent.render). Dynamic values must be folded to a single
 * line first — an embedded newline drops trailing text to column 0 and punches
 * through the rounded border. Colors follow the app theme: `error` (reaper red)
 * for refuse/severity, `success` (mint) for healthy/allow, `warning` (amber) for
 * degraded, `textDim`/`textMuted` for meta.
 */

import { UsagePanelComponent } from '#/tui/components/messages/usage-panel';
import { currentTheme } from '#/tui/theme';
import type { ColorToken } from '#/tui/theme';

import type { SlashCommandHost } from '../dispatch';
import type { SentinelActionValue, SubstrateRefusalMode, SubstrateSeverity } from './types';

/**
 * Collapse any whitespace run (including embedded newlines) to a single space
 * and trim. Mirrors mcp-status-panel's formatErrorLine so agent-authored
 * remediation / signature text can never break the box.
 */
export function foldLine(text: string): string {
  return text.trim().replaceAll(/\s+/g, ' ');
}

export interface Painters {
  readonly accent: (text: string) => string;
  readonly value: (text: string) => string;
  readonly muted: (text: string) => string;
  readonly faint: (text: string) => string;
  readonly error: (text: string) => string;
  readonly warning: (text: string) => string;
  readonly success: (text: string) => string;
}

/**
 * Build the theme painters at call time. Panels invoke this inside their
 * `buildLines` closure so a mid-session theme switch repaints correctly (the
 * UsagePanelComponent re-runs the builder on invalidate).
 */
export function painters(): Painters {
  return {
    accent: (text) => currentTheme.boldFg('primary', text),
    value: (text) => currentTheme.fg('text', text),
    muted: (text) => currentTheme.fg('textDim', text),
    faint: (text) => currentTheme.fg('textMuted', text),
    error: (text) => currentTheme.fg('error', text),
    warning: (text) => currentTheme.fg('warning', text),
    success: (text) => currentTheme.fg('success', text),
  };
}

/** Paint text with an arbitrary theme token (used when the token is computed). */
export function paintToken(token: ColorToken, text: string): string {
  return currentTheme.fg(token, text);
}

/** Bold-paint text with an arbitrary theme token. */
export function boldToken(token: ColorToken, text: string): string {
  return currentTheme.boldFg(token, text);
}

/** Reaper red for refuse, amber for warn, dim for info. */
export function severityColor(severity: SubstrateSeverity): ColorToken {
  switch (severity) {
    case 'refuse':
      return 'error';
    case 'warn':
      return 'warning';
    case 'info':
      return 'textDim';
  }
}

export function severityLabel(severity: SubstrateSeverity): string {
  return severity.toUpperCase();
}

/** hard -> red, soft -> amber, log-only -> dim. */
export function refusalModeColor(mode: SubstrateRefusalMode): ColorToken {
  switch (mode) {
    case 'hard':
      return 'error';
    case 'soft':
      return 'warning';
    case 'log_only':
      return 'textDim';
  }
}

export function refusalModeLabel(mode: SubstrateRefusalMode): string {
  switch (mode) {
    case 'hard':
      return 'hard block';
    case 'soft':
      return 'soft warn';
    case 'log_only':
      return 'log only';
  }
}

/** A sentinel gate signal painted by its action. */
export function sentinelActionColor(action: SentinelActionValue): ColorToken {
  switch (action) {
    case 'block':
      return 'error';
    case 'warn':
      return 'warning';
    case 'allow':
      return 'textDim';
  }
}

/**
 * The verdict a sentinel event's derived antibody WOULD carry if signed.
 * Mirrors Rust's `EvaluationOutcome::from_policy`.
 */
export function candidateWouldOutcome(
  severity: SubstrateSeverity,
  mode: SubstrateRefusalMode,
): 'refuse' | 'warn' | 'allow' {
  if (severity === 'refuse' && mode === 'hard') return 'refuse';
  if (mode === 'log_only' || severity === 'info') return 'allow';
  return 'warn';
}

/** Home-relativize an absolute path for compact display (`~/…`). */
export function homeRelative(path: string): string {
  const home = process.env['HOME'];
  if (home !== undefined && home.length > 0 && path.startsWith(home)) {
    return `~${path.slice(home.length)}`;
  }
  return path;
}

/** Humanize a byte count (e.g. 1.2 KB). */
export function humanBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rendered = unit === 0 ? String(Math.round(value)) : value.toFixed(1);
  return `${rendered} ${units[unit]}`;
}

/** Coarse relative-time label for a unix-seconds timestamp. */
export function relativeTime(unixSeconds: number, nowMs: number = Date.now()): string {
  const deltaSec = Math.max(0, Math.floor(nowMs / 1000 - unixSeconds));
  if (deltaSec < 60) return 'just now';
  const minutes = Math.floor(deltaSec / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

/**
 * Mount a bordered panel into the transcript and request a render. Central
 * helper so every command in the family mounts panels identically.
 */
export function mountPanel(
  host: SlashCommandHost,
  title: string,
  buildLines: () => string[],
): void {
  const panel = new UsagePanelComponent(buildLines, 'primary', title);
  host.state.transcriptContainer.addChild(panel);
  host.state.ui.requestRender();
}
