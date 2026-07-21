/**
 * `/immunity` (alias `/antibodies`) — read-only panel of the ACTIVE antibodies
 * in the substrate: what the gate will refuse, grouped by severity.
 */

import type { SlashCommandHost } from '../dispatch';
import {
  boldToken,
  foldLine,
  mountPanel,
  paintToken,
  painters,
  refusalModeColor,
  refusalModeLabel,
  severityColor,
  severityLabel,
} from './panel';
import { resolveSubstratePaths, runSubstrateJson } from './substrate-runner';
import type { Antibody, SubstrateSeverity, SubstrateSignature } from './types';

const SEVERITY_ORDER: readonly SubstrateSeverity[] = ['refuse', 'warn', 'info'];

interface SignatureField {
  readonly label: string;
  readonly value: string;
}

/** Ordered, populated signature fields with their display prefix. */
function signatureFields(signature: SubstrateSignature): SignatureField[] {
  const fields: SignatureField[] = [];
  if (signature.command_pattern != null) {
    fields.push({ label: 'cmd', value: signature.command_pattern });
  }
  if (signature.tool_pattern != null) fields.push({ label: 'tool', value: signature.tool_pattern });
  if (signature.file_pattern != null) fields.push({ label: 'file', value: signature.file_pattern });
  if (signature.error_class != null) fields.push({ label: 'err', value: signature.error_class });
  if (signature.agent_role != null) fields.push({ label: 'role', value: signature.agent_role });
  return fields;
}

function primarySignatureLabel(signature: SubstrateSignature): string {
  const [first] = signatureFields(signature);
  if (first === undefined) return '(no signature)';
  return foldLine(`${first.label}: ${first.value}`);
}

function isExpired(antibody: Antibody, nowMs: number): boolean {
  if (antibody.expires_at === null) return false;
  const expires = Date.parse(antibody.expires_at);
  return Number.isFinite(expires) && expires <= nowMs;
}

export interface ImmunityReportOptions {
  readonly antibodies: readonly Antibody[];
  readonly nowMs?: number;
}

export function buildImmunityReportLines(options: ImmunityReportOptions): string[] {
  const { accent, value, muted } = painters();
  const nowMs = options.nowMs ?? Date.now();
  const antibodies = options.antibodies;

  const lines: string[] = [accent('Your immune system — what the body will refuse')];

  if (antibodies.length === 0) {
    lines.push(
      muted('  No antibodies yet. The body has learned nothing to refuse — every command passes.'),
    );
    lines.push('');
    lines.push(`  ${muted('Curate with')} ${value('mycel-substrate antibody-add')}`);
    return lines;
  }

  // Column width: align the primary signature across every severity group.
  const sigWidth = Math.max(
    ...antibodies.map((antibody) => primarySignatureLabel(antibody.signature).length),
  );

  const counts: Record<SubstrateSeverity, number> = { refuse: 0, warn: 0, info: 0 };
  for (const antibody of antibodies) counts[antibody.severity] += 1;

  for (const severity of SEVERITY_ORDER) {
    const group = antibodies.filter((antibody) => antibody.severity === severity);
    if (group.length === 0) continue;

    lines.push(
      `${boldToken(severityColor(severity), severityLabel(severity))}${muted(` (${group.length})`)}`,
    );

    for (const antibody of group) {
      const fields = signatureFields(antibody.signature);
      const primary = primarySignatureLabel(antibody.signature).padEnd(sigWidth);
      const mode = paintToken(
        refusalModeColor(antibody.refusal_mode),
        refusalModeLabel(antibody.refusal_mode),
      );
      const hits = antibody.hit_count > 0 ? muted(` ·${antibody.hit_count}× fired`) : '';
      const expired = isExpired(antibody, nowMs) ? muted(' (expired)') : '';
      lines.push(`  ${value(primary)}  ${mode}  ${muted(antibody.signature.scope)}${hits}${expired}`);

      if (fields.length > 1) {
        const extra = fields
          .slice(1)
          .map((field) => `${field.label}: ${field.value}`)
          .join(', ');
        lines.push(`    ${muted('matches also:')} ${muted(foldLine(extra))}`);
      }

      lines.push(`    ${muted('→ ')}${value(foldLine(antibody.remediation))}`);
    }
  }

  lines.push('');
  lines.push(
    `  ${value(`${counts.refuse} refuse · ${counts.warn} warn · ${counts.info} info`)}${muted(
      ` — ${antibodies.length} antibodies active`,
    )}`,
  );
  lines.push(`  ${muted('Curate with')} ${value('mycel-substrate antibody-add')}`);
  return lines;
}

export async function showImmunity(host: SlashCommandHost): Promise<void> {
  const { dbPath } = resolveSubstratePaths();
  const result = await runSubstrateJson<Antibody[]>('list-antibodies', ['--db', dbPath]);

  if (!result.ok) {
    host.showError(`Immunity: ${result.failure.message}`);
    return;
  }
  if (!Array.isArray(result.data)) {
    host.showError('Immunity: unexpected output from mycel-substrate.');
    return;
  }

  const antibodies = result.data;
  const title = antibodies.length > 0 ? ` Immunity (${antibodies.length}) ` : ' Immunity ';
  mountPanel(host, title, () => buildImmunityReportLines({ antibodies }));
}
