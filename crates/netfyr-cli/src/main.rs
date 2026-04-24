use netfyr_cli::{resolve_color_mode, run_apply, run_completions, run_diagnose, run_history, run_query, run_revert, Cli, Commands};

use clap::Parser;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    resolve_color_mode(&cli.color);

    match cli.command {
        Commands::Apply(args) => match run_apply(args).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("Error: {:#}", e);
                ExitCode::from(2u8)
            }
        },
        Commands::Query(args) => match run_query(args).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("Error: {:#}", e);
                ExitCode::from(2u8)
            }
        },
        Commands::History(args) => match run_history(args).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("Error: {:#}", e);
                ExitCode::from(2u8)
            }
        },
        Commands::Revert(args) => match run_revert(args).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("Error: {:#}", e);
                ExitCode::from(2u8)
            }
        },
        Commands::Diagnose(args) => match run_diagnose(args).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("Error: {:#}", e);
                ExitCode::from(2u8)
            }
        },
        Commands::Completions(args) => run_completions(args),
    }
}
