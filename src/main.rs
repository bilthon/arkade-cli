use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use arkade_cli::config;
use arkade_cli::indexer::IndexerClient;
use arkade_cli::outpoint::Outpoint;
use arkade_cli::render;

/// akcli — a read-only explorer for Arkade virtual transactions and outputs.
#[derive(Debug, Parser)]
#[command(name = "akcli", version, about)]
struct Cli {
    /// Operator base URL (overrides the value in ~/.akcli/config.toml).
    #[arg(long, global = true)]
    operator: Option<String>,

    /// Emit machine-readable JSON instead of a human-readable report.
    #[arg(long, global = true)]
    json: bool,

    /// Colorize output: always, never, or auto (default; TTY-only, honors NO_COLOR).
    #[arg(long, global = true, default_value = "auto", value_parser = ["always", "never", "auto"])]
    color: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect a single VTXO by outpoint (<txid>:<vout>).
    Vtxo {
        /// Outpoint in the form <txid>:<vout>.
        outpoint: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let config_path = config::config_path();
    let cfg = config::load(&config_path)?;
    let operator = config::resolve_operator(
        cli.operator.as_deref(),
        &cfg,
        &config_path.display().to_string(),
    )?;

    match cli.command {
        Command::Vtxo { outpoint } => {
            let outpoint = Outpoint::parse(&outpoint)?;
            let client = IndexerClient::new(&operator);
            match client.get_vtxo(&outpoint).await? {
                Some(vtxo) => {
                    if cli.json {
                        println!("{}", render::render_json(&vtxo)?);
                    } else {
                        let color = render::should_color(
                            Some(cli.color.as_str()),
                            std::env::var_os("NO_COLOR").is_some(),
                            std::io::stdout().is_terminal(),
                        );
                        println!("{}", render::render_human(&vtxo, &outpoint, color));
                    }
                    Ok(ExitCode::SUCCESS)
                }
                None => {
                    if cli.json {
                        println!("null");
                    } else {
                        println!("{}", render::render_not_found(&outpoint));
                    }
                    // Not found is a clean "no record" answer, not a failure.
                    Ok(ExitCode::from(2))
                }
            }
        }
    }
}
