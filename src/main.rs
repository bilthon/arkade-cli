use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use arkade_cli::config;
use arkade_cli::indexer::IndexerClient;
use arkade_cli::outpoint::{self, Outpoint};
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

    /// Request timeout in seconds.
    #[arg(long, global = true, default_value_t = 10)]
    timeout: u64,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show operator information and policy.
    Info,
    /// List all indexer VTXOs linked to an Arkade address.
    Address {
        /// Arkade address (ark1... or tark1...).
        address: String,
    },
    /// Inspect a single VTXO by outpoint (<txid>:<vout>).
    Vtxo {
        /// Outpoint in the form <txid>:<vout>.
        outpoint: String,
    },
    /// Inspect a commitment transaction by txid.
    Tx {
        /// Commitment transaction id (64 hex chars).
        txid: String,
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
        Command::Info => {
            let client = IndexerClient::new(&operator, Duration::from_secs(cli.timeout))?;
            let info = client.get_info().await?;
            if cli.json {
                println!("{}", render::render_json(&info)?);
            } else {
                println!("{}", render::render_info_human(&info));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Address { address } => {
            let client = IndexerClient::new(&operator, Duration::from_secs(cli.timeout))?;
            let result = client.get_address(&address).await?;
            if cli.json {
                println!("{}", render::render_json(&result)?);
            } else {
                println!("{}", render::render_address_human(&result));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Vtxo { outpoint } => {
            let outpoint = Outpoint::parse(&outpoint)?;
            let client = IndexerClient::new(&operator, Duration::from_secs(cli.timeout))?;
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
        Command::Tx { txid } => {
            outpoint::validate_txid(&txid)?;
            let client = IndexerClient::new(&operator, Duration::from_secs(cli.timeout))?;
            match client.get_commitment_tx(&txid).await? {
                Some(resp) => {
                    if cli.json {
                        println!("{}", render::render_json(&resp)?);
                    } else {
                        println!("{}", render::render_commitment_human(&resp, &txid));
                    }
                    Ok(ExitCode::SUCCESS)
                }
                None => {
                    if cli.json {
                        println!("null");
                    } else {
                        println!("{}", render::render_commitment_not_found(&txid));
                    }
                    Ok(ExitCode::from(2))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_accepts_one_argument_and_global_flags() {
        for args in [
            vec!["akcli", "--json", "--operator", "http://localhost:7071", "--timeout", "5", "address", "tark1example"],
            vec!["akcli", "address", "tark1example", "--json", "--operator", "http://localhost:7071", "--timeout", "5"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(cli.command, Command::Address { address } if address == "tark1example"));
            assert!(cli.json);
            assert_eq!(cli.timeout, 5);
            assert_eq!(cli.operator.as_deref(), Some("http://localhost:7071"));
        }
        assert!(Cli::try_parse_from(["akcli", "address"]).is_err());
        assert!(Cli::try_parse_from(["akcli", "address", "a", "b"]).is_err());
    }

    #[test]
    fn info_accepts_global_flags_before_and_after_command() {
        for args in [
            vec!["akcli", "--operator", "http://localhost:7071", "--json", "--timeout", "5",
                "--color", "never", "info"],
            vec!["akcli", "info", "--operator", "http://localhost:7071", "--json",
                "--timeout", "5", "--color", "never"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(cli.command, Command::Info));
            assert_eq!(cli.operator.as_deref(), Some("http://localhost:7071"));
            assert!(cli.json);
            assert_eq!(cli.timeout, 5);
            assert_eq!(cli.color, "never");
        }
        assert!(Cli::try_parse_from(["akcli", "info"]).is_ok());
        assert!(Cli::try_parse_from(["akcli", "info", "unexpected"]).is_err());
    }
}
