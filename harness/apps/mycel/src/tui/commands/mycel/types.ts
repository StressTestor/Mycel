/**
 * TypeScript mirrors of the `mycel-substrate` JSON contract.
 *
 * These are parsed from the substrate CLI's stdout (serde_json). They are NOT
 * imported from Rust - they intentionally track the wire shape, so a version
 * drift shows up as a parse/shape mismatch the panels degrade on rather than a
 * type error here.
 */

export type SubstrateSeverity = 'refuse' | 'warn' | 'info';
/** snake_case from the Rust enum (`#[serde(rename_all = "snake_case")]`). */
export type SubstrateRefusalMode = 'hard' | 'soft' | 'log_only';
export type SubstrateScope = 'global' | 'project' | 'personal';
export type SentinelActionValue = 'block' | 'warn' | 'allow';

export interface SubstrateSignature {
  readonly error_class: string | null;
  readonly file_pattern: string | null;
  readonly agent_role: string | null;
  readonly tool_pattern: string | null;
  readonly command_pattern: string | null;
  readonly scope: SubstrateScope;
}

export interface Antibody {
  readonly id: string;
  readonly signature: SubstrateSignature;
  readonly source: string;
  readonly severity: SubstrateSeverity;
  readonly confidence: string;
  readonly refusal_mode: SubstrateRefusalMode;
  readonly remediation: string;
  readonly examples: readonly string[];
  readonly created_at: string;
  readonly expires_at: string | null;
  readonly hit_count: number;
}

export interface SentinelCandidate {
  readonly source: {
    readonly event_id: string;
    readonly timestamp: string;
    readonly tool_name: string;
    readonly action: SentinelActionValue;
    readonly mode: string;
  };
  readonly metadata: {
    readonly reason: string | null;
    readonly matched_rule: string | null;
  };
  readonly antibody: Antibody;
}

export interface MaintenanceSummary {
  readonly ts: number;
  readonly retained: number;
  readonly distilled: number;
  readonly decayed: number;
  readonly preserved: number;
  readonly skipped_live: number;
}

export interface SubstrateStatus {
  readonly antibody_count: number;
  readonly sentinel_event_count: number;
  readonly audit_bytes: number;
  readonly audit_lines: number;
  readonly last_maintenance: MaintenanceSummary | null;
}

/** One line of `proposals.jsonl` (written by the MCP `propose_antibody` tool). */
export interface Proposal {
  readonly id: string;
  readonly created_at: string;
  readonly signature: {
    readonly command_pattern?: string | null;
    readonly error_class?: string | null;
    readonly file_pattern?: string | null;
    readonly tool_name?: string | null;
  };
  readonly remediation: string | null;
  readonly rationale: string | null;
}

/** stdout of a successful `antibody-add`. */
export interface AntibodyAddResult {
  readonly id: string;
  readonly outcome_preview: 'refuse' | 'warn' | 'allow';
}
