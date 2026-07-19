use std::{fs::File, io::BufRead, io::BufReader, path::PathBuf};

use anyhow::{bail, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use mycel_core::{
    Antibody, AntibodySource, Confidence, Db, PromptPressureRecord, ProposedRun, RefusalMode,
    Severity, Signature, SignatureScope,
};
use mycel_mcp::McpTools;
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "mycel")]
#[command(about = "local Mycel harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Harness,
    Ingest {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long)]
        jsonl: PathBuf,
    },
    Evaluate {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long)]
        tool_name: String,
        #[arg(long)]
        error_class: Option<String>,
        #[arg(long, default_value = "project")]
        scope: String,
    },
    ListAntibodies {
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Insert one fully-specified, curated antibody into an existing substrate.
    AntibodyAdd {
        /// Path to the substrate db. Must already exist; antibody-add never creates it.
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        command_pattern: Option<String>,
        #[arg(long)]
        tool_name: Option<String>,
        #[arg(long)]
        error_class: Option<String>,
        #[arg(long)]
        file_pattern: Option<String>,
        #[arg(long)]
        remediation: String,
        /// One of: refuse, warn, info.
        #[arg(long)]
        severity: String,
        /// One of: hard, soft, log-only.
        #[arg(long)]
        refusal_mode: String,
        /// One of: global, project, personal.
        #[arg(long, default_value = "project")]
        scope: String,
        /// Provenance label. "curated"/"manual" store as manual; sentinel/failed-run also accepted.
        #[arg(long, default_value = "curated")]
        source: String,
    },
    /// Apply decay, regenerate SUBSTRATE.md and COMPOST.md, and append a maintenance audit event.
    Maintain {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        /// Unix timestamp for the maintenance cycle. Defaults to now.
        #[arg(long)]
        now: Option<i64>,
    },
    /// Import PromptPressure JSONL records into the run substrate.
    ImportPromptpressure {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        jsonl: PathBuf,
        /// Unix timestamp for the import. Defaults to now.
        #[arg(long)]
        now: Option<i64>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Harness => {
            let tools = McpTools::open_in_memory()?;
            let metrics = tools.run_harness(Utc::now())?;
            println!("{}", serde_json::to_string_pretty(&metrics)?);
        }
        Command::Ingest { db, jsonl } => {
            let tools = open_tools(db)?;
            let file = File::open(jsonl)?;
            let candidates = tools.ingest_sentinel(file, Utc::now())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "candidates": candidates.len() }))?
            );
        }
        Command::Evaluate {
            db,
            tool_name,
            error_class,
            scope,
        } => {
            let tools = open_tools(db)?;
            let evaluation = tools.evaluate(
                &ProposedRun {
                    error_class,
                    file_path: None,
                    agent_role: None,
                    tool_name: Some(tool_name),
                    command: None,
                    scope: parse_scope(&scope),
                },
                Utc::now(),
            )?;
            println!("{}", serde_json::to_string_pretty(&evaluation)?);
        }
        Command::ListAntibodies { db } => {
            let tools = open_tools(db)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&tools.list_antibodies()?)?
            );
        }
        Command::AntibodyAdd {
            db,
            command_pattern,
            tool_name,
            error_class,
            file_pattern,
            remediation,
            severity,
            refusal_mode,
            scope,
            source,
        } => {
            // SECURITY: never create the db - consistent with the gate's disarm rule. A missing
            // db means the substrate was never initialized (or was deleted); fail loudly.
            if !db.exists() {
                bail!(
                    "antibody-add error: substrate db missing at {}: run install.sh to initialize (never auto-created)",
                    db.display()
                );
            }
            if command_pattern.is_none()
                && tool_name.is_none()
                && error_class.is_none()
                && file_pattern.is_none()
            {
                bail!("antibody-add error: at least one signature field required (--command-pattern, --tool-name, --error-class, --file-pattern)");
            }

            let severity = parse_severity(&severity)?;
            let refusal_mode = parse_refusal_mode(&refusal_mode)?;
            let source = parse_source(&source)?;
            let preview = outcome_preview(severity, refusal_mode);

            // Loud at seeding time: a refuse-severity antibody that can never hard-block is almost
            // always a misconfigured gate.
            if severity == Severity::Refuse && preview != "refuse" {
                eprintln!(
                    "antibody-add warning: severity=refuse with refusal-mode={} will NOT hard-block (only severity=refuse + refusal-mode=hard refuses); this antibody will {}",
                    refusal_mode_label(refusal_mode),
                    preview
                );
            }

            let id = uuid::Uuid::new_v4();
            let now = Utc::now();
            let antibody = Antibody {
                id,
                signature: Signature {
                    error_class,
                    file_pattern,
                    agent_role: None,
                    tool_pattern: tool_name,
                    command_pattern,
                    scope: parse_scope(&scope),
                },
                source,
                severity,
                confidence: Confidence::Solid,
                refusal_mode,
                remediation,
                examples: Vec::new(),
                created_at: now,
                expires_at: None,
                hit_count: 0,
            };

            let tools = McpTools::open(&db)?;
            tools.insert_antibodies([antibody])?;
            println!(
                "{}",
                json!({ "id": id.to_string(), "outcome_preview": preview })
            );
        }
        Command::Maintain { db, workspace, now } => {
            let now_ts = now.unwrap_or_else(|| Utc::now().timestamp());
            let substrate_db = Db::open(&db)?;
            let report = mycel_core::run_maintenance(&substrate_db, &workspace, now_ts)?;
            let decay = &report.decay;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "retained": decay.retained.len(),
                    "distilled": decay.distilled.len(),
                    "decayed": decay.decayed.len(),
                    "preserved": decay.preserved.len(),
                    "skipped_live": decay.skipped_live.len(),
                    "substrate_path": report.substrate_path,
                    "compost_path": report.compost_path,
                }))?
            );
        }
        Command::ImportPromptpressure { db, jsonl, now } => {
            let now_ts = now.unwrap_or_else(|| Utc::now().timestamp());
            let substrate_db = Db::open(&db)?;
            let file = File::open(jsonl)?;
            let mut records: Vec<PromptPressureRecord> = Vec::new();
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let record: PromptPressureRecord = serde_json::from_str(&line)?;
                records.push(record);
            }
            let ids = mycel_core::PromptPressureImport::new(&substrate_db)
                .import_batch(&records, now_ts)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "imported": ids.len() }))?
            );
        }
    }
    Ok(())
}

fn open_tools(db: Option<PathBuf>) -> Result<McpTools> {
    match db {
        Some(path) => Ok(McpTools::open(path)?),
        None => Ok(McpTools::open_in_memory()?),
    }
}

fn parse_scope(scope: &str) -> SignatureScope {
    match scope {
        "global" => SignatureScope::Global,
        "personal" => SignatureScope::Personal,
        _ => SignatureScope::Project,
    }
}

fn parse_severity(value: &str) -> Result<Severity> {
    match value {
        "refuse" => Ok(Severity::Refuse),
        "warn" => Ok(Severity::Warn),
        "info" => Ok(Severity::Info),
        other => {
            bail!("antibody-add error: unknown --severity {other:?} (expected refuse|warn|info)")
        }
    }
}

fn parse_refusal_mode(value: &str) -> Result<RefusalMode> {
    match value {
        "hard" => Ok(RefusalMode::Hard),
        "soft" => Ok(RefusalMode::Soft),
        "log-only" => Ok(RefusalMode::LogOnly),
        other => {
            bail!("antibody-add error: unknown --refusal-mode {other:?} (expected hard|soft|log-only)")
        }
    }
}

fn parse_source(value: &str) -> Result<AntibodySource> {
    // AntibodySource has no dedicated "curated" variant; a curated/manual seed is stored as Manual.
    match value {
        "curated" | "manual" => Ok(AntibodySource::Manual),
        "sentinel" | "sentinel-block" | "sentinel_block" => Ok(AntibodySource::SentinelBlock),
        "failed-run" | "failed_run" => Ok(AntibodySource::FailedRun),
        other => bail!(
            "antibody-add error: unknown --source {other:?} (expected curated|manual|sentinel|failed-run)"
        ),
    }
}

fn refusal_mode_label(mode: RefusalMode) -> &'static str {
    match mode {
        RefusalMode::Hard => "hard",
        RefusalMode::Soft => "soft",
        RefusalMode::LogOnly => "log-only",
    }
}

/// Mirror of `mycel_core`'s private `EvaluationOutcome::from_policy` (lib.rs:1069): a hard block
/// requires severity=Refuse AND refusal-mode=Hard; log-only or info always allows; else warn.
fn outcome_preview(severity: Severity, refusal_mode: RefusalMode) -> &'static str {
    match (severity, refusal_mode) {
        (Severity::Refuse, RefusalMode::Hard) => "refuse",
        (_, RefusalMode::LogOnly) => "allow",
        (Severity::Info, _) => "allow",
        _ => "warn",
    }
}
