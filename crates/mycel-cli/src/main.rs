use std::{fs::File, path::PathBuf};

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use mycel_core::{ProposedRun, SignatureScope};
use mycel_mcp::McpTools;
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "mycel")]
#[command(about = "local Mycel v0.1 harness")]
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
