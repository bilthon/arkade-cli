//! Bounded, zeroizing input from the controlling terminal, never standard input.
use arkade_cli::wallet::Secret;
use rustix::{
    fs::{Mode, OFlags},
    termios::{self, LocalModes, OptionalActions, QueueSelector, SpecialCodeIndex, Termios},
};
use std::{io::Write, os::fd::OwnedFd, time::Duration};
use tokio::signal::unix::{signal, Signal, SignalKind};
use zeroize::Zeroizing;

pub struct Terminal {
    fd: OwnedFd,
    original: Termios,
    interrupt: Signal,
}

fn unavailable() -> anyhow::Error {
    anyhow::anyhow!("controlling terminal unavailable")
}
fn cancelled() -> anyhow::Error {
    anyhow::anyhow!("terminal input cancelled or invalid")
}

impl Terminal {
    pub fn open() -> anyhow::Result<Self> {
        let fd = rustix::fs::open(
            "/dev/tty",
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|_| unavailable())?;
        let original = termios::tcgetattr(&fd).map_err(|_| unavailable())?;
        // Register before disabling echo. No homemade handler; externally sent
        // SIGINT is delivered through Tokio and unwinds the terminal guard.
        let interrupt = signal(SignalKind::interrupt()).map_err(|_| unavailable())?;
        let terminal = Self {
            fd,
            original,
            interrupt,
        };
        let mut hidden = terminal.original.clone();
        hidden.local_modes.remove(
            LocalModes::ECHO
                | LocalModes::ECHONL
                | LocalModes::ICANON
                | LocalModes::ISIG
                | LocalModes::IEXTEN,
        );
        hidden.special_codes[SpecialCodeIndex::VMIN] = 1;
        hidden.special_codes[SpecialCodeIndex::VTIME] = 0;
        termios::tcsetattr(&terminal.fd, OptionalActions::Flush, &hidden)
            .map_err(|_| unavailable())?;
        Ok(terminal)
    }

    pub fn say(&self, text: &str) -> anyhow::Result<()> {
        // Borrowing the descriptor does not route output through stdout/stderr.
        let mut file = std::fs::File::from(rustix::io::dup(&self.fd).map_err(|_| unavailable())?);
        file.write_all(text.as_bytes()).map_err(|_| unavailable())?;
        file.flush().map_err(|_| unavailable())
    }

    pub async fn read(&mut self, prompt: &str, limit: usize) -> anyhow::Result<Secret> {
        self.say(prompt)?;
        // Fixed initialized allocation: no reallocations or unzeroized popped
        // bytes, including backspace and partial/invalid UTF-8 cancellation.
        let mut bytes = Zeroizing::new(vec![0u8; limit]);
        let mut used = 0;
        let mut byte = Zeroizing::new([0u8; 1]);
        loop {
            tokio::select! {
                biased;
                _ = self.interrupt.recv() => return Err(cancelled()),
                _ = tokio::task::yield_now() => (),
            }
            match rustix::io::read(&self.fd, &mut *byte) {
                Ok(0) => return Err(cancelled()),
                Ok(_) => match byte[0] {
                    3 | 4 => return Err(cancelled()),
                    b'\n' | b'\r' => {
                        self.say("\n")?;
                        let text = std::str::from_utf8(&bytes[..used]).map_err(|_| cancelled())?;
                        let mut owned = Zeroizing::new(String::with_capacity(used));
                        owned.push_str(text);
                        // Transfer the existing allocation directly into Secret;
                        // no ordinary String clone or formatting of secret text.
                        return Ok(Secret::new(std::mem::take(&mut *owned)));
                    }
                    8 | 127 => {
                        if used > 0 {
                            used -= 1;
                            while used > 0 && bytes[used] & 0xc0 == 0x80 {
                                bytes[used] = 0;
                                used -= 1;
                            }
                            bytes[used] = 0;
                        }
                    }
                    b if b < 32 => return Err(cancelled()),
                    b => {
                        if used == limit {
                            return Err(cancelled());
                        }
                        bytes[used] = b;
                        used += 1;
                    }
                },
                Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
                    tokio::select! {
                        _ = self.interrupt.recv() => return Err(cancelled()),
                        _ = tokio::time::sleep(Duration::from_millis(5)) => (),
                    }
                }
                Err(_) => return Err(cancelled()),
            }
        }
    }

    pub async fn confirm(&mut self, prompt: &str) -> anyhow::Result<()> {
        if self.read(prompt, 16).await?.expose() != "yes" {
            return Err(cancelled());
        }
        Ok(())
    }

    pub async fn password(&mut self, confirm: bool) -> anyhow::Result<Secret> {
        let password = self.read("File password: ", 4096).await?;
        anyhow::ensure!(!password.expose().is_empty(), "password must not be empty");
        if confirm {
            let repeated = self.read("Confirm file password: ", 4096).await?;
            anyhow::ensure!(
                password.expose() == repeated.expose(),
                "password confirmation does not match"
            );
        }
        Ok(password)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Flush unread secret bytes before restoring echo, including errors and
        // SIGINT. SIGKILL, process abort and machine failure cannot run this guard.
        let _ = termios::tcflush(&self.fd, QueueSelector::IFlush);
        let _ = termios::tcsetattr(&self.fd, OptionalActions::Flush, &self.original);
    }
}
