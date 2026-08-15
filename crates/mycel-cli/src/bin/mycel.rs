use std::{io::Write, process::ExitCode};

use clap::{error::ErrorKind, Parser};
use mycel_cli::{
    cli::Cli,
    execute_process,
    exit::{ERROR, SUCCESS},
    ProductionRuntimeAdapter,
};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return render_clap_error(error),
    };

    let mut adapter = match ProductionRuntimeAdapter::from_process() {
        Ok(adapter) => adapter,
        Err(error) => {
            eprintln!("error: {error}");
            return exit_code(ERROR);
        }
    };
    match execute_process(cli, &mut adapter) {
        Ok(result) => {
            let _ = std::io::stdout().write_all(result.stdout.as_bytes());
            let _ = std::io::stderr().write_all(result.stderr.as_bytes());
            exit_code(result.exit_code)
        }
        Err(error) => {
            eprintln!("error: {error}");
            exit_code(ERROR)
        }
    }
}

fn render_clap_error(error: clap::Error) -> ExitCode {
    let success = matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    );
    if success {
        print!("{error}");
    } else {
        eprint!("{error}");
    }
    exit_code(if success { SUCCESS } else { ERROR })
}

fn exit_code(code: i32) -> ExitCode {
    let code = u8::try_from(code).unwrap_or(u8::MAX);
    ExitCode::from(code)
}
