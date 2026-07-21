/**
 * `/promote <id>` (alias `/sign`) - the human-in-the-loop moment: sign a
 * proposed antibody from `proposals.jsonl` into the live substrate. Reads the
 * inert proposal by id (full uuid or unique prefix), then shells the real
 * `mycel-substrate antibody-add` and echoes exactly what got signed.
 *
 * Defaults are the LEAST-DESTRUCTIVE pair (severity=warn, refusal-mode=soft) so
 * signing can never silently hard-block the operator's own tools; a trailing
 * `[severity] [refusal-mode]` escalates to `refuse hard`.
 */

import { readFileSync } from 'node:fs';

import type { AutocompleteItem } from '@moonshot-ai/pi-tui';

import type { SlashCommandHost } from '../dispatch';
import { completeLeadingArg, type ArgCompletionSpec } from '../complete-args';
import { foldLine, mountPanel, paintToken, painters, severityColor } from './panel';
import { resolveSubstratePaths, runSubstrate } from './substrate-runner';
import type { AntibodyAddResult, Proposal, SubstrateSeverity } from './types';

const SEVERITY_COMPLETIONS: readonly ArgCompletionSpec[] = [
  { value: 'refuse', description: 'Hard-severity - refuses when paired with hard' },
  { value: 'warn', description: 'Warn only (default)' },
  { value: 'info', description: 'Log only' },
];

const REFUSAL_MODE_COMPLETIONS: readonly ArgCompletionSpec[] = [
  { value: 'hard', description: 'Hard block (refuses)' },
  { value: 'soft', description: 'Soft warn (default)' },
  { value: 'log-only', description: 'Log only' },
];

const VALID_SEVERITIES = new Set(['refuse', 'warn', 'info']);
const VALID_MODES = new Set(['hard', 'soft', 'log-only']);

interface ProposalSignatureField {
  readonly flag: string;
  readonly label: string;
  readonly value: string;
}

function shortId(id: string): string {
  return id.length > 8 ? id.slice(0, 8) : id;
}

/** Read proposals.jsonl, skipping blank and malformed lines. */
export function readProposals(proposalsPath: string): Proposal[] {
  let raw: string;
  try {
    raw = readFileSync(proposalsPath, 'utf8');
  } catch {
    return [];
  }
  const proposals: Proposal[] = [];
  for (const line of raw.split('\n')) {
    const trimmed = line.trim();
    if (trimmed.length === 0) continue;
    try {
      const parsed = JSON.parse(trimmed) as Proposal;
      if (parsed !== null && typeof parsed.id === 'string') proposals.push(parsed);
    } catch {
      // One corrupt line never aborts the whole read.
    }
  }
  return proposals;
}

type ResolveResult =
  | { readonly kind: 'found'; readonly proposal: Proposal }
  | { readonly kind: 'not-found' }
  | { readonly kind: 'ambiguous'; readonly ids: string[] };

function resolveProposal(proposals: readonly Proposal[], idArg: string): ResolveResult {
  const exact = proposals.find((proposal) => proposal.id === idArg);
  if (exact !== undefined) return { kind: 'found', proposal: exact };
  const matches = proposals.filter((proposal) => proposal.id.startsWith(idArg));
  if (matches.length === 1) return { kind: 'found', proposal: matches[0]! };
  if (matches.length > 1) return { kind: 'ambiguous', ids: matches.map((p) => shortId(p.id)) };
  return { kind: 'not-found' };
}

function proposalSignatureFields(proposal: Proposal): ProposalSignatureField[] {
  const signature = proposal.signature ?? {};
  const fields: ProposalSignatureField[] = [];
  if (signature.command_pattern != null) {
    fields.push({ flag: '--command-pattern', label: 'command_pattern', value: signature.command_pattern });
  }
  if (signature.tool_name != null) {
    fields.push({ flag: '--tool-name', label: 'tool_name', value: signature.tool_name });
  }
  if (signature.error_class != null) {
    fields.push({ flag: '--error-class', label: 'error_class', value: signature.error_class });
  }
  if (signature.file_pattern != null) {
    fields.push({ flag: '--file-pattern', label: 'file_pattern', value: signature.file_pattern });
  }
  return fields;
}

function signatureSummary(proposal: Proposal): string {
  const [first] = proposalSignatureFields(proposal);
  if (first === undefined) return '(no signature)';
  return foldLine(`${first.label}: ${first.value}`);
}

// ── Panels ──────────────────────────────────────────────────────────────────

export interface PromoteResultOptions {
  readonly newId: string;
  readonly proposalId: string;
  readonly signatureLabel: string;
  readonly outcome: 'refuse' | 'warn' | 'allow';
  readonly remediation: string;
  readonly scope: string;
  readonly source: string;
  readonly downgradeWarning?: string;
}

export function buildPromoteReportLines(options: PromoteResultOptions): string[] {
  const { accent, value, muted, warning } = painters();
  const severityToken = severityColor(
    options.outcome === 'allow' ? 'info' : (options.outcome as SubstrateSeverity),
  );
  const lines = [
    accent('signed into the substrate'),
    `  ${muted('antibody     ')}${value(shortId(options.newId))} ${muted('(new live id)')}`,
    `  ${muted('from         ')}${muted(`proposal ${shortId(options.proposalId)}`)}`,
    `  ${muted('signature    ')}${value(foldLine(options.signatureLabel))}`,
    `  ${muted('reflex       ')}${paintToken(severityToken, options.outcome)} ${muted('(outcome_preview)')}`,
    `  ${muted('remediation  ')}${value(foldLine(options.remediation))}`,
    `  ${muted('scope        ')}${value(options.scope)} ${muted(`· source ${options.source}`)}`,
  ];
  if (options.downgradeWarning !== undefined) {
    lines.push(`  ${warning(foldLine(options.downgradeWarning))}`);
  }
  lines.push('');
  lines.push(`  ${muted('This reflex is live now - the gate will act on the next matching call.')}`);
  return lines;
}

export function buildPendingProposalsLines(proposals: readonly Proposal[]): string[] {
  const { accent, value, muted } = painters();
  if (proposals.length === 0) {
    return [
      accent('learned proposals'),
      muted("  no candidates yet - the substrate hasn't proposed anything (propose_antibody writes here)"),
    ];
  }
  const idWidth = Math.max(8, ...proposals.map((p) => shortId(p.id).length));
  const lines = [accent('learned proposals - sign one with  /promote <id>')];
  for (const proposal of proposals) {
    const remediation = proposal.remediation ?? proposal.rationale ?? '(no remediation)';
    lines.push(
      `  ${value(shortId(proposal.id).padEnd(idWidth))}  ${muted(signatureSummary(proposal))}  ${muted(
        `→ ${foldLine(remediation)}`,
      )}`,
    );
  }
  return lines;
}

// ── Argument completion ───────────────────────────────────────────────────────

export function promoteArgumentCompletions(prefix: string): AutocompleteItem[] | null {
  const spaceIdx = prefix.indexOf(' ');
  if (spaceIdx === -1) {
    return completeProposalIds(prefix);
  }
  const rest = prefix.slice(spaceIdx + 1);
  const restSpace = rest.indexOf(' ');
  if (restSpace === -1) {
    return completeLeadingArg(SEVERITY_COMPLETIONS, rest);
  }
  const modePrefix = rest.slice(restSpace + 1);
  if (modePrefix.includes(' ')) return null;
  return completeLeadingArg(REFUSAL_MODE_COMPLETIONS, modePrefix);
}

function completeProposalIds(prefix: string): AutocompleteItem[] | null {
  const { proposalsPath } = resolveSubstratePaths();
  const proposals = readProposals(proposalsPath);
  const items: AutocompleteItem[] = [];
  for (const proposal of proposals) {
    const id = shortId(proposal.id);
    if (prefix.length > 0 && !id.startsWith(prefix)) continue;
    const remediation = proposal.remediation ?? proposal.rationale ?? '(no remediation)';
    items.push({
      value: id,
      label: id,
      description: foldLine(`${signatureSummary(proposal)} -> ${remediation}`),
    });
  }
  return items.length > 0 ? items : null;
}

// ── Handler ───────────────────────────────────────────────────────────────────

export async function handlePromoteCommand(host: SlashCommandHost, args: string): Promise<void> {
  const { dbPath, proposalsPath } = resolveSubstratePaths();
  const tokens = args.trim().length === 0 ? [] : args.trim().split(/\s+/);
  const proposals = readProposals(proposalsPath);

  // No id: show the pending-proposals list instead of promoting.
  if (tokens.length === 0) {
    mountPanel(host, ' Candidates ', () => buildPendingProposalsLines(proposals));
    return;
  }

  const [idArg, severityArg, modeArg] = tokens;

  if (severityArg !== undefined && !VALID_SEVERITIES.has(severityArg)) {
    host.showError(`Invalid severity "${severityArg}" (expected refuse|warn|info).`);
    return;
  }
  if (modeArg !== undefined && !VALID_MODES.has(modeArg)) {
    host.showError(`Invalid refusal-mode "${modeArg}" (expected hard|soft|log-only).`);
    return;
  }
  const severity: SubstrateSeverity = (severityArg as SubstrateSeverity | undefined) ?? 'warn';
  const refusalMode = severityArg !== undefined && modeArg === undefined
    ? severity === 'refuse'
      ? 'hard'
      : 'soft'
    : (modeArg ?? 'soft');

  const resolved = resolveProposal(proposals, idArg!);
  if (resolved.kind === 'not-found') {
    host.showError(`No candidate matches ${idArg}. Run /promote with no id to list pending ones.`);
    return;
  }
  if (resolved.kind === 'ambiguous') {
    host.showError(`Ambiguous id ${idArg} - matches ${resolved.ids.join(', ')}.`);
    return;
  }

  const proposal = resolved.proposal;
  const signatureFields = proposalSignatureFields(proposal);
  if (signatureFields.length === 0) {
    host.showError(`Candidate ${shortId(proposal.id)} has an empty signature; cannot promote.`);
    return;
  }

  const remediation =
    proposal.remediation ?? proposal.rationale ?? `Promoted from proposal ${shortId(proposal.id)}`;

  const addArgs = ['--db', dbPath, '--remediation', remediation];
  for (const field of signatureFields) {
    addArgs.push(field.flag, field.value);
  }
  addArgs.push('--severity', severity, '--refusal-mode', refusalMode, '--source', 'curated');

  const result = await runSubstrate('antibody-add', addArgs, { timeoutMs: 10_000 });
  if (!result.ok) {
    host.showError(`Failed to sign candidate: ${result.failure.message}`);
    return;
  }

  let parsed: AntibodyAddResult | null = null;
  try {
    parsed = JSON.parse(result.stdout) as AntibodyAddResult;
  } catch {
    parsed = null;
  }
  const newId = parsed?.id ?? '(unknown)';
  const outcome = parsed?.outcome_preview ?? 'warn';

  // Surface the substrate's own "will NOT hard-block" warning so a toothless
  // sign is never silent.
  const stderrLine = foldLine(result.stderr);
  const downgradeWarning =
    severity === 'refuse' && outcome !== 'refuse' && stderrLine.length > 0
      ? stderrLine
      : undefined;

  mountPanel(host, ' Signed ', () =>
    buildPromoteReportLines({
      newId,
      proposalId: proposal.id,
      signatureLabel: `${signatureFields[0]!.label} = ${signatureFields[0]!.value}`,
      outcome,
      remediation,
      scope: 'project',
      source: 'curated',
      downgradeWarning,
    }),
  );
}
