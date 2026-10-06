//! Binary-owned presentation for local wallet operations.
use arkade_cli::wallet::agent::{Agent, Error, Options, Status};
#[cfg(unix)]
use arkade_cli::wallet::{Secret, WalletName, WalletStore};
use clap::Subcommand;
use std::path::PathBuf;
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use zeroize::Zeroizing;
#[cfg(unix)]
mod terminal;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Generate and encrypt a new English mnemonic (12 words by default; interactive).
    Create {
        name: String,
        /// Number of words in the new English mnemonic.
        #[arg(long, value_enum, default_value = "12")]
        words: CreationWords,
    },
    /// Encrypt an existing English mnemonic without redisplaying it (interactive).
    Import { name: String },
    /// List public wallet names without decrypting.
    List,
    /// Unlock one wallet in the local agent (interactive).
    Unlock { name: String },
    /// Show whether the local agent is running and which wallet is unlocked.
    Status,
    /// Show a receive address for the currently unlocked wallet (reuses index 0).
    Receive,
    /// Forget the agent's retained mnemonic.
    Lock,
    /// Manage the local wallet agent.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum CreationWords {
    #[value(name = "12")]
    Twelve,
    #[value(name = "24")]
    TwentyFour,
}

#[cfg(unix)]
impl CreationWords {
    fn count(self) -> usize {
        match self {
            Self::Twelve => 12,
            Self::TwentyFour => 24,
        }
    }

    fn entropy_bytes(self) -> usize {
        match self {
            Self::Twelve => 16,
            Self::TwentyFour => 32,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Stop the local agent and forget its retained mnemonic.
    Stop,
}

#[cfg(unix)]
fn result(action: &str, name: &WalletName, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({ "result": action, "name": name.as_str() })
        );
    } else {
        println!("Wallet {}: {action}", name.as_str());
    }
}

#[cfg(unix)]
fn canonical(mnemonic: &bip39::Mnemonic) -> Secret {
    let mut text = Zeroizing::new(String::with_capacity(256));
    for word in mnemonic.words() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(word);
    }
    Secret::new(std::mem::take(&mut *text))
}

#[cfg(unix)]
async fn save(
    home: &std::path::Path,
    raw_name: &str,
    creation_words: Option<CreationWords>,
    json: bool,
) -> anyhow::Result<()> {
    let name = WalletName::new(raw_name)?;
    let mut tty = terminal::Terminal::open()?;
    tty.say("The file password encrypts this file; it is NOT a BIP39 passphrase.\nA mnemonic alone is not a guaranteed complete wallet recovery backup.\n")?;
    let create = creation_words.is_some();
    let (phrase, password) = if let Some(words) = creation_words {
        let password = tty.password(true).await?;
        tty.say("The encrypted wallet will be saved BEFORE its phrase is displayed.\nAfter publication, cancellation keeps the file even without backup acknowledgement.\nOnce saving starts, cancellation is deferred until saving finishes.\n")?;
        tty.confirm("Display a new phrase on this controlling terminal? Type yes: ")
            .await?;
        let mut entropy = Zeroizing::new(vec![0u8; words.entropy_bytes()]);
        getrandom::fill(&mut entropy)
            .map_err(|_| anyhow::anyhow!("cryptographic operation failed"))?;
        let mnemonic = bip39::Mnemonic::from_entropy_in(bip39::Language::English, &entropy)
            .map_err(|_| anyhow::anyhow!("cryptographic operation failed"))?;
        (canonical(&mnemonic), password)
    } else {
        let phrase = tty.read("English mnemonic (hidden): ", 512).await?;
        let mnemonic =
            bip39::Mnemonic::parse_in_normalized(bip39::Language::English, phrase.expose())
                .map_err(|_| anyhow::anyhow!("invalid English mnemonic"))?;
        (canonical(&mnemonic), tty.password(true).await?)
    };
    // W1 consumes its secret. Copy into a preallocated zeroizing owner only for
    // the intentional creation display; never format a phrase into ordinary text.
    let mut display = Zeroizing::new(String::with_capacity(if create {
        phrase.expose().len()
    } else {
        0
    }));
    if create {
        display.push_str(phrase.expose());
    }
    WalletStore::open(home)?
        .create(&name, phrase, password)
        .map_err(|error| {
            // W1 can report an fsync failure after atomic publication. Never remove
            // a possibly published file or display a phrase without confirmed save.
            anyhow::anyhow!("{error}; any published encrypted file is retained")
        })?;
    if let Some(words) = creation_words {
        let acknowledged = async {
            tty.say(&format!(
                "Encrypted wallet saved. Write down this {}-word phrase privately:\n",
                words.count()
            ))?;
            tty.say(&display)?;
            tty.say("\n")?;
            tty.confirm("Backup recorded? Type yes: ").await
        }
        .await;
        if acknowledged.is_err() {
            let _ = tty.say("Cancelled after publication: the encrypted wallet file is retained; backup was not acknowledged.\n");
            anyhow::bail!("encrypted wallet retained; backup not acknowledged");
        }
    }
    result(if create { "created" } else { "imported" }, &name, json);
    Ok(())
}

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("wallet commands require an absolute HOME"))
}

pub async fn run(command: &Command, json: bool) -> anyhow::Result<()> {
    let home = home()?;
    #[cfg(unix)]
    match command {
        Command::Create { name, words } => {
            return save(&home, name, Some(*words), json).await;
        }
        Command::Import { name } => {
            return save(&home, name, None, json).await;
        }
        Command::List => {
            let names = WalletStore::open(&home)?.list()?;
            let names: Vec<_> = names.iter().map(WalletName::as_str).collect();
            if json {
                println!("{}", serde_json::json!({ "names": names }));
            } else {
                println!("Wallet name");
                for name in names {
                    println!("{name}");
                }
            }
            return Ok(());
        }
        Command::Unlock { name } => {
            let name = WalletName::new(name)?;
            let mut tty = terminal::Terminal::open()?;
            let settings = arkade_cli::config::load_wallet_settings(&home)?;
            let agent = Agent::new(
                home,
                Options::new(Duration::from_secs(settings.idle_timeout_seconds))?,
            )?;
            tty.say("An existing agent retains its original idle timeout. Stop it before unlocking to apply configuration changes.\n")?;
            let password = tty.password(false).await?;
            agent.ensure_running().await?;
            agent.unlock(&name, password).await?;
            result("unlocked", &name, json);
            return Ok(());
        }
        _ => (),
    }
    let agent = Agent::new(home, Options::default())?;
    let (running, status) = match command {
        Command::Create { .. }
        | Command::Import { .. }
        | Command::List
        | Command::Unlock { .. } => {
            anyhow::bail!("interactive wallets are unsupported on this platform");
        }
        Command::Receive => {
            let address = agent.receive().await.map_err(|error| match error {
                Error::Protocol | Error::Timeout => anyhow::anyhow!(
                    "{error}; an older agent may not support receive. Run `akcli wallet agent stop`, then unlock again and retry"
                ),
                _ => error.into(),
            })?;
            if json {
                println!("{}", serde_json::json!({ "address": address }));
            } else {
                println!("Address: {address}");
            }
            return Ok(());
        }
        Command::Status => match agent.status().await {
            Ok(status) => (true, status),
            Err(Error::Absent) => (false, Status::Locked),
            Err(error) => return Err(error.into()),
        },
        Command::Lock => match agent.lock().await {
            Ok(()) => (true, Status::Locked),
            Err(Error::Absent) => (false, Status::Locked),
            Err(error) => return Err(error.into()),
        },
        Command::Agent {
            command: AgentCommand::Stop,
        } => {
            match agent.stop().await {
                Ok(()) | Err(Error::Absent) => (),
                Err(error) => return Err(error.into()),
            }
            (false, Status::Locked)
        }
    };
    let name = match &status {
        Status::Locked => None,
        Status::Unlocked(name) => Some(name.as_str()),
    };
    let agent_label = if running { "running" } else { "stopped" };
    let status_label = if name.is_some() { "unlocked" } else { "locked" };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "agent": agent_label, "status": status_label, "name": name,
            })
        );
    } else {
        println!("Agent: {agent_label}\nWallet: {status_label}");
        if let Some(name) = name {
            println!("Name: {name}");
        }
    }
    Ok(())
}
