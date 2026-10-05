//! Private, single-wallet memory agent. No operation returns mnemonic material.

use std::{path::PathBuf, time::Duration};

#[cfg(unix)]
mod protocol;
#[cfg(unix)]
mod runtime;
#[cfg(unix)]
mod server;
#[cfg(unix)]
mod state;

use super::{Secret, WalletName};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid agent options")]
    Options,
    #[error("unsafe or unavailable agent runtime")]
    Runtime,
    #[error("agent protocol or authentication failure")]
    Protocol,
    #[error("agent request timed out")]
    Timeout,
    #[error("agent is busy")]
    Busy,
    #[error("unable to unlock wallet")]
    Unlock,
    #[error("wallet agent is unsupported on this platform")]
    Unsupported,
}

#[derive(Clone, Copy)]
pub struct Options {
    lease: Duration,
}
impl Options {
    /// Whole seconds, from one second through one day. TOML integration is separate.
    pub fn new(lease: Duration) -> Result<Self, Error> {
        if lease.is_zero() || lease.as_secs() > 86_400 || lease.subsec_nanos() != 0 {
            return Err(Error::Options);
        }
        Ok(Self { lease })
    }
    pub fn lease(self) -> Duration {
        self.lease
    }
}
impl Default for Options {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Locked,
    Unlocked(WalletName),
}

/// Explicit home only: never resolves HOME, explorer config, or an operator URL.
#[derive(Clone)]
pub struct Agent {
    home: PathBuf,
    options: Options,
}
impl Agent {
    pub fn new(home: PathBuf, options: Options) -> Result<Self, Error> {
        if !cfg!(any(target_os = "linux", target_os = "macos")) {
            return Err(Error::Unsupported);
        }
        if !home.is_absolute() {
            return Err(Error::Options);
        }
        Ok(Self { home, options })
    }

    /// Launches the current executable in its hidden daemon mode if absent.
    /// Existing agents retain their original lease options.
    pub async fn ensure_running(&self) -> Result<(), Error> {
        #[cfg(unix)]
        {
            runtime::ensure(self).await
        }
        #[cfg(not(unix))]
        {
            Err(Error::Unsupported)
        }
    }
    pub async fn unlock(&self, name: &WalletName, password: Secret) -> Result<Status, Error> {
        #[cfg(unix)]
        {
            runtime::call(self, protocol::UNLOCK, Some((name, &password))).await
        }
        #[cfg(not(unix))]
        {
            let _ = (name, password);
            Err(Error::Unsupported)
        }
    }
    pub async fn status(&self) -> Result<Status, Error> {
        #[cfg(unix)]
        {
            runtime::call(self, protocol::STATUS, None).await
        }
        #[cfg(not(unix))]
        {
            Err(Error::Unsupported)
        }
    }
    pub async fn lock(&self) -> Result<(), Error> {
        #[cfg(unix)]
        {
            runtime::call(self, protocol::LOCK, None).await.map(|_| ())
        }
        #[cfg(not(unix))]
        {
            Err(Error::Unsupported)
        }
    }
    pub async fn stop(&self) -> Result<(), Error> {
        #[cfg(unix)]
        {
            runtime::call(self, protocol::STOP, None).await.map(|_| ())
        }
        #[cfg(not(unix))]
        {
            Err(Error::Unsupported)
        }
    }
}

/// Binary bootstrap, called synchronously before creating any runtime or loading
/// explorer configuration. This is not an interactive wallet CLI.
#[doc(hidden)]
pub fn internal_entry() -> Option<std::process::ExitCode> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--wallet-agent-internal")) {
        return None;
    }
    let result = (|| {
        let mode = args.next().ok_or(Error::Options)?;
        let home = PathBuf::from(args.next().ok_or(Error::Options)?);
        let seconds = args
            .next()
            .and_then(|s| s.to_str().and_then(|s| s.parse::<u64>().ok()))
            .ok_or(Error::Options)?;
        if args.next().is_some() {
            return Err(Error::Options);
        }
        let agent = Agent::new(home, Options::new(Duration::from_secs(seconds))?)?;
        #[cfg(unix)]
        {
            let daemon = mode == "daemon";
            if !daemon && mode != "ensure" {
                return Err(Error::Options);
            }
            if daemon {
                // Post-exec, single threaded, no unsafe pre_exec or fork hooks.
                rustix::process::setsid().map_err(|_| Error::Runtime)?;
                rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|_| Error::Runtime)?;
            runtime.block_on(async {
                if daemon {
                    server::serve(agent).await
                } else {
                    agent.ensure_running().await
                }
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (mode, agent);
            Err(Error::Unsupported)
        }
    })();
    if let Err(ref error) = result {
        eprintln!("{error}");
    }
    Some(if result.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}
