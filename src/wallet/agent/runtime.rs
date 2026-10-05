use super::{protocol, Agent, Error, Status};
use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags};
use std::{
    os::{fd::OwnedFd, unix::ffi::OsStrExt},
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    net::{UnixListener, UnixStream},
    time::{sleep, timeout, Instant},
};

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const SOCKET: &str = "agent.sock";
const STARTUP: Duration = Duration::from_secs(5);
pub(super) const IO_DEADLINE: Duration = Duration::from_secs(2);

pub(super) struct Runtime {
    directory: OwnedFd,
    lock: OwnedFd,
    pub(super) socket: PathBuf,
    owned_socket: Option<(u64, u64)>,
}
fn check(s: &fs::Stat, kind: FileType, parent: bool) -> Result<(), Error> {
    let mode = if parent {
        s.st_mode & 0o022 == 0
    } else {
        s.st_mode & 0o7777
            == if kind == FileType::Directory {
                0o700
            } else {
                0o600
            }
    };
    if s.st_uid != rustix::process::geteuid().as_raw()
        || FileType::from_raw_mode(s.st_mode) != kind
        || !mode
    {
        return Err(Error::Runtime);
    }
    Ok(())
}
// Stat field widths differ between macOS and Linux targets.
#[allow(clippy::unnecessary_cast)]
fn identity(s: &fs::Stat) -> (u64, u64) {
    (s.st_dev as u64, s.st_ino as u64)
}
impl Runtime {
    pub(super) fn open(agent: &Agent) -> Result<Self, Error> {
        let socket = agent.home.join(".akcli/run").join(SOCKET);
        if socket.as_os_str().as_bytes().len() >= 104
            || agent
                .home
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::Runtime);
        }
        let mut directory =
            fs::open(&agent.home, DIRECTORY, Mode::empty()).map_err(|_| Error::Runtime)?;
        for (name, parent) in [(".akcli", true), ("run", false)] {
            match fs::mkdirat(&directory, name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => (),
                Err(_) => return Err(Error::Runtime),
            }
            directory = fs::openat(&directory, name, DIRECTORY, Mode::empty())
                .map_err(|_| Error::Runtime)?;
            check(
                &fs::fstat(&directory).map_err(|_| Error::Runtime)?,
                FileType::Directory,
                parent,
            )?;
        }
        let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        // Explicit exclusive publication avoids Darwin's concurrent O_CREAT /
        // O_NOFOLLOW open race; the existing-file path never creates or truncates.
        let lock = match fs::openat(
            &directory,
            "agent.lock",
            flags | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(lock) => lock,
            Err(rustix::io::Errno::EXIST) => {
                fs::openat(&directory, "agent.lock", flags, Mode::empty())
                    .map_err(|_| Error::Runtime)?
            }
            Err(_) => return Err(Error::Runtime),
        };
        let stat = fs::fstat(&lock).map_err(|_| Error::Runtime)?;
        check(&stat, FileType::RegularFile, false)?;
        if stat.st_nlink != 1 {
            return Err(Error::Runtime);
        }
        Ok(Self {
            directory,
            lock,
            socket,
            owned_socket: None,
        })
    }
    fn socket_stat(&self) -> Result<Option<fs::Stat>, Error> {
        match fs::statat(&self.directory, SOCKET, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(s) => {
                check(&s, FileType::Socket, false)?;
                Ok(Some(s))
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(_) => Err(Error::Runtime),
        }
    }
    pub(super) async fn connect(&self) -> Result<Option<UnixStream>, Error> {
        if self.socket_stat()?.is_none() {
            return Ok(None);
        }
        match timeout(IO_DEADLINE, UnixStream::connect(&self.socket)).await {
            Ok(Ok(stream)) => {
                protocol::authenticate(&stream)?;
                Ok(Some(stream))
            }
            Ok(Err(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                Ok(None)
            }
            Ok(Err(_)) => Err(Error::Runtime),
            Err(_) => Err(Error::Timeout),
        }
    }
    pub(super) async fn bind(&mut self) -> Result<UnixListener, Error> {
        fs::flock(&self.lock, FlockOperation::NonBlockingLockExclusive).map_err(|_| Error::Busy)?;
        // Even incompatible/unresponsive live listeners are preserved. Only a
        // definite absence/refusal, while holding the stable lock, allows cleanup.
        if self.connect().await?.is_some() {
            return Err(Error::Busy);
        }
        if let Some(stale) = self.socket_stat()? {
            let current = self.socket_stat()?.ok_or(Error::Runtime)?;
            if identity(&stale) != identity(&current) {
                return Err(Error::Runtime);
            }
            fs::unlinkat(&self.directory, SOCKET, AtFlags::empty()).map_err(|_| Error::Runtime)?;
        }
        // Directory creation is complete and no request tasks exist yet. Bind
        // with 0600 from the first instant (socket creation starts from 0777).
        let previous = rustix::process::umask(Mode::from_raw_mode(0o177));
        let listener = UnixListener::bind(&self.socket);
        rustix::process::umask(previous);
        let listener = listener.map_err(|_| Error::Runtime)?;
        let stat = fs::statat(&self.directory, SOCKET, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| Error::Runtime)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Socket
            || stat.st_uid != rustix::process::geteuid().as_raw()
        {
            return Err(Error::Runtime);
        }
        self.owned_socket = Some(identity(&stat));
        self.socket_stat()?.ok_or(Error::Runtime)?;
        Ok(listener)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        // The exclusive flock is still owned here. Never unlink the lock file.
        if let Some(owned) = self.owned_socket {
            if self
                .socket_stat()
                .ok()
                .flatten()
                .is_some_and(|s| identity(&s) == owned)
            {
                let _ = fs::unlinkat(&self.directory, SOCKET, AtFlags::empty());
            }
        }
    }
}

async fn probe(runtime: &Runtime) -> Result<bool, Error> {
    let Some(mut stream) = runtime.connect().await? else {
        return Ok(false);
    };
    timeout(IO_DEADLINE, async {
        protocol::write(&mut stream, &protocol::request(protocol::STATUS, None)?).await?;
        protocol::parse_response(&protocol::read(&mut stream).await?).map(|_| true)
    })
    .await
    .map_err(|_| Error::Timeout)?
}
// Cancellation of ensure_running must not abandon an unreaped startup child.
struct PendingChild(Option<std::process::Child>);
impl PendingChild {
    fn detach(mut self) {
        let mut child = self.0.take().unwrap();
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
impl Drop for PendingChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(super) async fn ensure(agent: &Agent) -> Result<(), Error> {
    let runtime = Runtime::open(agent)?;
    if probe(&runtime).await? {
        return Ok(());
    }
    let mut child = PendingChild(Some(
        Command::new(std::env::current_exe().map_err(|_| Error::Runtime)?)
            .args(["--wallet-agent-internal", "daemon"])
            .arg(&agent.home)
            .arg(agent.options.lease().as_secs().to_string())
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| Error::Runtime)?,
    ));
    let result = timeout(STARTUP, async {
        loop {
            if probe(&runtime).await? {
                return Ok(());
            }
            // A losing child may exit before the winner has bound its socket.
            // Reap it, but keep the same bounded readiness budget for the winner.
            let _ = child
                .0
                .as_mut()
                .unwrap()
                .try_wait()
                .map_err(|_| Error::Runtime)?;
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or(Err(Error::Timeout));
    if result.is_ok() {
        // Reap independently of the caller's Tokio runtime lifetime.
        child.detach();
    }
    // Failure/cancellation kills and reaps only our child. Any remaining stale
    // socket is left for the next exclusive lock holder, never blindly unlinked.
    result
}

pub(super) async fn call(
    agent: &Agent,
    op: u8,
    unlock: Option<(&crate::wallet::WalletName, &crate::wallet::Secret)>,
) -> Result<Status, Error> {
    let runtime = Runtime::open(agent)?;
    timeout(Duration::from_secs(30), async {
        let mut stream = runtime.connect().await?.ok_or(Error::Runtime)?;
        // Authentication preceded constructing/writing the secret frame.
        let bytes = protocol::request(op, unlock)?;
        protocol::write(&mut stream, &bytes).await?;
        drop(bytes);
        protocol::parse_response(&protocol::read(&mut stream).await?)
    })
    .await
    .map_err(|_| Error::Timeout)?
}

pub(super) async fn expire_at(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
