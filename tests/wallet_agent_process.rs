#![cfg(unix)]

use arkade_cli::wallet::agent::{Agent, Options, Status};
use std::{
    io::{Read, Write},
    os::unix::{
        fs::{symlink, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Home(tempfile::TempDir);
impl Home {
    fn new() -> Self {
        Self(tempfile::tempdir_in("/tmp").unwrap())
    }
    fn path(&self) -> PathBuf {
        self.0.path().canonicalize().unwrap()
    }
    fn socket(&self) -> PathBuf {
        self.path().join(".akcli/run/agent.sock")
    }
    fn agent(&self) -> Agent {
        Agent::new(self.path(), Options::default()).unwrap()
    }
    fn runtime_dir(&self) {
        std::fs::create_dir(self.path().join(".akcli")).unwrap();
        std::fs::set_permissions(
            self.path().join(".akcli"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::create_dir(self.path().join(".akcli/run")).unwrap();
        std::fs::set_permissions(
            self.path().join(".akcli/run"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        if let Ok(mut stream) = UnixStream::connect(self.socket()) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = stream.write_all(&[0, 0, 0, 2, 1, 4]);
            let mut ack = [0; 6];
            let _ = stream.read_exact(&mut ack);
        }
    }
}
fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_akcli"));
    command
        .env_clear()
        .args(["--wallet-agent-internal", "ensure"])
        .arg(home)
        .arg("600")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}
fn launch(home: &Home) -> std::process::Output {
    command(&home.path()).output().unwrap()
}
async fn gone(home: &Home) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while home.socket().exists() {
        assert!(Instant::now() < deadline, "agent did not clean its socket");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn launcher_exits_and_detached_agent_stays_locked_then_stops_and_restarts() {
    let home = Home::new();
    let agent = home.agent();
    let output = launch(&home);
    assert!(output.status.success(), "{output:?}");
    // Captured pipes reached EOF while daemon still lives: stdio was not inherited.
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    assert_eq!(agent.status().await.unwrap(), Status::Locked);
    let name = arkade_cli::wallet::WalletName::new("alice").unwrap();
    arkade_cli::wallet::WalletStore::open(&home.path()).unwrap().create(
        &name,
        arkade_cli::wallet::Secret::new("abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about".into()),
        arkade_cli::wallet::Secret::new("synthetic password".into()),
    ).unwrap();
    assert_eq!(
        agent
            .unlock(
                &name,
                arkade_cli::wallet::Secret::new("synthetic password".into())
            )
            .await
            .unwrap(),
        Status::Unlocked(name)
    );
    let lock = home.path().join(".akcli/run/agent.lock");
    let inode = std::fs::metadata(&lock).unwrap().ino();
    agent.stop().await.unwrap();
    gone(&home).await;
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), inode);
    assert!(launch(&home).status.success());
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), inode);
    assert_eq!(agent.status().await.unwrap(), Status::Locked);
    agent.stop().await.unwrap();
    gone(&home).await;
}

#[tokio::test]
async fn concurrent_starters_converge_and_reuse_the_same_socket() {
    let home = Home::new();
    let children: Vec<_> = (0..6)
        .map(|_| command(&home.path()).spawn().unwrap())
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let inode = std::fs::metadata(home.socket()).unwrap().ino();
    assert!(launch(&home).status.success());
    assert_eq!(std::fs::metadata(home.socket()).unwrap().ino(), inode);
    assert_eq!(home.agent().status().await.unwrap(), Status::Locked);
}

#[tokio::test]
async fn safe_parent_mode_stale_socket_recovery_and_stable_lock() {
    let home = Home::new();
    home.runtime_dir();
    let stale = UnixListener::bind(home.socket()).unwrap();
    std::fs::set_permissions(home.socket(), std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(stale);
    assert!(launch(&home).status.success());
    assert_eq!(home.agent().status().await.unwrap(), Status::Locked);
    let lock = home.path().join(".akcli/run/agent.lock");
    assert!(std::fs::metadata(&lock).unwrap().is_file());
    home.agent().stop().await.unwrap();
    gone(&home).await;
    assert!(lock.exists());
}

#[test]
fn symlinks_wrong_types_and_unsafe_modes_fail_closed() {
    for target in [
        ".akcli",
        ".akcli/run",
        ".akcli/run/agent.lock",
        ".akcli/run/agent.sock",
    ] {
        let home = Home::new();
        if target != ".akcli" {
            std::fs::create_dir(home.path().join(".akcli")).unwrap();
        }
        if target.starts_with(".akcli/run/") {
            std::fs::create_dir(home.path().join(".akcli/run")).unwrap();
            std::fs::set_permissions(
                home.path().join(".akcli/run"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let destination = home.path().join("synthetic-target");
        std::fs::write(&destination, b"unchanged").unwrap();
        symlink(&destination, home.path().join(target)).unwrap();
        assert!(!launch(&home).status.success(), "accepted symlink {target}");
        assert_eq!(std::fs::read(destination).unwrap(), b"unchanged");
        assert!(std::fs::symlink_metadata(home.path().join(target))
            .unwrap()
            .file_type()
            .is_symlink());
    }
    for target in ["agent.sock", "agent.lock"] {
        let home = Home::new();
        home.runtime_dir();
        let path = home.path().join(".akcli/run").join(target);
        // Nonblocking nofollow open must reject a FIFO, not wait for a writer.
        assert!(Command::new("/usr/bin/mkfifo")
            .env_clear()
            .args(["-m", "600"])
            .arg(&path)
            .status()
            .unwrap()
            .success());
        let start = Instant::now();
        assert!(!launch(&home).status.success());
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(std::fs::symlink_metadata(path).is_ok());
    }
    for target in [".akcli", ".akcli/run"] {
        let home = Home::new();
        home.runtime_dir();
        std::fs::set_permissions(
            home.path().join(target),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(!launch(&home).status.success());
    }
    let home = Home::new();
    home.runtime_dir();
    std::fs::write(home.socket(), b"do not replace").unwrap();
    std::fs::set_permissions(home.socket(), std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!launch(&home).status.success());
    assert_eq!(std::fs::read(home.socket()).unwrap(), b"do not replace");
}

#[test]
fn too_long_and_symlink_home_are_rejected() {
    let home = Home::new();
    let long = home.path().join("x".repeat(100));
    std::fs::create_dir(&long).unwrap();
    assert!(!command(&long).output().unwrap().status.success());
    assert!(!long.join(".akcli").exists());
    let alias = home.path().join("alias");
    symlink(home.path(), &alias).unwrap();
    assert!(!command(&alias).output().unwrap().status.success());
}

#[test]
fn occupied_incompatible_and_unresponsive_sockets_are_preserved() {
    for respond in [true, false] {
        let home = Home::new();
        home.runtime_dir();
        let listener = UnixListener::bind(home.socket()).unwrap();
        std::fs::set_permissions(home.socket(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let inode = std::fs::metadata(home.socket()).unwrap().ino();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 6];
            stream.read_exact(&mut request).unwrap();
            if respond {
                stream.write_all(&[0, 0, 0, 2, 99, 0]).unwrap();
            } else {
                std::thread::sleep(Duration::from_millis(2300));
            }
        });
        let start = Instant::now();
        assert!(!launch(&home).status.success());
        assert!(start.elapsed() < Duration::from_secs(4));
        assert_eq!(std::fs::metadata(home.socket()).unwrap().ino(), inode);
        peer.join().unwrap();
    }
}

#[tokio::test]
async fn synthetic_poisoned_environment_and_invalid_operator_config_are_ignored() {
    let home = Home::new();
    home.runtime_dir();
    std::fs::write(home.path().join(".akcli/config.toml"), b"not valid TOML [").unwrap();
    let fake_home = home.path().join("unused-home");
    std::fs::create_dir(&fake_home).unwrap();
    let output = command(&home.path())
        .env("HOME", &fake_home)
        .env("AKCLI_OPERATOR", "invalid://synthetic")
        .env("AGENT_TEST_SENTINEL", "synthetic-not-a-secret")
        .current_dir(&fake_home)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!fake_home.join(".akcli").exists());
    assert_eq!(home.agent().status().await.unwrap(), Status::Locked);
    assert_eq!(
        std::fs::read(home.path().join(".akcli/config.toml")).unwrap(),
        b"not valid TOML ["
    );
}

#[test]
fn held_startup_lock_has_bounded_readiness_and_is_never_unlinked() {
    let home = Home::new();
    home.runtime_dir();
    let path = home.path().join(".akcli/run/agent.lock");
    let lock = rustix::fs::open(
        &path,
        rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL | rustix::fs::OFlags::RDWR,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    let start = Instant::now();
    assert!(!launch(&home).status.success());
    assert!(start.elapsed() < Duration::from_secs(7));
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    assert!(!home.socket().exists());
}

#[test]
fn unsafe_regular_lock_and_socket_permissions_are_preserved() {
    for socket in [false, true] {
        let home = Home::new();
        home.runtime_dir();
        let path = if socket {
            home.socket()
        } else {
            home.path().join(".akcli/run/agent.lock")
        };
        let _listener = if socket {
            Some(UnixListener::bind(&path).unwrap())
        } else {
            std::fs::write(&path, b"stable").unwrap();
            None
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        assert!(!launch(&home).status.success());
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o666
        );
    }
}

#[tokio::test]
async fn shutdown_does_not_unlink_a_replacement_socket() {
    let home = Home::new();
    assert!(launch(&home).status.success());
    let mut connected = UnixStream::connect(home.socket()).unwrap();
    connected
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let moved = home.path().join(".akcli/run/old.sock");
    std::fs::rename(home.socket(), &moved).unwrap();
    let replacement = UnixListener::bind(home.socket()).unwrap();
    std::fs::set_permissions(home.socket(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let inode = std::fs::metadata(home.socket()).unwrap().ino();
    connected.write_all(&[0, 0, 0, 2, 1, 4]).unwrap();
    let mut reply = [0; 6];
    connected.read_exact(&mut reply).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(std::fs::metadata(home.socket()).unwrap().ino(), inode);
    drop(replacement);
}
