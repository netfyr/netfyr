use netfyr_cli::{run_apply, run_history, run_query, Cli, Commands};

use clap::Parser;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().len() == 1 {
        println!("netfyr");
        std::process::exit(0);
    }

    let cli = Cli::parse();

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
    }
}
