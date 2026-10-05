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
fn missing_name_reports_safe_usage_without_side_effects() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &["wallet", "create"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("missing required argument"));
    assert!(error.contains("Usage: akcli wallet create [OPTIONS] <NAME>"));
    assert!(error.contains("--help"));
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
    for name in ["create", "import", "list", "unlock"] {
        assert!(help.contains(name));
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
        vec!["wallet", "lock"],
        vec!["wallet", "agent", "stop"],
    ] {
        assert!(!cli(home.path(), &args).status.success());
    }
    assert!(!home.path().join(".akcli/run").exists());
}

#[test]
fn parser_errors_do_not_echo_accidental_secret_arguments() {
    let home = tempfile::tempdir().unwrap();
    let marker = "synthetic-secret-marker";
    for args in [
        vec!["wallet", "status", marker],
        vec!["wallet", "lock", "--password", marker],
        vec!["wallet", "agent", "stop", "--mnemonic", marker],
        vec!["wallet", "status", "--color", marker],
        vec!["--timeout", marker, "wallet", "status"],
        vec!["wallet", "create", "fresh", "--words", marker],
        vec!["wallet", "create", marker, "--words", "15"],
        vec!["wallet", "create", "--words", "0", marker],
        vec!["wallet", "create", "--words", "24", "fresh", marker],
        vec!["wallet", "create", "--words", "12", "fresh", "--password", marker],
        vec!["--operator", marker, "wallet", "create", "--words", "15"],
        vec![marker, "wallet", "create"],
        vec!["wallet", "create", "fresh", "--words"],
        vec!["wallet", "import", "fresh", "--words", "12"],
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Usage: akcli"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(marker));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(marker));
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }
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
