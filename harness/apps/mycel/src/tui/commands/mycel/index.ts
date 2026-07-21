/**
 * The Mycel command family: seven immune-system-framed slash commands that read
 * from and write to the substrate. Registry entries are aggregated here into one
 * array and spread into BUILTIN_SLASH_COMMANDS once (see registry.ts); the
 * handlers are wired in dispatch.ts.
 *
 * Read-only panels: /immunity, /gate, /substrate, /candidates.
 * Actions: /promote, /deny, /delegate.
 * All fail SOFT - a missing db/binary/empty result renders a clear message,
 * never a crash.
 */

import type { KimiSlashCommand } from '../types';
import { promoteArgumentCompletions } from './promote';

/** Shared priority so the whole family clusters together in the palette. */
const FAMILY_PRIORITY = 65;

export const MYCEL_SLASH_COMMANDS = [
  {
    name: 'immunity',
    aliases: ['antibodies'],
    description: 'Show active antibodies - what the gate will refuse',
    priority: FAMILY_PRIORITY,
    availability: 'always',
  },
  {
    name: 'gate',
    aliases: ['guard', 'doorman'],
    description: 'Show guard status - the doorman: fail-closed, deny by default',
    priority: FAMILY_PRIORITY,
    availability: 'always',
  },
  {
    name: 'substrate',
    aliases: ['marrow'],
    description: 'Show substrate health - the marrow that persists across sessions',
    priority: FAMILY_PRIORITY,
    availability: 'always',
  },
  {
    name: 'candidates',
    aliases: ['candidate', 'learned'],
    description: 'Show captured lessons not yet signed into trusted antibodies',
    priority: FAMILY_PRIORITY,
    availability: 'always',
  },
  {
    name: 'promote',
    aliases: ['sign'],
    description: 'Sign a proposed antibody into the substrate - you sign what the body learns',
    priority: FAMILY_PRIORITY,
    availability: 'always',
    argumentHint: '<id> [severity] [refusal-mode]',
    completeArgs: promoteArgumentCompletions,
  },
  {
    name: 'deny',
    aliases: ['refuse', 'block'],
    description: 'Teach the gate to refuse a command pattern (writes a hard-refuse antibody)',
    priority: FAMILY_PRIORITY,
    availability: 'always',
    argumentHint: '<command-pattern>',
  },
  {
    name: 'delegate',
    aliases: ['handoff'],
    description: 'Hand a task to a governed subagent (claude -p), gate stays closed',
    priority: FAMILY_PRIORITY,
    availability: 'always',
    argumentHint: '<task>',
  },
] as const satisfies readonly KimiSlashCommand[];

export type MycelSlashCommandName = (typeof MYCEL_SLASH_COMMANDS)[number]['name'];

export { showImmunity, buildImmunityReportLines } from './immunity';
export {
  showGateStatus,
  buildGateReportLines,
  readGateWiring,
  deriveGateStatus,
} from './gate';
export { showSubstrateStatus, buildSubstrateStatusReportLines } from './substrate';
export { showCandidates, buildCandidatesReportLines } from './candidates';
export {
  handlePromoteCommand,
  promoteArgumentCompletions,
  buildPromoteReportLines,
  buildPendingProposalsLines,
  readProposals,
} from './promote';
export { handleDenyCommand, buildDenyConfirmLines } from './deny';
export { handleDelegateCommand, buildDelegateResultLines } from './delegate';

export * from './substrate-runner';
export {
  foldLine,
  painters,
  paintToken,
  boldToken,
  severityColor,
  severityLabel,
  refusalModeColor,
  refusalModeLabel,
  sentinelActionColor,
  candidateWouldOutcome,
  homeRelative,
  humanBytes,
  relativeTime,
  mountPanel,
} from './panel';
export type * from './types';
