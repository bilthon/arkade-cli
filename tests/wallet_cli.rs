use std::process::{Command, Output};

fn cli(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_akcli"))
        .env_clear()
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn public_commands_ignore_unrelated_configuration_and_preserve_local_script() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".akcli")).unwrap();
    std::fs::write(
        home.path().join(".akcli/config.toml"),
        "invalid TOML [[[ secret-marker",
    )
    .unwrap();
    for args in [
        vec!["wallet", "list", "--json"],
        vec!["--json", "wallet", "status"],
        vec!["wallet", "lock"],
        vec!["wallet", "agent", "stop"],
    ] {
        assert!(cli(home.path(), &args).status.success());
    }
    let output = cli(home.path(), &["script", "", "--json"]);
    assert!(output.status.success());
    assert!(serde_json::from_slice::<serde_json::Value>(&output.stdout).is_ok());
    let output = cli(home.path(), &["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in ["info", "address", "script", "vtxo", "virtual-tx", "tx"] {
        assert!(help.contains(command));
    }
}

#[test]
fn no_arguments_shows_help_on_stderr() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(error.matches("Usage:").count(), 1);
    assert!(error.contains("Usage: akcli [OPTIONS] <COMMAND>"));
    for expected in ["Commands:", "wallet", "Options:", "--help", "--version"] {
        assert!(error.contains(expected), "missing {expected}: {error}");
    }
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn missing_name_reports_required_argument_without_side_effects() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &["wallet", "create"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(error.matches("Usage:").count(), 1);
    assert!(error.contains("the following required arguments were not provided:\n  <NAME>"));
    assert!(
        error.contains("Usage: akcli wallet create <NAME>"),
        "{error}"
    );
    assert!(error.contains("For more information, try '--help'."));
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn arity_and_absolute_home_are_required() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["wallet"],
        vec!["wallet", "create"],
        vec!["wallet", "import"],
        vec!["wallet", "unlock"],
        vec!["wallet", "list", "extra"],
        vec!["wallet", "agent"],
        vec!["wallet", "create", "../invalid"],
    ] {
        assert!(!cli(home.path(), &args).status.success());
    }
    assert!(!home.path().join(".akcli").exists());
    for home in [None, Some("relative")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_akcli"));
        command.env_clear().args(["wallet", "status"]);
        if let Some(home) = home {
            command.env("HOME", home);
        }
        assert!(!command.output().unwrap().status.success());
    }
}

#[cfg(unix)]
#[test]
fn unsafe_runtime_entries_never_become_idempotent_success() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    for entry in ["agent.sock", "agent.lock"] {
        let home = tempfile::tempdir().unwrap();
        let run = home.path().join(".akcli/run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(home.path().join("missing"), run.join(entry)).unwrap();
        for args in [
            vec!["wallet", "status"],
            vec!["wallet", "receive"],
            vec!["wallet", "lock"],
            vec!["wallet", "agent", "stop"],
        ] {
            assert!(!cli(home.path(), &args).status.success());
        }
    }
}

#[test]
fn wallet_help_has_interactive_commands() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &["wallet", "--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for name in ["create", "import", "list", "unlock", "receive"] {
        assert!(help.contains(name));
    }
}

#[test]
fn receive_rejects_operator_flag_without_reading_config_or_starting_agent() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec![
            "--operator",
            "http://secret-marker.invalid",
            "wallet",
            "receive",
        ],
        vec![
            "wallet",
            "receive",
            "--operator",
            "http://secret-marker.invalid",
        ],
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"error: wallet receive does not accept --operator; set operator in .akcli/config.toml\n");
        assert!(!home.path().join(".akcli").exists());
    }
}

#[test]
fn receive_is_parameterless_and_never_starts_an_absent_agent() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["wallet", "receive"],
        vec!["--json", "wallet", "receive"],
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("wallet agent is not running"));
    }
    for extra in [
        "alice",
        "--amount",
        "--rotate",
        "--passphrase",
        "--password",
        "--unlock",
    ] {
        let output = cli(home.path(), &["wallet", "receive", extra]);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn receive_rejects_incompatible_and_malformed_agent_responses_without_restarting() {
    use std::{
        io::{Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixListener},
        time::{Duration, Instant},
    };
    let address = "tark1qpumuen7l8wthtz45p3ftn58pvrs9xlumvkuu2xet8egzkcklqte3egkjh5ls9mvgks7uczg0xv5s4pgkqt6ufmrt3qx0cfym0ja4y7t4pgslm";
    let address_response = [b"\x01\x05".as_slice(), address.as_bytes()].concat();
    for (command, response, extra_length) in [
        ("receive", vec![1, 0], 0),
        ("receive", b"\x01\x01alice".to_vec(), 0),
        ("receive", vec![1, 4], 0),
        ("receive", b"\x01\x05not-an-address".to_vec(), 0),
        (
            "receive",
            [address_response.as_slice(), b"\x1b[31m"].concat(),
            0,
        ),
        (
            "receive",
            [b"\x01\x05".as_slice(), &vec![b'x'; 513]].concat(),
            0,
        ),
        ("receive", address_response.clone(), 1),
        ("receive", vec![], 0), // Older daemons close on unknown opcode 5.
        ("status", address_response, 0),
    ] {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let run = home.path().join(".akcli/run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let lock = run.join("agent.lock");
        std::fs::write(&lock, []).unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
        let socket = run.join("agent.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let generic_error = response == [1, 4];
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "client did not connect");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 6];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(
                request,
                [0, 0, 0, 2, 1, if command == "receive" { 5 } else { 1 }]
            );
            if !response.is_empty() {
                stream
                    .write_all(&((response.len() + extra_length) as u32).to_be_bytes())
                    .unwrap();
                stream.write_all(&response).unwrap();
            }
        });
        let output = cli(home.path(), &["wallet", command]);
        server.join().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(!error.contains(address));
        assert!(!error.contains("not-an-address"));
        assert!(!error.contains('\x1b'));
        if command == "receive" && !generic_error {
            assert!(error.contains("akcli wallet agent stop"));
            assert!(error.contains("unlock again"));
        }
        assert!(socket.exists(), "no automatic cleanup/restart");
        assert!(!home.path().join(".akcli/wallets").exists());
        assert!(!home.path().join(".akcli/config.toml").exists());
    }
}

#[test]
fn absent_status_is_public_and_does_not_create_runtime() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &["wallet", "status", "--json"]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["agent"], "stopped");
    assert_eq!(value["status"], "locked");
    assert!(!home.path().join(".akcli").exists());
}

#[test]
fn absent_lock_and_stop_are_idempotent_without_configuration() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["--json", "wallet", "lock"],
        vec!["wallet", "agent", "stop", "--json"],
    ] {
        for _ in 0..2 {
            assert!(cli(home.path(), &args).status.success());
        }
    }
    assert!(!home.path().join(".akcli").exists());
}

#[cfg(unix)]
#[test]
fn unsafe_runtime_is_not_absence() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".akcli")).unwrap();
    std::fs::set_permissions(
        home.path().join(".akcli"),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    for args in [
        vec!["wallet", "status"],
        vec!["wallet", "receive"],
        vec!["wallet", "lock"],
        vec!["wallet", "agent", "stop"],
    ] {
        assert!(!cli(home.path(), &args).status.success());
    }
    assert!(!home.path().join(".akcli/run").exists());
}

#[test]
fn invalid_word_counts_are_informative_and_have_no_side_effects() {
    let home = tempfile::tempdir().unwrap();
    for value in ["15", "0", "invalid"] {
        let output = cli(
            home.path(),
            &["wallet", "create", "fresh", "--words", value],
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(&format!("invalid value '{value}' for '--words <WORDS>'")));
        assert!(error.contains("possible values: 12, 24"));
    }
    for args in [
        vec!["wallet", "create", "fresh", "--words"],
        vec!["wallet", "import", "fresh", "--words", "12"],
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("--words"));
    }
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn creation_help_explains_word_choices_without_changing_import_or_version() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &["wallet", "create", "--help"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout).unwrap();
    for expected in ["--words", "[default: 12]", "[possible values: 12, 24]"] {
        assert!(help.contains(expected));
    }
    let output = cli(home.path(), &["wallet", "import", "--help"]);
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("--words"));
    let output = cli(home.path(), &["--version"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("akcli "));
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}
