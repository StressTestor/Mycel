// Use U+25CF instead of U+23FA to avoid emoji/fallback rendering in terminals.
export const STATUS_BULLET = '● ';

// Shared transcript markers. Keep widths stable because message wrapping
// assumes the marker occupies the leading cells.
export const USER_MESSAGE_BULLET = '✨ ';
export const SUCCESS_MARK = '✓ ';
export const FAILURE_MARK = '✗ ';

// Conversational turn labels shown in a fixed-width gutter so user and
// assistant message bodies align in a single column. These replace the
// per-role glyph markers on conversational turns only; the shared glyph
// constants above (STATUS_BULLET, USER_MESSAGE_BULLET) are still used by
// other components (tool calls, status rows, panels) and must stay intact.
export const USER_ROLE_LABEL = 'you';
// The mushroom marks mycel's turns in the transcript (the mascot is the voice).
export const ASSISTANT_ROLE_LABEL = '🍄';
export const ROLE_GUTTER = 8;

// Left-align a role label in the fixed gutter so every message body starts
// at the same column. Continuation and image lines reuse ROLE_GUTTER for
// their indent to stay aligned under the body.
export function padRoleLabel(label: string): string {
  return label.padEnd(ROLE_GUTTER);
}

// Shared selector markers — keep every list picker visually consistent.
// SELECT_POINTER marks the highlighted row; CURRENT_MARK is appended to the
// row that is the currently-active value. See .agents/skills/write-tui/DESIGN.md.
export const SELECT_POINTER = '❯';
export const CURRENT_MARK = '← current';
