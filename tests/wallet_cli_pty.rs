#![cfg(unix)]

use rustix::{
    fs::{Mode, OFlags},
    pty, termios,
};
use std::{
    io::Read,
    os::fd::OwnedFd,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const PHRASE: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const PASSWORD: &str = "synthetic-file-password";

// A fresh exec'd test process performs setsid safely, without a pre_exec hook in
// the multithreaded runner. Only public fixture paths/arguments cross its env.
#[test]
#[ignore]
fn terminal_child() {
    rustix::process::setsid().unwrap();
    let tty = std::env::var_os("AKCLI_TEST_PTY").map(|path| {
        let fd = rustix::fs::open(
            Path::new(&path),
            OFlags::RDWR | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        rustix::process::ioctl_tiocsctty(&fd).unwrap();
        let original = termios::tcgetattr(&fd).unwrap();
        (fd, original)
    });
    let args: Vec<String> =
        serde_json::from_str(&std::env::var("AKCLI_TEST_ARGS").unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_akcli"))
        .args(args)
        .env_clear()
        .env("HOME", std::env::var_os("HOME").unwrap())
        .spawn()
        .unwrap();
    if let Some((fd, _)) = &tty {
        rustix::io::write(fd, format!("AKCLI_TEST_PID:{}\n", child.id()).as_bytes()).unwrap();
    }
    let status = child.wait().unwrap();
    // Darwin invalidates the slave on session-leader exit. Inspect restoration
    // while this fresh leader is still alive, after the CLI has exited.
    if let Some((fd, original)) = &tty {
        let restored = termios::tcgetattr(fd).unwrap();
        assert_eq!(restored.local_modes, original.local_modes);
        assert_eq!(restored.input_modes, original.input_modes);
        assert_eq!(restored.output_modes, original.output_modes);
        for code in [
            termios::SpecialCodeIndex::VMIN,
            termios::SpecialCodeIndex::VTIME,
            termios::SpecialCodeIndex::VINTR,
            termios::SpecialCodeIndex::VERASE,
            termios::SpecialCodeIndex::VEOF,
        ] {
            assert_eq!(restored.special_codes[code], original.special_codes[code]);
        }
        eprintln!("AKCLI_TERMINAL_RESTORED");
    }
    std::process::exit(status.code().unwrap_or(99));
}

fn helper(home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .env("HOME", home)
        .env("AKCLI_TEST_ARGS", serde_json::to_string(args).unwrap())
        .args(["--exact", "terminal_child", "--ignored", "--nocapture"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("aw-")
        .tempdir_in("/tmp")
        .unwrap()
}
fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_akcli"))
        .env_clear()
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap()
}
fn status(home: &Path) -> serde_json::Value {
    let output = cli(home, &["wallet", "status", "--json"]);
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

struct Session {
    child: Child,
    master: OwnedFd,
    _slave: OwnedFd,
    transcript: Zeroizing<Vec<u8>>,
}
impl Session {
    fn new(home: &Path, args: &[&str]) -> Self {
        let master = pty::openpt(pty::OpenptFlags::RDWR | pty::OpenptFlags::NOCTTY).unwrap();
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC).unwrap();
        pty::grantpt(&master).unwrap();
        pty::unlockpt(&master).unwrap();
        let path = pty::ptsname(&master, Vec::new()).unwrap();
        let slave = rustix::fs::open(
            &*path,
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        rustix::fs::fcntl_setfl(&master, OFlags::RDWR | OFlags::NONBLOCK).unwrap();
        let child = helper(home, args)
            .env("AKCLI_TEST_PTY", path.to_str().unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            master,
            _slave: slave,
            transcript: Zeroizing::new(Vec::new()),
        }
    }
    fn drain(&mut self) {
        let mut bytes = Zeroizing::new([0; 1024]);
        loop {
            match rustix::io::read(&self.master, &mut *bytes) {
                Ok(0) | Err(rustix::io::Errno::AGAIN | rustix::io::Errno::IO) => break,
                Ok(n) => self.transcript.extend_from_slice(&bytes[..n]),
                Err(_) => panic!("PTY read failed"),
            }
        }
        assert!(self.transcript.len() < 16_384, "bounded terminal output");
    }
    fn until(&mut self, prompt: &str) {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            self.drain();
            if self
                .transcript
                .windows(prompt.len())
                .any(|w| w == prompt.as_bytes())
            {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "child exited before prompt (transcript suppressed)"
            );
            assert!(
                Instant::now() < end,
                "prompt timeout (transcript suppressed)"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn send(&self, bytes: &[u8]) {
        let mut sent = 0;
        let end = Instant::now() + Duration::from_secs(10);
        while sent < bytes.len() {
            match rustix::io::write(&self.master, &bytes[sent..]) {
                Ok(n) => sent += n,
                Err(rustix::io::Errno::AGAIN) => thread::sleep(Duration::from_millis(5)),
                Err(_) => panic!("PTY write failed"),
            }
            assert!(Instant::now() < end, "PTY write timeout");
        }
    }
    fn line(&self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\n");
    }
    fn password(&mut self) {
        self.until("File password: ");
        self.line(PASSWORD);
        self.until("Confirm file password: ");
        self.line(PASSWORD);
    }
    fn finish(&mut self, success: bool) -> (Vec<u8>, Vec<u8>) {
        let end = Instant::now() + Duration::from_secs(30);
        let status = loop {
            self.drain();
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < end, "child exit timeout");
            thread::sleep(Duration::from_millis(10));
        };
        self.child.wait().unwrap();
        assert_eq!(
            status.success(),
            success,
            "unexpected child outcome (transcript suppressed)"
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        self.child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .unwrap();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        assert!(
            String::from_utf8_lossy(&stderr).contains("AKCLI_TERMINAL_RESTORED"),
            "terminal restoration verified by session leader"
        );
        for bytes in [&stdout, &stderr] {
            assert!(
                !bytes
                    .windows(PASSWORD.len())
                    .any(|w| w == PASSWORD.as_bytes()),
                "password redacted"
            );
            assert!(
                !bytes.windows(PHRASE.len()).any(|w| w == PHRASE.as_bytes()),
                "mnemonic redacted"
            );
        }
        assert!(
            !self
                .transcript
                .windows(PASSWORD.len())
                .any(|w| w == PASSWORD.as_bytes()),
            "password not echoed"
        );
        (stdout, stderr)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
        }
        let _ = self.child.wait();
    }
}

fn import(home: &Path, name: &str) {
    let mut session = Session::new(home, &["wallet", "import", name, "--json"]);
    session.until("English mnemonic (hidden): ");
    session.line(PHRASE);
    session.password();
    session.finish(true);
    assert!(!session
        .transcript
        .windows(PHRASE.len())
        .any(|w| w == PHRASE.as_bytes()));
}

#[test]
fn hidden_import_and_duplicate_are_safe() {
    let home = home();
    import(home.path(), "first");
    let path = home.path().join(".akcli/wallets/first.json");
    let original = std::fs::read(&path).unwrap();
    assert!(!original
        .windows(PHRASE.len())
        .any(|w| w == PHRASE.as_bytes()));
    let mut session = Session::new(home.path(), &["wallet", "import", "first"]);
    session.until("English mnemonic (hidden): ");
    session.line(PHRASE);
    session.password();
    session.finish(false);
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert_eq!(status(home.path())["agent"], "stopped");
}

#[test]
fn create_publishes_before_display_and_retains_on_cancellation() {
    let home = home();
    let mut session = Session::new(home.path(), &["--json", "wallet", "create", "fresh"]);
    session.password();
    session.until("Type yes: ");
    assert!(!home.path().join(".akcli/wallets/fresh.json").exists());
    session.line("yes");
    session.until("Backup recorded? Type yes: ");
    assert!(home.path().join(".akcli/wallets/fresh.json").exists());
    let text = std::str::from_utf8(&session.transcript).unwrap();
    let phrase = text
        .lines()
        .find(|line| {
            line.split_whitespace().count() == 12
                && bip39::Mnemonic::parse_in_normalized(bip39::Language::English, line).is_ok()
        })
        .unwrap();
    assert!(bip39::Mnemonic::parse_in_normalized(bip39::Language::English, phrase).is_ok());
    let mut generated = Zeroizing::new(Vec::new());
    generated.extend_from_slice(phrase.as_bytes());
    session.line("no");
    let (stdout, stderr) = session.finish(false);
    for bytes in [&stdout, &stderr] {
        assert!(!bytes
            .windows(generated.len())
            .any(|w| w == generated.as_slice()));
    }
    assert!(home.path().join(".akcli/wallets/fresh.json").exists());
    assert!(String::from_utf8_lossy(&stderr).contains("retained"));
}

#[test]
fn creation_word_choices_have_english_checksums_and_private_display() {
    for (args, count) in [
        (vec!["wallet", "create", "fresh"], 12),
        (vec!["wallet", "create", "--words", "12", "fresh", "--json"], 12),
        (vec!["--json", "wallet", "create", "fresh", "--words", "24"], 24),
    ] {
        let home = home();
        let mut session = Session::new(home.path(), &args);
        session.password();
        session.until("Type yes: ");
        session.line("yes");
        session.until("Backup recorded? Type yes: ");
        let text = std::str::from_utf8(&session.transcript).unwrap();
        assert!(text.contains(&format!("Write down this {count}-word phrase privately:")));
        let phrase = text.lines().find(|line| {
            line.split_whitespace().count() == count
                && bip39::Mnemonic::parse_in_normalized(bip39::Language::English, line).is_ok()
        }).expect("valid English phrase of requested length (transcript suppressed)");
        let generated = Zeroizing::new(phrase.as_bytes().to_vec());
        let encrypted = std::fs::read(home.path().join(".akcli/wallets/fresh.json")).unwrap();
        assert!(!encrypted.windows(generated.len()).any(|w| w == generated.as_slice()));
        session.line("yes");
        let (stdout, stderr) = session.finish(true);
        for bytes in [&stdout, &stderr] {
            assert!(!bytes.windows(generated.len()).any(|w| w == generated.as_slice()));
        }
        assert_eq!(status(home.path())["agent"], "stopped");
    }
}

#[test]
fn create_acknowledgement_and_prepublication_cancellation() {
    let home = home();
    for (name, permission) in [("cancelled", "no"), ("saved", "yes")] {
        let mut session = Session::new(home.path(), &["wallet", "create", name, "--json"]);
        session.password();
        session.until("Type yes: ");
        session.line(permission);
        if permission == "yes" {
            session.until("Backup recorded? Type yes: ");
            session.line("yes");
        }
        session.finish(permission == "yes");
        assert_eq!(
            home.path()
                .join(format!(".akcli/wallets/{name}.json"))
                .exists(),
            permission == "yes"
        );
    }
}

#[test]
fn invalid_mnemonic_mismatch_and_input_cancellation_restore_terminal() {
    let home = home();
    let mut invalid = Session::new(home.path(), &["wallet", "import", "invalid"]);
    invalid.until("English mnemonic (hidden): ");
    invalid.line("not a mnemonic");
    invalid.finish(false);
    let mut mismatch = Session::new(home.path(), &["wallet", "create", "mismatch"]);
    mismatch.until("File password: ");
    mismatch.line(PASSWORD);
    mismatch.until("Confirm file password: ");
    mismatch.line("different");
    mismatch.finish(false);
    for bytes in [vec![3], vec![4], vec![0xff, b'\n'], vec![b'a'; 4097]] {
        let mut session = Session::new(home.path(), &["wallet", "create", "cancelled"]);
        session.until("File password: ");
        session.send(&bytes);
        session.finish(false);
    }
    assert!(!home.path().join(".akcli").exists());
}

#[test]
fn valid_english_lengths_and_bounded_utf8_password_editing() {
    let home = home();
    for (index, size) in [16, 20, 24, 28, 32].into_iter().enumerate() {
        let mnemonic =
            bip39::Mnemonic::from_entropy_in(bip39::Language::English, &vec![0u8; size]).unwrap();
        let mut phrase = Zeroizing::new(String::with_capacity(256));
        for word in mnemonic.words() {
            if !phrase.is_empty() {
                phrase.push(' ');
            }
            phrase.push_str(word);
        }
        let mut session = Session::new(
            home.path(),
            &["wallet", "import", &format!("length-{index}")],
        );
        session.until("English mnemonic (hidden): ");
        session.line(&phrase);
        session.until("File password: ");
        if index == 0 {
            // Remove one multi-byte character; the entire removed byte sequence
            // must be erased, not left as invalid UTF-8 or included in the file.
            session.send("é".as_bytes());
            session.send(&[127]);
            session.line(PASSWORD);
            session.until("Confirm file password: ");
            session.line(PASSWORD);
        } else if index == 1 {
            let password = Zeroizing::new("é".repeat(2048));
            session.line(&password);
            session.until("Confirm file password: ");
            session.line(&password);
        } else {
            session.line(PASSWORD);
            session.until("Confirm file password: ");
            session.line(PASSWORD);
        }
        session.finish(true);
    }
}

#[test]
fn mnemonic_overflow_and_empty_password_do_not_publish() {
    let home = home();
    let mut session = Session::new(home.path(), &["wallet", "import", "overflow"]);
    session.until("English mnemonic (hidden): ");
    session.send(&[b'a'; 513]);
    session.finish(false);
    let mut session = Session::new(home.path(), &["wallet", "create", "empty"]);
    session.until("File password: ");
    session.line("");
    session.finish(false);
    assert!(!home.path().join(".akcli").exists());
}

#[test]
fn externally_delivered_sigint_restores_echo_and_does_not_publish() {
    let home = home();
    let mut session = Session::new(home.path(), &["wallet", "create", "interrupted"]);
    session.until("File password: ");
    session.send(b"partial-secret");
    let pid = std::str::from_utf8(&session.transcript)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("AKCLI_TEST_PID:"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::INT,
    )
    .unwrap();
    session.finish(false);
    assert!(!home.path().join(".akcli").exists());
}

#[test]
fn no_controlling_terminal_rejects_before_storage_or_agent_start() {
    let home = home();
    for operation in ["create", "import", "unlock"] {
        let output = helper(home.path(), &["wallet", operation, "test"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("controlling terminal"));
        assert!(!home.path().join(".akcli").exists());
    }
}

struct Daemon {
    child: Child,
    home: std::path::PathBuf,
}
impl Daemon {
    fn start(home: &Path, lease: u64) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_akcli"))
            .env_clear()
            .args(["--wallet-agent-internal", "daemon"])
            .arg(home)
            .arg(lease.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut daemon = Self {
            child,
            home: home.to_owned(),
        };
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if status(home)["agent"] == "running" {
                return daemon;
            }
            assert!(daemon.child.try_wait().unwrap().is_none());
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = cli(&self.home, &["wallet", "agent", "stop"]);
        let end = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < end {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn unlock(home: &Path, name: &str, password: &str, success: bool) {
    let mut session = Session::new(home, &["wallet", "unlock", name, "--json"]);
    session.until("File password: ");
    session.line(password);
    session.finish(success);
}

// Autostart detaches the daemon from the CLI, so this fixture cannot waitpid it.
// Own its unique temporary home instead: request authenticated shutdown, then
// require socket removal AND release of its lifetime lock. Never find/kill a
// process by a substring or a possibly recycled PID.
struct AutostartCleanup {
    home: std::path::PathBuf,
    finished: bool,
}
impl AutostartCleanup {
    fn new(home: &Path) -> Self {
        Self {
            home: home.to_owned(),
            finished: false,
        }
    }

    fn stop_and_wait(&mut self) -> bool {
        use arkade_cli::wallet::agent::{Agent, Error, Options};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let agent = Agent::new(self.home.clone(), Options::default()).unwrap();
        self.finished = runtime.block_on(async {
            // An assertion may unwind while ensure_running is still starting a
            // child. Keep sending stop beyond W2's five-second startup budget,
            // rather than accepting an initially absent socket as final cleanup.
            let settle = tokio::time::Instant::now() + Duration::from_secs(6);
            tokio::time::timeout(Duration::from_secs(12), async {
                loop {
                    match agent.stop().await {
                        Ok(()) | Err(Error::Absent) => (),
                        Err(_) => {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            continue;
                        }
                    }
                    let socket_absent =
                        match std::fs::symlink_metadata(self.home.join(".akcli/run/agent.sock")) {
                            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
                            Ok(_) => false,
                        };
                    let lock_released = match rustix::fs::open(
                        self.home.join(".akcli/run/agent.lock"),
                        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    ) {
                        Ok(lock) => rustix::fs::flock(
                            &lock,
                            rustix::fs::FlockOperation::NonBlockingLockExclusive,
                        )
                        .is_ok(),
                        Err(rustix::io::Errno::NOENT) => true,
                        Err(_) => false,
                    };
                    if tokio::time::Instant::now() >= settle && socket_absent && lock_released {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .is_ok()
        });
        self.finished
    }
}
impl Drop for AutostartCleanup {
    fn drop(&mut self) {
        if !self.finished && !self.stop_and_wait() {
            eprintln!("autostart fixture cleanup did not confirm socket removal and lock release");
        }
    }
}

#[test]
fn interactive_unlock_autostarts_and_retains_until_separate_lock_and_stop() {
    let home = home();
    import(home.path(), "autostart");
    assert_eq!(status(home.path())["agent"], "stopped");
    assert!(!home.path().join(".akcli/run").exists());

    // Declared before the session so unwinding kills/waits the CLI session
    // first, then stops any daemon it managed to detach, before removing HOME.
    let mut cleanup = AutostartCleanup::new(home.path());
    let mut session = Session::new(home.path(), &["wallet", "unlock", "autostart", "--json"]);
    session.until("File password: ");
    session.line(PASSWORD);
    session.finish(true);
    drop(session);

    let retained = status(home.path());
    assert_eq!(retained["agent"], "running");
    assert_eq!(retained["status"], "unlocked");
    assert_eq!(retained["name"], "autostart");
    assert!(cli(home.path(), &["wallet", "lock"]).status.success());
    let locked = status(home.path());
    assert_eq!(locked["agent"], "running");
    assert_eq!(locked["status"], "locked");
    assert!(cli(home.path(), &["wallet", "agent", "stop"])
        .status
        .success());
    assert!(
        cleanup.stop_and_wait(),
        "autostart daemon shutdown must complete within budget"
    );
    assert_eq!(status(home.path())["agent"], "stopped");
}

#[test]
fn failed_unlock_does_not_renew_and_overflow_does_not_start_agent() {
    let home = home();
    import(home.path(), "first");
    {
        let _daemon = Daemon::start(home.path(), 1);
        unlock(home.path(), "first", PASSWORD, true);
        thread::sleep(Duration::from_millis(600));
        unlock(home.path(), "first", "incorrect", false);
        thread::sleep(Duration::from_millis(500));
        assert_eq!(status(home.path())["status"], "locked");
    }
    let mut session = Session::new(home.path(), &["wallet", "unlock", "first"]);
    session.until("File password: ");
    session.send(&[b'x'; 4097]);
    session.finish(false);
    assert_eq!(status(home.path())["agent"], "stopped");
    std::fs::write(
        home.path().join(".akcli/config.toml"),
        "[wallet]\nidle_timeout_seconds = 'synthetic-invalid-value'",
    )
    .unwrap();
    let mut session = Session::new(home.path(), &["wallet", "unlock", "first"]);
    let (_, stderr) = session.finish(false);
    assert!(!String::from_utf8_lossy(&stderr).contains("synthetic-invalid-value"));
    assert_eq!(status(home.path())["agent"], "stopped");
}

#[test]
fn independent_calls_replace_lock_stop_and_retain_original_configuration() {
    let home = home();
    import(home.path(), "first");
    import(home.path(), "second");
    let daemon = Daemon::start(home.path(), 30);
    std::fs::write(
        home.path().join(".akcli/config.toml"),
        "operator = 42\n[wallet]\nidle_timeout_seconds = 1",
    )
    .unwrap();
    unlock(home.path(), "first", PASSWORD, true);
    assert_eq!(status(home.path())["name"], "first");
    unlock(home.path(), "second", "incorrect", false);
    assert_eq!(status(home.path())["name"], "first");
    unlock(home.path(), "second", PASSWORD, true);
    thread::sleep(Duration::from_millis(1200));
    assert_eq!(status(home.path())["name"], "second");
    assert!(cli(home.path(), &["wallet", "lock"]).status.success());
    assert_eq!(status(home.path())["status"], "locked");
    drop(daemon);
    assert_eq!(status(home.path())["agent"], "stopped");
    let _restarted = Daemon::start(home.path(), 1);
    assert_eq!(status(home.path())["status"], "locked");
    unlock(home.path(), "first", PASSWORD, true);
    let end = Instant::now() + Duration::from_secs(3);
    while status(home.path())["status"] != "locked" {
        assert!(Instant::now() < end, "status must not renew lease");
        thread::sleep(Duration::from_millis(100));
    }
}
