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
    /// Decode script bytecode and list VTXOs for P2TR scripts.
    Script {
        /// Script hex (quote an empty string for empty bytecode).
        hex: String,
    },
    /// Inspect a single VTXO by outpoint (<txid>:<vout>).
    Vtxo {
        /// Outpoint in the form <txid>:<vout>.
        outpoint: String,
    },
    /// Inspect an operator-provided virtual transaction PSBT by txid.
    VirtualTx {
        /// Virtual transaction id (64 lowercase hex chars).
        txid: String,
    },
    /// Inspect a commitment transaction by txid.
    Tx {
        /// Commitment transaction id (64 hex chars).
        txid: String,
    },
}

fn main() -> ExitCode {
    if let Some(code) = arkade_cli::wallet::agent::internal_entry() {
        return code;
    }
    explorer_main()
}

#[tokio::main]
async fn explorer_main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn configured_operator(cli: &Cli) -> anyhow::Result<String> {
    let config_path = config::config_path();
    let cfg = config::load(&config_path)?;
    Ok(config::resolve_operator(
        cli.operator.as_deref(),
        &cfg,
        &config_path.display().to_string(),
    )?)
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let color = render::should_color(
        Some(cli.color.as_str()),
        std::env::var_os("NO_COLOR").is_some(),
        std::io::stdout().is_terminal(),
    );
    // Script validation and unsupported decoding must precede even config loading.
    if let Command::Script { hex } = &cli.command {
        return script_command(hex, cli.json, color, || {
            Ok(IndexerClient::new(&configured_operator(&cli)?, Duration::from_secs(cli.timeout))?)
        }, &mut std::io::stdout()).await;
    }
    let operator = configured_operator(&cli)?;
    match cli.command {
        Command::Script { .. } => unreachable!("script command handled before configuration"),
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
                println!("{}", render::render_address_human(&result, color));
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
        Command::VirtualTx { txid } => {
            let client = IndexerClient::new(&operator, Duration::from_secs(cli.timeout))?;
            virtual_tx_command(&client, &txid, cli.json, &mut std::io::stdout()).await
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

// The lazy client keeps local decoding independent of configuration and HTTP.
async fn script_command(
    raw: &str,
    json: bool,
    color: bool,
    client: impl FnOnce() -> anyhow::Result<IndexerClient>,
    stdout: &mut impl std::io::Write,
) -> anyhow::Result<ExitCode> {
    use arkade_cli::script::{ScriptResult, ScriptSummary};
    let script = ScriptSummary::decode(raw)?;
    let result = if script.p2tr_output_key.is_some() {
        client()?.get_script(&script.hex).await?
    } else {
        ScriptResult::unsupported(script)
    };
    let output = if json {
        render::render_json(&result)?
    } else {
        render::render_script_human(&result, color)
    };
    writeln!(stdout, "{output}")?;
    Ok(ExitCode::SUCCESS)
}

// Buffer the complete result before writing; this seam avoids global config/env in tests.
async fn virtual_tx_command(
    client: &IndexerClient,
    txid: &str,
    json: bool,
    stdout: &mut impl std::io::Write,
) -> anyhow::Result<ExitCode> {
    let result = client.get_virtual_tx(txid).await?;
    let code = if result.is_some() { ExitCode::SUCCESS } else { ExitCode::from(2) };
    let output = if json {
        render::render_json(&result)?
    } else {
        match result {
            Some(tx) => render::render_virtual_tx_human(&tx),
            None => format!("No virtual transaction {txid} found at the selected operator."),
        }
    };
    writeln!(stdout, "{output}")?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_tx_accepts_one_id_and_global_flags() {
        for args in [
            vec!["akcli", "--json", "--color", "always", "--operator", "http://localhost:7071", "--timeout", "5", "virtual-tx", "id"],
            vec!["akcli", "virtual-tx", "id", "--json", "--color", "always", "--operator", "http://localhost:7071", "--timeout", "5"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(cli.command, Command::VirtualTx { txid } if txid == "id"));
            assert!(cli.json);
            assert_eq!(cli.color, "always");
            assert_eq!(cli.timeout, 5);
            assert_eq!(cli.operator.as_deref(), Some("http://localhost:7071"));
        }
        assert!(Cli::try_parse_from(["akcli", "virtual-tx"]).is_err());
        assert!(Cli::try_parse_from(["akcli", "virtual-tx", "a", "b"]).is_err());
    }

    #[tokio::test]
    async fn virtual_tx_command_exit_codes_and_buffered_output() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::path;
        use base64::{engine::general_purpose::STANDARD, Engine};
        let tx = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn::default()],
            output: vec![],
        };
        let txid = tx.compute_txid().to_string();
        let psbt = STANDARD.encode(bitcoin::psbt::Psbt::from_unsigned_tx(tx).unwrap().serialize());
        for json in [false, true] {
            for (status, body, code) in [
                (200, serde_json::json!({"txs": [psbt]}), Some(ExitCode::SUCCESS)),
                (200, serde_json::json!({"txs": []}), Some(ExitCode::from(2))),
                (404, serde_json::json!({}), None),
                (200, serde_json::json!({"txs": [psbt, psbt]}), None),
                (200, serde_json::json!({"txs": ["!"]}), None),
            ] {
                let server = MockServer::start().await;
                Mock::given(path(format!("/v1/indexer/virtualTx/{txid}")))
                    .respond_with(ResponseTemplate::new(status).set_body_json(body))
                    .expect(1).mount(&server).await;
                let client = IndexerClient::new(&server.uri(), Duration::from_secs(5)).unwrap();
                let mut stdout = Vec::new();
                let result = virtual_tx_command(&client, &txid, json, &mut stdout).await;
                if let Some(code) = code {
                    assert_eq!(result.unwrap(), code);
                    let text = String::from_utf8(stdout).unwrap();
                    assert!(!text.contains('\x1b'));
                    if code == ExitCode::from(2) {
                        if json { assert_eq!(text, "null\n"); }
                        else { assert!(text.contains(&format!("No virtual transaction {txid}"))); }
                    } else if json {
                        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["psbt"], psbt);
                    } else {
                        assert!(text.contains("Virtual tx"));
                    }
                } else {
                    assert!(result.is_err()); // main maps errors to exit 1.
                    assert!(stdout.is_empty());
                }
            }
        }
    }

    #[tokio::test]
    async fn script_local_decoding_bypasses_configuration_and_buffers_errors() {
        for json in [false, true] {
            for raw in ["", "6A", "4c0101"] {
                let mut stdout = Vec::new();
                let code = script_command(raw, json, true,
                    || panic!("local script must not load even malformed configuration"),
                    &mut stdout).await.unwrap();
                assert_eq!(code, ExitCode::SUCCESS);
                let text = String::from_utf8(stdout).unwrap();
                assert!(!text.contains('\x1b'));
                if json {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(value["lookup_status"], "unsupported");
                    assert!(value["vtxos"].is_null());
                } else {
                    assert!(text.contains("unsupported"));
                    assert!(!text.contains("No VTXOs"));
                }
            }
            for raw in ["gg", "0", "4c02ff"] {
                let mut stdout = Vec::new();
                assert!(script_command(raw, json, true,
                    || panic!("malformed script must fail before configuration or HTTP"),
                    &mut stdout).await.is_err());
                assert!(stdout.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn script_p2tr_lookup_is_complete_anonymous_and_fail_closed() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{header_regex, method, path, query_param};
        let hex = format!("5120{}", "ab".repeat(32));
        let txid = "cd".repeat(32);
        let record = serde_json::json!({
            "outpoint": {"txid": txid, "vout": 0}, "amount": "5000000000",
            "isSpent": true, "spentBy": txid, "arkTxid": txid,
            "settledBy": txid, "commitmentTxids": [txid], "script": hex
        });
        for json in [false, true] {
            for scenario in ["empty", "multipage", "duplicate", "later_http", "missing_page", "stalled"] {
                let server = MockServer::start().await;
                let empty = scenario == "empty";
                let first = if empty {
                    serde_json::json!({"vtxos": [], "page": {"next": 0, "total": 0}})
                } else {
                    serde_json::json!({"vtxos": [record], "page": {"next": 7, "total": 9}})
                };
                Mock::given(method("GET")).and(path("/v1/indexer/vtxos"))
                    .and(query_param("scripts", &hex))
                    .and(query_param("page.index", "0"))
                    .and(query_param("page.size", "100"))
                    .and(header_regex("user-agent", "^akcli/"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(first))
                    .expect(1).mount(&server).await;
                if !empty {
                    let mut second_record = record.clone();
                    second_record["outpoint"]["vout"] = 1.into();
                    let (status, body) = match scenario {
                        "duplicate" => (200, serde_json::json!({"vtxos": [record], "page": {"next": 9, "total": 9}})),
                        "later_http" => (500, serde_json::json!({})),
                        "missing_page" => (200, serde_json::json!({})),
                        "stalled" => (200, serde_json::json!({"page": {"next": 7, "total": 9}})),
                        _ => (200, serde_json::json!({"vtxos": [second_record], "page": {"next": 9, "total": 9}})),
                    };
                    Mock::given(method("GET")).and(path("/v1/indexer/vtxos"))
                        .and(query_param("scripts", &hex))
                        .and(query_param("page.index", "7"))
                        .and(query_param("page.size", "100"))
                        .respond_with(ResponseTemplate::new(status).set_body_json(body))
                        .expect(1).mount(&server).await;
                }
                let mut stdout = Vec::new();
                let result = script_command(&hex.to_uppercase(), json, true,
                    || Ok(IndexerClient::new(&server.uri(), Duration::from_secs(5))?),
                    &mut stdout).await;
                if empty || scenario == "multipage" {
                    assert_eq!(result.unwrap(), ExitCode::SUCCESS);
                    let text = String::from_utf8(stdout).unwrap();
                    if json {
                        assert!(!text.contains('\x1b'));
                        assert!(!text.contains("\\u001b"));
                        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(value["lookup_status"], "complete");
                        assert_eq!(value["script"]["hex"], hex);
                        assert_eq!(value["script"]["p2tr_output_key"], "ab".repeat(32));
                        assert_eq!(value["vtxos"].as_array().unwrap().len(), if empty { 0 } else { 2 });
                        if !empty { assert_eq!(value["vtxos"][0], record); }
                    } else {
                        assert!(text.contains("Lookup complete"));
                        assert_eq!(text.contains("No VTXOs found"), empty);
                        assert!(!text.contains("unsupported"));
                    }
                } else {
                    assert!(result.is_err(), "{scenario}");
                    assert!(stdout.is_empty(), "no partial output: {scenario}");
                }
                let requests = server.received_requests().await.unwrap();
                assert_eq!(requests.len(), if empty { 1 } else { 2 });
                for request in requests {
                    assert_eq!(request.url.path(), "/v1/indexer/vtxos", "no info lookup");
                    assert_eq!(request.url.query_pairs().count(), 3, "no status or extra filters");
                    assert!(!request.headers.contains_key("authorization"));
                }
            }
        }
    }

    #[test]
    fn script_accepts_one_argument_and_global_flags() {
        for args in [
            vec!["akcli", "--json", "--color", "always", "--operator", "http://localhost:7071", "--timeout", "5", "script", "6A"],
            vec!["akcli", "script", "6A", "--json", "--color", "always", "--operator", "http://localhost:7071", "--timeout", "5"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(cli.json);
            assert_eq!(cli.color, "always");
            assert_eq!(cli.timeout, 5);
            assert_eq!(cli.operator.as_deref(), Some("http://localhost:7071"));
        }
        assert!(Cli::try_parse_from(["akcli", "script", ""]).is_ok());
        assert!(Cli::try_parse_from(["akcli", "script"]).is_err());
        assert!(Cli::try_parse_from(["akcli", "script", "a", "b"]).is_err());
        assert!(Cli::try_parse_from(["akcli", "script", "6a", "--decode-only"]).is_err());
    }

    #[test]
    fn address_accepts_one_argument_and_global_flags() {
        for args in [
            vec!["akcli", "--color", "always", "--json", "--operator", "http://localhost:7071", "--timeout", "5", "address", "tark1example"],
            vec!["akcli", "address", "tark1example", "--color", "always", "--json", "--operator", "http://localhost:7071", "--timeout", "5"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(cli.command, Command::Address { address } if address == "tark1example"));
            assert!(cli.json);
            assert_eq!(cli.timeout, 5);
            assert_eq!(cli.color, "always");
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
