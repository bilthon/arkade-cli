#![cfg(unix)]
use arkade_cli::wallet::{Secret, WalletName, WalletStore};

use serde_json::{json, Value};
use std::os::unix::fs::{symlink, PermissionsExt};

const PHRASE: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
fn secret(value: &str) -> Secret {
    Secret::new(value.to_owned())
}

#[test]
fn wallet_names_are_validated_without_normalization() {
    for name in ["a", "wallet-1", "a_b", &"a".repeat(32)] {
        assert_eq!(WalletName::new(name).unwrap().as_str(), name);
    }
    for name in [
        "",
        "A",
        "../a",
        "a/b",
        "a\\b",
        "-a",
        "_a",
        "a b",
        "é",
        &"a".repeat(33),
    ] {
        assert!(WalletName::new(name).is_err());
    }
}

#[test]
fn create_list_unlock_encrypted_mnemonic() {
    let home = tempfile::tempdir().unwrap();
    let store = WalletStore::open(home.path()).unwrap();
    let name = WalletName::new("primary").unwrap();
    store
        .create(&name, secret(PHRASE), secret("password"))
        .unwrap();
    assert_eq!(store.list().unwrap(), vec![name.clone()]);
    assert_eq!(
        store.unlock(&name, secret("password")).unwrap().expose(),
        PHRASE
    );
    let start = std::time::Instant::now();
    assert!(store.unlock(&name, secret("password")).is_ok());
    eprintln!("production-profile unlock elapsed: {:?}", start.elapsed());
    let disk = std::fs::read_to_string(home.path().join(".akcli/wallets/primary.json")).unwrap();
    assert!(!disk.contains("abandon"));
    assert!(!disk.contains("password"));
}

fn fixture() -> (
    tempfile::TempDir,
    WalletStore,
    WalletName,
    std::path::PathBuf,
) {
    let home = tempfile::tempdir().unwrap();
    let store = WalletStore::open(home.path()).unwrap();
    let name = WalletName::new("primary").unwrap();
    store
        .create(&name, secret(PHRASE), secret("password"))
        .unwrap();
    let path = home.path().join(".akcli/wallets/primary.json");
    (home, store, name, path)
}

#[test]
fn wrong_password_and_each_tampered_field_have_safe_errors() {
    let (_home, store, name, path) = fixture();
    let original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        store
            .unlock(&name, secret("wrong-sensitive-password"))
            .err()
            .unwrap()
            .to_string(),
        "unable to unlock wallet"
    );
    for (field, replacement) in [
        ("version", json!(2)),
        ("name", json!("other")),
        ("profile", json!("argon2id-v19-m1-t1-p1-xchacha20poly1305")),
        ("salt", json!("AAAAAAAAAAAAAAAAAAAAAA==")),
        ("nonce", json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")),
        ("ciphertext", json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")),
    ] {
        let mut changed = original.clone();
        changed[field] = replacement;
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_eq!(
            store
                .unlock(&name, secret("password"))
                .err()
                .unwrap()
                .to_string(),
            "unable to unlock wallet",
            "{field}"
        );
    }
}

#[test]
fn rejects_unbounded_unknown_and_malformed_envelopes() {
    let (_home, store, name, path) = fixture();
    let original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut cases = vec![vec![b' '; 16385], b"{}".to_vec(), b"not JSON".to_vec()];
    for (field, replacement) in [
        ("memory", json!(1)),
        ("parameters", json!({"m": 1})),
        ("version", json!(0)),
        ("profile", json!("unknown")),
        ("salt", json!("AA==")),
        ("nonce", json!("AA==")),
        ("ciphertext", json!("AA==")),
        ("salt", json!("not-base64")),
        ("nonce", json!("not-base64")),
        ("ciphertext", json!("not-base64")),
        ("ciphertext", json!("A".repeat(1024))),
    ] {
        let mut changed = original.clone();
        changed[field] = replacement;
        cases.push(serde_json::to_vec(&changed).unwrap());
    }
    let serialized = serde_json::to_string(&original).unwrap();
    cases.push(format!("{{\"version\":1,{}", &serialized[1..]).into_bytes());
    for bytes in cases {
        std::fs::write(&path, bytes).unwrap();
        assert!(store.list().is_err());
        assert_eq!(
            store
                .unlock(&name, secret("password"))
                .err()
                .unwrap()
                .to_string(),
            "unable to unlock wallet"
        );
    }
}

#[test]
fn copied_wallet_cannot_be_rebound_by_renaming_metadata() {
    let (home, store, _name, path) = fixture();
    let other = WalletName::new("other").unwrap();
    let other_path = home.path().join(".akcli/wallets/other.json");
    std::fs::copy(&path, &other_path).unwrap();
    assert!(store.list().is_err());
    assert!(store.unlock(&other, secret("password")).is_err());
    let mut changed: Value = serde_json::from_slice(&std::fs::read(&other_path).unwrap()).unwrap();
    changed["name"] = json!("other");
    std::fs::write(&other_path, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(store.unlock(&other, secret("password")).is_err());
}

#[test]
fn validates_and_canonicalizes_mnemonics_without_password_policy() {
    let home = tempfile::tempdir().unwrap();
    let store = WalletStore::open(home.path()).unwrap();
    let name = WalletName::new("primary").unwrap();
    for phrase in ["sensitive invalid mnemonic", "abandon abandon abandon", "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon"] {
        assert_eq!(store.create(&name, secret(phrase), secret("x")).err().unwrap().to_string(), "invalid English mnemonic");
    }
    assert!(store.list().unwrap().is_empty());
    assert_eq!(
        store
            .create(&name, secret(PHRASE), secret(""))
            .err()
            .unwrap()
            .to_string(),
        "password must not be empty"
    );
    store
        .create(
            &name,
            secret(&format!("  {}  ", PHRASE.replace(' ', "  "))),
            secret(" "),
        )
        .unwrap();
    assert_eq!(store.unlock(&name, secret(" ")).unwrap().expose(), PHRASE);
}

#[test]
fn publication_is_private_randomized_and_never_clobbers() {
    let (home, store, name, path) = fixture();
    let original = std::fs::read(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(store
        .create(&name, secret(PHRASE), secret("different"))
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let other = WalletName::new("other").unwrap();
    store
        .create(&other, secret(PHRASE), secret("password"))
        .unwrap();
    let a: Value = serde_json::from_slice(&original).unwrap();
    let b: Value = serde_json::from_slice(
        &std::fs::read(home.path().join(".akcli/wallets/other.json")).unwrap(),
    )
    .unwrap();
    for field in ["salt", "nonce", "ciphertext"] {
        assert_ne!(a[field], b[field]);
    }
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        2
    );
}

#[test]
fn racing_creates_have_one_winner() {
    let home = tempfile::tempdir().unwrap();
    let store = WalletStore::open(home.path()).unwrap();
    let name = WalletName::new("race").unwrap();
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let spawn = |password: &'static str| {
            let store = &store;
            let name = &name;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                store.create(name, secret(PHRASE), secret(password))
            })
        };
        let a = spawn("one");
        let b = spawn("two");
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(results.0.is_ok(), results.1.is_ok());
    let password = if results.0.is_ok() { "one" } else { "two" };
    assert_eq!(
        store.unlock(&name, secret(password)).unwrap().expose(),
        PHRASE
    );
    assert_eq!(store.list().unwrap(), vec![name]);
}

#[test]
fn rejects_relative_home_and_unsafe_directory_paths() {
    assert!(WalletStore::open(std::path::Path::new("relative")).is_err());
    for component in [".akcli", ".akcli/wallets"] {
        let home = tempfile::tempdir().unwrap();
        if component.contains('/') {
            std::fs::create_dir(home.path().join(".akcli")).unwrap();
        }
        let target = tempfile::tempdir().unwrap();
        symlink(target.path(), home.path().join(component)).unwrap();
        assert!(WalletStore::open(home.path()).is_err());
        assert!(std::fs::read_dir(target.path()).unwrap().next().is_none());
    }
    for (component, mode) in [(".akcli", 0o777), (".akcli/wallets", 0o755)] {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(component);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(WalletStore::open(home.path()).is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
    for component in [".akcli", ".akcli/wallets"] {
        let home = tempfile::tempdir().unwrap();
        if component.contains('/') {
            std::fs::create_dir(home.path().join(".akcli")).unwrap();
        }
        std::fs::write(home.path().join(component), b"not a directory").unwrap();
        assert!(WalletStore::open(home.path()).is_err());
    }
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".akcli")).unwrap();
    std::fs::set_permissions(
        home.path().join(".akcli"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(WalletStore::open(home.path()).is_ok());
}

#[test]
fn rejects_symlink_files_bad_permissions_and_nonregular_objects() {
    let (home, store, name, path) = fixture();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.list().is_err());
    assert!(store.unlock(&name, secret("password")).is_err());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    let other = WalletName::new("link").unwrap();
    symlink(&path, home.path().join(".akcli/wallets/link.json")).unwrap();
    assert!(store.unlock(&other, secret("password")).is_err());
    assert!(store
        .create(&other, secret(PHRASE), secret("password"))
        .is_err());
    let directory = WalletName::new("directory").unwrap();
    std::fs::create_dir(home.path().join(".akcli/wallets/directory.json")).unwrap();
    assert!(store.unlock(&directory, secret("password")).is_err());
    let socket = WalletName::new("socket").unwrap();
    let _listener =
        std::os::unix::net::UnixListener::bind(home.path().join(".akcli/wallets/socket.json"))
            .unwrap();
    assert!(store.unlock(&socket, secret("password")).is_err());
}

#[test]
fn listing_rejects_unsafe_names_without_exposing_them() {
    let home = tempfile::tempdir().unwrap();
    let store = WalletStore::open(home.path()).unwrap();
    std::fs::write(home.path().join(".akcli/wallets/UPPER.json"), b"{}").unwrap();
    assert_eq!(
        store.list().err().unwrap().to_string(),
        "invalid wallet envelope"
    );
}

#[test]
fn all_english_bip39_lengths_are_accepted() {
    let home = tempfile::tempdir().unwrap();
    let store = WalletStore::open(home.path()).unwrap();
    for size in [16, 20, 24, 28, 32] {
        let mnemonic = bip39::Mnemonic::from_entropy(&vec![0; size])
            .unwrap()
            .to_string();
        let name = WalletName::new(&format!("words-{size}")).unwrap();
        store.create(&name, secret(&mnemonic), secret("x")).unwrap();
        assert_eq!(store.unlock(&name, secret("x")).unwrap().expose(), mnemonic);
    }
}
