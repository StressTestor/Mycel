use std::{fs::File, io::BufRead, io::BufReader, path::PathBuf};

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use mycel_core::{Db, PromptPressureRecord, ProposedRun, SignatureScope};
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
