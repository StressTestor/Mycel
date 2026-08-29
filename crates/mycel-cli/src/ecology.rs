//! Native implementation of Mycel's seven mushroom-ecology slash commands.
//!
//! Substrate commands call `mycel-core` directly and delegation is returned as
//! a typed request for the native child-agent host. Environmental failures are
//! soft command outcomes so an interactive session never crashes because a
//! status panel could not be populated.

use std::{
    error::Error,
    fmt,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use mycel_core::{
    Antibody, AntibodySource, AuditLog, Confidence, Db, RefusalMode, SentinelAction, Severity,
    Signature, SignatureScope,
};
use mycel_mcp::McpTools;
use serde::Deserialize;
use uuid::Uuid;

use crate::util::short_id;

const DENY_REMEDIATION: &str =
    "Denied by operator via /deny. Do not run this command; use an approved alternative.";
const MAX_CANDIDATES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcologyCommand {
    Immunity,
    Gate,
    Substrate,
    Candidates,
    Promote,
    Deny,
    Delegate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EcologyCommandSpec {
    pub command: EcologyCommand,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
}

pub const ECOLOGY_COMMANDS: [EcologyCommandSpec; 7] = [
    EcologyCommandSpec {
        command: EcologyCommand::Immunity,
        name: "immunity",
        aliases: &["antibodies"],
        description: "show active antibodies",
    },
    EcologyCommandSpec {
        command: EcologyCommand::Gate,
        name: "gate",
        aliases: &["guard", "doorman"],
        description: "show fail-closed guard status",
    },
    EcologyCommandSpec {
        command: EcologyCommand::Substrate,
        name: "substrate",
        aliases: &["marrow"],
        description: "show substrate health",
    },
    EcologyCommandSpec {
        command: EcologyCommand::Candidates,
        name: "candidates",
        aliases: &["candidate", "learned"],
        description: "show learned but unsigned candidates",
    },
    EcologyCommandSpec {
        command: EcologyCommand::Promote,
        name: "promote",
        aliases: &["sign"],
        description: "sign a candidate into the substrate",
    },
    EcologyCommandSpec {
        command: EcologyCommand::Deny,
        name: "deny",
        aliases: &["refuse", "block"],
        description: "add a hard-refuse command antibody",
    },
    EcologyCommandSpec {
        command: EcologyCommand::Delegate,
        name: "delegate",
        aliases: &["handoff"],
        description: "hand work to a native governed subagent",
    },
];

/// Parse only the retained ecology family. Unknown slash commands return
/// `None` so the interactive host can preserve the normal text fallthrough.
pub fn parse_ecology_submission(input: &str) -> Option<(EcologyCommand, &str)> {
    let input = input.strip_prefix('/')?;
    let split = input.find(char::is_whitespace).unwrap_or(input.len());
    let (name, remainder) = input.split_at(split);
    let command = ECOLOGY_COMMANDS
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))?;
    Some((command.command, remainder.trim()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EcologyDispatch {
    Panel {
        title: String,
        lines: Vec<String>,
    },
    Error(String),
    Status(String),
    /// The interactive host must route this through `NativeChildAgentHost`.
    Delegate {
        task: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcologyPaths {
    pub mycel_home: PathBuf,
    pub config: PathBuf,
    pub database: PathBuf,
    pub audit: PathBuf,
    pub proposals: PathBuf,
}

impl EcologyPaths {
    pub fn new(mycel_home: impl Into<PathBuf>) -> Self {
        let mycel_home = mycel_home.into();
        let substrate = mycel_home.join("substrate");
        Self {
            config: mycel_home.join("config.toml"),
            database: substrate.join("mycel.db"),
            audit: substrate.join("audit.jsonl"),
            proposals: substrate.join("proposals.jsonl"),
            mycel_home,
        }
    }
}

/// What the gate can honestly claim about itself, condensed from the `/gate`
/// panel's own wiring/db matrix (see `EcologyService::gate`). `Tripwire` is
/// wired-fail-closed with the substrate db missing: every routed tool call is
/// being refused. `Disarmed` covers unwired and wired-fail-open, where the
/// fail-closed guarantee does not hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GateStatus {
    Ok,
    Tripwire,
    Disarmed,
    #[default]
    Unknown,
}

/// A cheap read-only snapshot of the substrate for the TUI. Built by
/// `EcologyService::summary` at construction and after ecology-mutating
/// events; never on the render tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubstrateStatus {
    pub antibodies_active: u32,
    pub candidates_pending: u32,
    pub gate: GateStatus,
}

#[derive(Debug, Clone)]
pub struct EcologyService {
    paths: EcologyPaths,
}

impl EcologyService {
    pub fn new(mycel_home: impl Into<PathBuf>) -> Self {
        Self {
            paths: EcologyPaths::new(mycel_home),
        }
    }

    pub fn paths(&self) -> &EcologyPaths {
        &self.paths
    }

    pub fn run(
        &self,
        command: EcologyCommand,
        arguments: &str,
        now: DateTime<Utc>,
    ) -> EcologyDispatch {
        match command {
            EcologyCommand::Immunity => self.immunity(now),
            EcologyCommand::Gate => self.gate(),
            EcologyCommand::Substrate => self.substrate(),
            EcologyCommand::Candidates => self.candidates(now),
            EcologyCommand::Promote => self.promote(arguments, now),
            EcologyCommand::Deny => self.deny(arguments, now),
            EcologyCommand::Delegate => Self::delegate(arguments),
        }
    }

    /// Snapshot the substrate counts and gate state from the same primitives
    /// the `/immunity`, `/candidates`, and `/gate` panels read. A missing db is
    /// a valid state (0 antibodies, 0 candidates, gate per wiring), never an
    /// error: the summary feeds a status line, and a status line that errors
    /// out renders nothing at all.
    pub fn summary(&self, now: DateTime<Utc>) -> SubstrateStatus {
        let (antibodies_active, candidates_pending) = match self.open_existing() {
            Ok(tools) => {
                let antibodies = tools
                    .list_antibodies()
                    .map(|antibodies| {
                        antibodies
                            .iter()
                            .filter(|antibody| {
                                antibody.expires_at.is_none_or(|expires| expires > now)
                            })
                            .count()
                    })
                    .unwrap_or(0);
                let candidates = tools
                    .list_candidates(now)
                    .map(|candidates| candidates.len())
                    .unwrap_or(0);
                (
                    u32::try_from(antibodies).unwrap_or(u32::MAX),
                    u32::try_from(candidates).unwrap_or(u32::MAX),
                )
            }
            Err(_) => (0, 0),
        };
        // Mirrors the status matrix in `gate()`: ARMED -> Ok, ARMED-TRIPWIRE
        // -> Tripwire, DISARMED / ARMED-FAIL-OPEN -> Disarmed, and both
        // unreadable-config and unknown-fail-mode -> Unknown.
        let gate = match (
            read_gate_wiring(&self.paths.config),
            self.paths.database.is_file(),
        ) {
            (
                GateWiring::Wired {
                    fail_mode: GateFailMode::Closed,
                    ..
                },
                true,
            ) => GateStatus::Ok,
            (
                GateWiring::Wired {
                    fail_mode: GateFailMode::Closed,
                    ..
                },
                false,
            ) => GateStatus::Tripwire,
            (
                GateWiring::Wired {
                    fail_mode: GateFailMode::Open,
                    ..
                },
                _,
            )
            | (GateWiring::Unwired, _) => GateStatus::Disarmed,
            (
                GateWiring::Wired {
                    fail_mode: GateFailMode::Unknown,
                    ..
                },
                _,
            )
            | (GateWiring::Unreadable, _) => GateStatus::Unknown,
        };
        SubstrateStatus {
            antibodies_active,
            candidates_pending,
            gate,
        }
    }

    fn open_existing(&self) -> Result<McpTools, EcologyError> {
        if !self.paths.database.is_file() {
            return Err(EcologyError::DatabaseMissing(self.paths.database.clone()));
        }
        McpTools::open(&self.paths.database)
            .map_err(|error| EcologyError::Substrate(error.to_string()))
    }

    fn immunity(&self, now: DateTime<Utc>) -> EcologyDispatch {
        let tools = match self.open_existing() {
            Ok(tools) => tools,
            Err(error) => return EcologyDispatch::Error(format!("immunity: {error}")),
        };
        let antibodies = match tools.list_antibodies() {
            Ok(antibodies) => antibodies,
            Err(error) => return EcologyDispatch::Error(format!("immunity: {error}")),
        };
        let mut lines = vec!["your immune system - what the body will refuse".to_owned()];
        if antibodies.is_empty() {
            lines.push("no antibodies yet; every command passes".to_owned());
        } else {
            for severity in [Severity::Refuse, Severity::Warn, Severity::Info] {
                let group: Vec<_> = antibodies
                    .iter()
                    .filter(|antibody| antibody.severity == severity)
                    .collect();
                if group.is_empty() {
                    continue;
                }
                lines.push(format!("{} ({})", severity_label(severity), group.len()));
                for antibody in group {
                    let signature = signature_summary(&antibody.signature);
                    let fields = signature_fields(&antibody.signature);
                    let expired = antibody.expires_at.is_some_and(|expires| expires <= now);
                    lines.push(format!(
                        "  {signature}  {}  {}{}{}",
                        refusal_label(antibody.refusal_mode),
                        scope_label(antibody.signature.scope),
                        if antibody.hit_count > 0 {
                            format!(" ·{}x fired", antibody.hit_count)
                        } else {
                            String::new()
                        },
                        if expired { " (expired)" } else { "" }
                    ));
                    if fields.len() > 1 {
                        lines.push(format!("    matches also: {}", fields[1..].join(", ")));
                    }
                    lines.push(format!("    -> {}", one_line(&antibody.remediation)));
                }
            }
            let refuse = antibodies
                .iter()
                .filter(|item| item.severity == Severity::Refuse)
                .count();
            let warn = antibodies
                .iter()
                .filter(|item| item.severity == Severity::Warn)
                .count();
            let info = antibodies.len() - refuse - warn;
            lines.push(format!(
                "{refuse} refuse · {warn} warn · {info} info - {} antibodies active",
                antibodies.len()
            ));
        }
        panel(
            if antibodies.is_empty() {
                "Immunity".to_owned()
            } else {
                format!("Immunity ({})", antibodies.len())
            },
            lines,
        )
    }

    fn gate(&self) -> EcologyDispatch {
        let wiring = read_gate_wiring(&self.paths.config);
        let db_present = self.paths.database.is_file();
        let status = match (&wiring, db_present) {
            (GateWiring::Unreadable, _) => "STATUS UNKNOWN",
            (GateWiring::Unwired, _) => "DISARMED",
            (
                GateWiring::Wired {
                    fail_mode: GateFailMode::Open,
                    ..
                },
                _,
            ) => "ARMED - FAIL-OPEN",
            (
                GateWiring::Wired {
                    fail_mode: GateFailMode::Closed,
                    ..
                },
                false,
            ) => "ARMED - TRIPWIRE",
            (
                GateWiring::Wired {
                    fail_mode: GateFailMode::Closed,
                    ..
                },
                true,
            ) => "ARMED",
            (
                GateWiring::Wired {
                    fail_mode: GateFailMode::Unknown,
                    ..
                },
                _,
            ) => "STATUS UNKNOWN",
        };
        let mut lines = vec![
            "the doorman - fail-closed, deny by default".to_owned(),
            format!("Status       {status}"),
        ];
        match wiring {
            GateWiring::Unreadable => {
                lines.push("Guard hook   unknown (config unreadable or unparseable)".to_owned());
                lines.push("Matcher      unknown".to_owned());
                lines.push("Fail mode    unknown".to_owned());
            }
            GateWiring::Unwired => {
                lines.push("Guard hook   not wired".to_owned());
                lines.push("Matcher      n/a".to_owned());
                lines.push("Fail mode    n/a".to_owned());
            }
            GateWiring::Wired { matcher, fail_mode } => {
                lines.push("Guard hook   mycel-gate  PreToolUse".to_owned());
                lines.push(format!(
                    "Matcher      {}",
                    if matcher.is_empty() {
                        "catch-all (\"\")".to_owned()
                    } else {
                        matcher
                    }
                ));
                lines.push(format!(
                    "Fail mode    {}",
                    match fail_mode {
                        GateFailMode::Closed => "closed",
                        GateFailMode::Open => "open",
                        GateFailMode::Unknown => "unknown",
                    }
                ));
            }
        }
        lines.push(format!(
            "Substrate db {}  {}",
            if db_present { "present" } else { "MISSING" },
            self.paths.database.display()
        ));
        match self.open_existing().and_then(|tools| {
            tools
                .list_antibodies()
                .map_err(|error| EcologyError::Substrate(error.to_string()))
        }) {
            Ok(antibodies) => {
                let refuse = antibodies
                    .iter()
                    .filter(|item| item.severity == Severity::Refuse)
                    .count();
                let warn = antibodies
                    .iter()
                    .filter(|item| item.severity == Severity::Warn)
                    .count();
                lines.push(format!(
                    "Antibodies  {} active ({refuse} refuse, {warn} warn)",
                    antibodies.len()
                ));
            }
            Err(error) => lines.push(format!("Antibodies  unavailable ({error})")),
        }
        lines.push("Protected   bin/  config.toml  substrate/".to_owned());
        lines.push("            compiled floor, cannot be disabled by config".to_owned());
        panel("Gate", lines)
    }

    fn substrate(&self) -> EcologyDispatch {
        let tools = match self.open_existing() {
            Ok(tools) => tools,
            Err(error) => {
                return panel(
                    "Substrate",
                    vec![
                        ">_ Substrate (the marrow - what persists across sessions)".to_owned(),
                        error.to_string(),
                        format!("db  {}", self.paths.database.display()),
                    ],
                )
            }
        };
        let antibodies = match tools.list_antibodies() {
            Ok(items) => items.len(),
            Err(error) => return EcologyDispatch::Error(format!("substrate: {error}")),
        };
        let candidates = match tools.sentinel_event_count() {
            Ok(count) => count,
            Err(error) => return EcologyDispatch::Error(format!("substrate: {error}")),
        };
        let (audit_bytes, audit_lines) = match audit_stats(&self.paths.audit) {
            Ok(stats) => stats,
            Err(error) => {
                return EcologyDispatch::Error(format!(
                    "substrate: could not read audit log: {error}"
                ))
            }
        };
        let maintenance = last_maintenance(&self.paths.database)
            .unwrap_or_else(|error| format!("unavailable ({error})"));
        panel(
            "Substrate",
            vec![
                ">_ Substrate (the marrow - what persists across sessions)".to_owned(),
                format!("Antibodies      {antibodies} active"),
                format!("Candidates      {candidates} pending"),
                format!("Audit log       {audit_bytes} bytes · {audit_lines} lines"),
                format!("Last maintenance {maintenance}"),
                format!("db  {}", self.paths.database.display()),
            ],
        )
    }

    fn candidates(&self, now: DateTime<Utc>) -> EcologyDispatch {
        let tools = match self.open_existing() {
            Ok(tools) => tools,
            Err(error) => return EcologyDispatch::Error(format!("candidates: {error}")),
        };
        let candidates = match tools.list_candidates(now) {
            Ok(candidates) => candidates,
            Err(error) => return EcologyDispatch::Error(format!("candidates: {error}")),
        };
        let mut lines = vec!["learned, not yet trusted".to_owned()];
        if candidates.is_empty() {
            lines.push("nothing captured yet; the loop has learned nothing to sign".to_owned());
        } else {
            lines.push("Tool  Signal  Would  Rule".to_owned());
            let mut refuse = 0usize;
            let mut warn = 0usize;
            let mut allow = 0usize;
            for candidate in candidates.iter().take(MAX_CANDIDATES) {
                let outcome =
                    outcome_preview(candidate.antibody.severity, candidate.antibody.refusal_mode);
                match outcome {
                    "refuse" => refuse += 1,
                    "warn" => warn += 1,
                    _ => allow += 1,
                }
                let rule = candidate
                    .metadata
                    .matched_rule
                    .as_deref()
                    .unwrap_or("(no rule)");
                lines.push(format!(
                    "{}  {}  {}  {}",
                    candidate.source.tool_name,
                    action_label(candidate.source.action),
                    outcome,
                    one_line(rule)
                ));
                if let Some(reason) = candidate
                    .metadata
                    .reason
                    .as_deref()
                    .filter(|reason| !reason.trim().is_empty())
                {
                    lines.push(format!("  why: {}", one_line(reason)));
                }
            }
            if candidates.len() > MAX_CANDIDATES {
                lines.push(format!("and {} more...", candidates.len() - MAX_CANDIDATES));
            }
            lines.push(format!(
                "{} learned, not yet trusted · {refuse} would-refuse · {warn} would-warn · {allow} log-only",
                candidates.len(),
            ));
            lines.push("promotion is manual: /promote <id>".to_owned());
        }
        panel(
            if candidates.is_empty() {
                "Candidates".to_owned()
            } else {
                format!("Candidates ({})", candidates.len())
            },
            lines,
        )
    }

    fn promote(&self, arguments: &str, now: DateTime<Utc>) -> EcologyDispatch {
        let proposals = read_proposals(&self.paths.proposals);
        let tokens: Vec<_> = arguments.split_whitespace().collect();
        if tokens.is_empty() {
            let mut lines = vec!["learned proposals - sign one with /promote <id>".to_owned()];
            if proposals.is_empty() {
                lines.push("no candidates yet".to_owned());
            } else {
                for proposal in proposals {
                    lines.push(format!(
                        "{}  {}  -> {}",
                        short_id(&proposal.id),
                        proposal.signature.summary(),
                        proposal.remediation()
                    ));
                }
            }
            return panel("Candidates", lines);
        }
        if tokens.len() > 3 {
            return EcologyDispatch::Error(
                "usage: /promote <id> [refuse|warn|info] [hard|soft|log-only]".to_owned(),
            );
        }
        let severity = match tokens.get(1).copied().unwrap_or("warn") {
            "refuse" => Severity::Refuse,
            "warn" => Severity::Warn,
            "info" => Severity::Info,
            value => {
                return EcologyDispatch::Error(format!(
                    "invalid severity {value:?} (expected refuse|warn|info)"
                ))
            }
        };
        let default_mode = if tokens.len() == 2 && severity == Severity::Refuse {
            RefusalMode::Hard
        } else {
            RefusalMode::Soft
        };
        let refusal_mode = match tokens.get(2).copied() {
            None => default_mode,
            Some("hard") => RefusalMode::Hard,
            Some("soft") => RefusalMode::Soft,
            Some("log-only") => RefusalMode::LogOnly,
            Some(value) => {
                return EcologyDispatch::Error(format!(
                    "invalid refusal-mode {value:?} (expected hard|soft|log-only)"
                ))
            }
        };
        let exact = proposals.iter().find(|proposal| proposal.id == tokens[0]);
        let matching: Vec<_> = proposals
            .iter()
            .filter(|proposal| proposal.id.starts_with(tokens[0]))
            .collect();
        let proposal = match exact {
            Some(proposal) => proposal,
            None => match matching.as_slice() {
                [] => {
                    return EcologyDispatch::Error(format!(
                        "no candidate matches {}; run /promote to list pending ones",
                        tokens[0]
                    ))
                }
                [proposal] => *proposal,
                many => {
                    return EcologyDispatch::Error(format!(
                        "ambiguous id {} - matches {}",
                        tokens[0],
                        many.iter()
                            .map(|proposal| short_id(&proposal.id))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            },
        };
        let signature = match proposal.signature.to_core() {
            Some(signature) => signature,
            None => {
                return EcologyDispatch::Error(format!(
                    "candidate {} has an empty signature; cannot promote",
                    short_id(&proposal.id)
                ))
            }
        };
        let new_id = Uuid::new_v4();
        let antibody = Antibody {
            id: new_id,
            signature,
            source: AntibodySource::Manual,
            severity,
            confidence: Confidence::Solid,
            refusal_mode,
            remediation: proposal.remediation(),
            examples: Vec::new(),
            created_at: now,
            expires_at: None,
            hit_count: 0,
        };
        let tools = match self.open_existing() {
            Ok(tools) => tools,
            Err(error) => return EcologyDispatch::Error(format!("promote: {error}")),
        };
        if let Err(error) = tools.insert_antibodies([antibody.clone()]) {
            return EcologyDispatch::Error(format!("promote: {error}"));
        }
        let outcome = outcome_preview(severity, refusal_mode);
        let mut lines = vec![
            "signed into the substrate".to_owned(),
            format!(
                "antibody    {} (new live id)",
                short_id(&new_id.to_string())
            ),
            format!("from        proposal {}", short_id(&proposal.id)),
            format!("signature   {}", signature_summary(&antibody.signature)),
            format!("reflex      {outcome} (outcome preview)"),
            format!("remediation {}", one_line(&antibody.remediation)),
            "scope       project · source curated".to_owned(),
        ];
        if severity == Severity::Refuse && outcome != "refuse" {
            lines.push(format!(
                "warning: refuse severity with {} mode does not hard-block",
                refusal_label(refusal_mode)
            ));
        }
        lines.push("this reflex is live now".to_owned());
        panel("Signed", lines)
    }

    fn deny(&self, arguments: &str, now: DateTime<Utc>) -> EcologyDispatch {
        let pattern = arguments.trim();
        if pattern.is_empty() {
            return EcologyDispatch::Status("usage: /deny <command-pattern>".to_owned());
        }
        let id = Uuid::new_v4();
        let antibody = Antibody {
            id,
            signature: Signature {
                error_class: None,
                file_pattern: None,
                agent_role: None,
                tool_pattern: None,
                command_pattern: Some(pattern.to_owned()),
                scope: SignatureScope::Project,
            },
            source: AntibodySource::Manual,
            severity: Severity::Refuse,
            confidence: Confidence::Solid,
            refusal_mode: RefusalMode::Hard,
            remediation: DENY_REMEDIATION.to_owned(),
            examples: Vec::new(),
            created_at: now,
            expires_at: None,
            hit_count: 0,
        };
        let tools = match self.open_existing() {
            Ok(tools) => tools,
            Err(error) => return EcologyDispatch::Error(format!("deny: {error}")),
        };
        if let Err(error) = tools.insert_antibodies([antibody]) {
            return EcologyDispatch::Error(format!("deny: {error}"));
        }
        panel(
            "Antibody",
            vec![
                "taught the gate to refuse this".to_owned(),
                format!("pattern   {}", one_line(pattern)),
                "verdict   refuse · hard refusal (fails closed)".to_owned(),
                "scope     project".to_owned(),
                format!("antibody  {}", short_id(&id.to_string())),
                "next matching command is blocked before it runs".to_owned(),
            ],
        )
    }

    fn delegate(arguments: &str) -> EcologyDispatch {
        let task = arguments.trim();
        if task.is_empty() {
            EcologyDispatch::Error("usage: /delegate <task>".to_owned())
        } else {
            EcologyDispatch::Delegate {
                task: task.to_owned(),
            }
        }
    }
}

fn panel(title: impl Into<String>, lines: Vec<String>) -> EcologyDispatch {
    EcologyDispatch::Panel {
        title: title.into(),
        lines,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GateWiring {
    Unreadable,
    Unwired,
    Wired {
        matcher: String,
        fail_mode: GateFailMode,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateFailMode {
    Closed,
    Open,
    Unknown,
}

fn read_gate_wiring(path: &Path) -> GateWiring {
    let Ok(raw) = fs::read_to_string(path) else {
        return GateWiring::Unreadable;
    };
    let Ok(value) = raw.parse::<toml::Value>() else {
        return GateWiring::Unreadable;
    };
    let Some(hooks) = value.get("hooks").and_then(toml::Value::as_array) else {
        return GateWiring::Unwired;
    };
    for hook in hooks {
        let Some(table) = hook.as_table() else {
            continue;
        };
        let event = table.get("event").and_then(toml::Value::as_str);
        let command = table.get("command").and_then(toml::Value::as_str);
        if event != Some("PreToolUse") || !command.is_some_and(|value| value.contains("mycel-gate"))
        {
            continue;
        }
        return GateWiring::Wired {
            matcher: table
                .get("matcher")
                .and_then(toml::Value::as_str)
                .unwrap_or("")
                .to_owned(),
            fail_mode: match table.get("fail_mode").and_then(toml::Value::as_str) {
                Some("closed") => GateFailMode::Closed,
                Some("open") => GateFailMode::Open,
                _ => GateFailMode::Unknown,
            },
        };
    }
    GateWiring::Unwired
}

#[derive(Debug, Clone, Deserialize)]
struct Proposal {
    id: String,
    #[serde(default)]
    signature: ProposalSignature,
    remediation: Option<String>,
    rationale: Option<String>,
}

impl Proposal {
    fn remediation(&self) -> String {
        self.remediation
            .as_deref()
            .or(self.rationale.as_deref())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Promoted from proposal {}", short_id(&self.id)))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProposalSignature {
    command_pattern: Option<String>,
    error_class: Option<String>,
    file_pattern: Option<String>,
    tool_name: Option<String>,
}

impl ProposalSignature {
    fn to_core(&self) -> Option<Signature> {
        if self.command_pattern.is_none()
            && self.error_class.is_none()
            && self.file_pattern.is_none()
            && self.tool_name.is_none()
        {
            return None;
        }
        Some(Signature {
            error_class: self.error_class.clone(),
            file_pattern: self.file_pattern.clone(),
            agent_role: None,
            tool_pattern: self.tool_name.clone(),
            command_pattern: self.command_pattern.clone(),
            scope: SignatureScope::Project,
        })
    }

    fn summary(&self) -> String {
        self.command_pattern
            .as_ref()
            .map(|value| format!("command_pattern: {}", one_line(value)))
            .or_else(|| {
                self.tool_name
                    .as_ref()
                    .map(|value| format!("tool_name: {}", one_line(value)))
            })
            .or_else(|| {
                self.error_class
                    .as_ref()
                    .map(|value| format!("error_class: {}", one_line(value)))
            })
            .or_else(|| {
                self.file_pattern
                    .as_ref()
                    .map(|value| format!("file_pattern: {}", one_line(value)))
            })
            .unwrap_or_else(|| "(no signature)".to_owned())
    }
}

fn read_proposals(path: &Path) -> Vec<Proposal> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Proposal>(line.trim()).ok())
        .filter(|proposal| !proposal.id.is_empty())
        .collect()
}

fn audit_stats(path: &Path) -> std::io::Result<(u64, u64)> {
    if !path.is_file() {
        return Ok((0, 0));
    }
    let bytes = fs::metadata(path)?.len();
    let lines = BufReader::new(File::open(path)?)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .count() as u64;
    Ok((bytes, lines))
}

fn last_maintenance(path: &Path) -> Result<String, EcologyError> {
    let db = Db::open(path).map_err(|error| EcologyError::Substrate(error.to_string()))?;
    let entries = AuditLog::new(&db)
        .list()
        .map_err(|error| EcologyError::Substrate(error.to_string()))?;
    let Some(entry) = entries
        .into_iter()
        .rfind(|entry| entry.event == "maintenance")
    else {
        return Ok("never run".to_owned());
    };
    let number = |key: &str| {
        entry
            .payload
            .get(key)
            .and_then(|value| value.as_u64())
            .unwrap_or(0)
    };
    Ok(format!(
        "{} · decayed {} · distilled {} · retained {} · preserved {}",
        entry.ts,
        number("decayed"),
        number("distilled"),
        number("retained"),
        number("preserved")
    ))
}

fn signature_summary(signature: &Signature) -> String {
    signature_fields(signature)
        .into_iter()
        .next()
        .unwrap_or_else(|| "(no signature)".to_owned())
}

fn signature_fields(signature: &Signature) -> Vec<String> {
    let mut fields = Vec::new();
    if let Some(value) = &signature.command_pattern {
        fields.push(format!("cmd: {}", one_line(value)));
    }
    if let Some(value) = &signature.tool_pattern {
        fields.push(format!("tool: {}", one_line(value)));
    }
    if let Some(value) = &signature.file_pattern {
        fields.push(format!("file: {}", one_line(value)));
    }
    if let Some(value) = &signature.error_class {
        fields.push(format!("err: {}", one_line(value)));
    }
    if let Some(value) = &signature.agent_role {
        fields.push(format!("role: {}", one_line(value)));
    }
    fields
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn severity_label(value: Severity) -> &'static str {
    match value {
        Severity::Refuse => "REFUSE",
        Severity::Warn => "WARN",
        Severity::Info => "INFO",
    }
}

fn refusal_label(value: RefusalMode) -> &'static str {
    match value {
        RefusalMode::Hard => "hard",
        RefusalMode::Soft => "soft",
        RefusalMode::LogOnly => "log-only",
    }
}

fn scope_label(value: SignatureScope) -> &'static str {
    match value {
        SignatureScope::Project => "project",
        SignatureScope::Global => "global",
        SignatureScope::Personal => "personal",
    }
}

fn action_label(value: SentinelAction) -> &'static str {
    match value {
        SentinelAction::Block => "block",
        SentinelAction::Warn => "warn",
        SentinelAction::Allow => "allow",
    }
}

fn outcome_preview(severity: Severity, refusal_mode: RefusalMode) -> &'static str {
    match (severity, refusal_mode) {
        (Severity::Refuse, RefusalMode::Hard) => "refuse",
        (_, RefusalMode::LogOnly) | (Severity::Info, _) => "allow",
        _ => "warn",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EcologyError {
    DatabaseMissing(PathBuf),
    Substrate(String),
}

impl fmt::Display for EcologyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseMissing(path) => write!(
                formatter,
                "substrate not initialized at {}; run install.sh",
                path.display()
            ),
            Self::Substrate(error) => write!(formatter, "could not read substrate: {error}"),
        }
    }
}

impl Error for EcologyError {}

#[cfg(test)]
mod tests {
    use super::*;
    use mycel_core::{EvaluationOutcome, ProposedRun};

    fn fixture() -> (tempfile::TempDir, EcologyService) {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(directory.path().join("substrate")).expect("substrate dir");
        let service = EcologyService::new(directory.path());
        McpTools::open(&service.paths.database).expect("initialize db");
        (directory, service)
    }

    #[test]
    fn registry_recognizes_exact_names_and_aliases_with_unknown_fallthrough() {
        assert_eq!(ECOLOGY_COMMANDS.len(), 7);
        assert_eq!(
            parse_ecology_submission("/guard"),
            Some((EcologyCommand::Gate, ""))
        );
        assert_eq!(
            parse_ecology_submission("/handoff  inspect this "),
            Some((EcologyCommand::Delegate, "inspect this"))
        );
        assert_eq!(parse_ecology_submission("/unknown command"), None);
        assert_eq!(parse_ecology_submission("plain text"), None);
    }

    #[test]
    fn read_only_commands_never_create_a_missing_database() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service = EcologyService::new(directory.path());
        for command in [
            EcologyCommand::Immunity,
            EcologyCommand::Gate,
            EcologyCommand::Substrate,
            EcologyCommand::Candidates,
        ] {
            let _ = service.run(command, "", Utc::now());
        }
        assert!(!service.paths.database.exists());
    }

    #[test]
    fn gate_reports_wiring_database_and_compiled_floor() {
        let (_directory, service) = fixture();
        fs::write(
            &service.paths.config,
            r#"[[hooks]]
event = "PreToolUse"
matcher = ""
command = "$HOME/.mycel/bin/mycel-gate"
fail_mode = "closed"
"#,
        )
        .expect("config");
        let EcologyDispatch::Panel { lines, .. } =
            service.run(EcologyCommand::Gate, "", Utc::now())
        else {
            panic!("gate panel");
        };
        let report = lines.join("\n");
        assert!(report.contains("ARMED"));
        assert!(report.contains("catch-all"));
        assert!(report.contains("bin/  config.toml  substrate/"));
    }

    #[test]
    fn deny_writes_a_live_hard_refusal_without_shell_parsing() {
        let (_directory, service) = fixture();
        let pattern = "rm -rf '$HOME/safe value'";
        let outcome = service.run(EcologyCommand::Deny, pattern, Utc::now());
        assert!(matches!(outcome, EcologyDispatch::Panel { .. }));
        let evaluation = McpTools::open(&service.paths.database)
            .expect("tools")
            .evaluate(
                &ProposedRun {
                    error_class: None,
                    file_path: None,
                    agent_role: None,
                    tool_name: Some("Bash".to_owned()),
                    command: Some(pattern.to_owned()),
                    scope: SignatureScope::Project,
                },
                Utc::now(),
            )
            .expect("evaluate");
        assert_eq!(evaluation.outcome, EvaluationOutcome::Refuse);
    }

    #[test]
    fn promotion_skips_corrupt_lines_resolves_unique_prefix_and_defaults_soft() {
        let (_directory, service) = fixture();
        fs::write(
            &service.paths.proposals,
            concat!(
                "not-json\n",
                r#"{"id":"abcdef12-0000-0000-0000-000000000000","signature":{"tool_name":"Bash"},"remediation":"use Read","rationale":null}"#,
                "\n"
            ),
        )
        .expect("proposals");
        let outcome = service.run(EcologyCommand::Promote, "abcdef12", Utc::now());
        let EcologyDispatch::Panel { lines, .. } = outcome else {
            panic!("promote panel");
        };
        assert!(lines.join("\n").contains("reflex      warn"));
        let antibodies = McpTools::open(&service.paths.database)
            .expect("tools")
            .list_antibodies()
            .expect("antibodies");
        assert_eq!(antibodies.len(), 1);
        assert_eq!(antibodies[0].refusal_mode, RefusalMode::Soft);
        assert_eq!(antibodies[0].source, AntibodySource::Manual);
    }

    fn wire_gate(service: &EcologyService, fail_mode: &str) {
        fs::write(
            &service.paths.config,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nmatcher = \"\"\ncommand = \"$HOME/.mycel/bin/mycel-gate\"\nfail_mode = \"{fail_mode}\"\n"
            ),
        )
        .expect("config");
    }

    #[test]
    fn summary_counts_live_antibodies_and_candidates_with_an_armed_gate() {
        let (_directory, service) = fixture();
        wire_gate(&service, "closed");
        let now = Utc::now();
        service.run(EcologyCommand::Deny, "rm -rf /", now);
        let tools = McpTools::open(&service.paths.database).expect("tools");
        tools
            .ingest_sentinel(
                r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"shell","action":"block","reason":"blocked ssh key access","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#.as_bytes(),
                now,
            )
            .expect("ingest candidate");
        assert_eq!(
            service.summary(now),
            SubstrateStatus {
                antibodies_active: 1,
                candidates_pending: 1,
                gate: GateStatus::Ok,
            }
        );
    }

    #[test]
    fn summary_excludes_expired_antibodies() {
        let (_directory, service) = fixture();
        let now = Utc::now();
        let expired = Antibody {
            id: Uuid::new_v4(),
            signature: Signature {
                error_class: None,
                file_pattern: None,
                agent_role: None,
                tool_pattern: None,
                command_pattern: Some("old".to_owned()),
                scope: SignatureScope::Project,
            },
            source: AntibodySource::Manual,
            severity: Severity::Refuse,
            confidence: Confidence::Solid,
            refusal_mode: RefusalMode::Hard,
            remediation: "expired".to_owned(),
            examples: Vec::new(),
            created_at: now - chrono::Duration::days(2),
            expires_at: Some(now - chrono::Duration::days(1)),
            hit_count: 0,
        };
        McpTools::open(&service.paths.database)
            .expect("tools")
            .insert_antibodies([expired])
            .expect("insert");
        assert_eq!(service.summary(now).antibodies_active, 0);
    }

    #[test]
    fn summary_gate_state_mirrors_the_gate_panel_matrix() {
        // Wired fail-closed without a db: the tripwire state, every call
        // refused. The db must NOT be created by the summary read.
        let directory = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(directory.path().join("substrate")).expect("substrate dir");
        let service = EcologyService::new(directory.path());
        wire_gate(&service, "closed");
        let summary = service.summary(Utc::now());
        assert_eq!(summary.gate, GateStatus::Tripwire);
        assert_eq!(summary.antibodies_active, 0);
        assert!(!service.paths.database.exists());

        // Fail-open and unwired both surrender the fail-closed guarantee.
        wire_gate(&service, "open");
        assert_eq!(service.summary(Utc::now()).gate, GateStatus::Disarmed);
        fs::write(&service.paths.config, "").expect("unwired config");
        assert_eq!(service.summary(Utc::now()).gate, GateStatus::Disarmed);

        // Unreadable config and unknown fail mode are honest Unknowns.
        fs::write(&service.paths.config, "not toml [").expect("bad config");
        assert_eq!(service.summary(Utc::now()).gate, GateStatus::Unknown);
        fs::write(
            &service.paths.config,
            "[[hooks]]\nevent = \"PreToolUse\"\ncommand = \"mycel-gate\"\n",
        )
        .expect("no fail mode");
        assert_eq!(service.summary(Utc::now()).gate, GateStatus::Unknown);
    }

    #[test]
    fn delegate_is_a_native_request_not_a_claude_process() {
        assert_eq!(
            EcologyService::delegate(" inspect the runtime "),
            EcologyDispatch::Delegate {
                task: "inspect the runtime".to_owned()
            }
        );
        assert!(matches!(
            EcologyService::delegate("  "),
            EcologyDispatch::Error(_)
        ));
    }
}
