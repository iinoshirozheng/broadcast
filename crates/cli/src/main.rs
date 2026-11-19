//! `broadcast`: read and write the board from a terminal or an agent. Results print as one JSON
//! line on stdout; failures print `{schemaVersion, error}` on stderr with a non-zero exit code.

mod args;
mod client;
mod commands;
mod error;
mod runner;
mod target;

use serde_json::{Map, Value};

use crate::error::{CliError, Result, usage};

pub const SCHEMA_VERSION: i32 = 2;

enum Outcome {
    Result(Map<String, Value>),
    Help(&'static str),
    /// A runner stopped by a shutdown signal.
    Interrupted,
}

fn main() {
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| CliError::new(error.to_string(), "INTERNAL_ERROR", 1))
        .and_then(|runtime| runtime.block_on(run(&argv)));
    let code = match outcome {
        Ok(Outcome::Result(mut result)) => {
            result.insert("schemaVersion".into(), SCHEMA_VERSION.into());
            println!("{}", Value::Object(result));
            0
        }
        Ok(Outcome::Help(help)) => {
            println!("{help}");
            0
        }
        Ok(Outcome::Interrupted) => 130,
        Err(error) => {
            eprintln!("{}", error.to_json());
            error.exit_code()
        }
    };
    std::process::exit(code)
}

async fn run(argv: &[String]) -> Result<Outcome> {
    let parsed = args::parse(argv)?;
    let command = parsed.command();
    if parsed.flag("help") {
        return match args::help(&command) {
            Some(help) if parsed.operands.is_empty() && parsed.options.len() == 1 => {
                Ok(Outcome::Help(help))
            }
            _ => Err(usage(
                "Help is available for broadcast, broadcast issue, and broadcast comment list",
            )),
        };
    }
    let allowed = args::allowed_options(&command).ok_or_else(|| usage(args::UNKNOWN_COMMAND))?;
    parsed.validate_options(&allowed)?;
    let agent = match command.as_str() {
        "runner claude" => Some(broadcast_agents::runner::Agent::Claude),
        "runner codex" => Some(broadcast_agents::runner::Agent::Codex),
        _ => None,
    };
    if let Some(agent) = agent {
        return runner::run(&parsed, agent)
            .await
            .map(|()| Outcome::Interrupted);
    }
    commands::execute(&parsed, &command)
        .await
        .map(Outcome::Result)
}
