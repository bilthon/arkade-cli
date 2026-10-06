use super::{fs::Directory, Error, WalletName};
use crate::config::ReceiveSettings;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const PROFILE: &str = "sdk65b383e-default-empty-passphrase-index0";
const DATABASE_FILES: [&str; 4] = [
    "contracts.db",
    "contracts.db-journal",
    "contracts.db-wal",
    "contracts.db-shm",
];

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u32,
    environment: String,
    operator_rest_url: String,
    derivation_profile: String,
}

/// Receive-only public state. Keep this owner alive while the SDK uses the database.
/// The SDK's later pathname open is not protected from hostile same-user mutation.
pub struct ReceiveStore {
    directory: Directory,
    database_path: PathBuf,
}

impl ReceiveStore {
    /// Opens only existing private directories; never creates or repairs storage.
    pub fn open_existing(home: &Path, name: &WalletName) -> Result<Self, Error> {
        Ok(Self {
            directory: Directory::open_existing_wallet(home, name)?,
            database_path: home
                .join(".akcli/wallets")
                .join(name.as_str())
                .join("contracts.db"),
        })
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Read-only validation of the binding and every SQLite file, including auxiliaries.
    pub fn check_binding(&self, settings: &ReceiveSettings) -> Result<(), Error> {
        self.inspect(settings).map(|_| ())
    }

    // Return presence only after validating all files. No unknown database is adopted.
    fn inspect(&self, settings: &ReceiveSettings) -> Result<(bool, bool), Error> {
        let metadata = self.directory.read_file("metadata.json")?;
        if let Some(bytes) = &metadata {
            let binding: Metadata = serde_json::from_slice(bytes).map_err(|_| Error::Storage)?;
            if binding.version != 2
                || binding.derivation_profile != PROFILE
                || binding.environment != settings.environment()
                || binding.operator_rest_url != settings.operator_rest_url()
            {
                return Err(Error::Storage);
            }
        }
        let mut database = false;
        for (index, name) in DATABASE_FILES.iter().enumerate() {
            let exists = self.directory.file(name)?.is_some();
            if exists && (metadata.is_none() || (index > 0 && !database)) {
                return Err(Error::Storage);
            }
            if index == 0 {
                database = exists;
            }
        }
        Ok((metadata.is_some(), database))
    }

    /// Call only after operator validation. Publishes metadata without replacement,
    /// then exclusively precreates the database. Metadata-only failures are retryable.
    pub fn initialize(&self, settings: &ReceiveSettings) -> Result<(), Error> {
        let (metadata, database) = self.inspect(settings)?;
        if !metadata {
            let bytes = serde_json::to_vec(&Metadata {
                version: 2,
                environment: settings.environment().to_owned(),
                operator_rest_url: settings.operator_rest_url().to_owned(),
                derivation_profile: PROFILE.to_owned(),
            })
            .map_err(|_| Error::Storage)?;
            self.directory.publish_file("metadata.json", &bytes)?;
        }
        if !database {
            self.directory.create_database()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn fixture() -> (tempfile::TempDir, WalletName) {
        let home = tempfile::tempdir().unwrap();
        let name = WalletName::new("alice").unwrap();
        for part in [".akcli", ".akcli/wallets", ".akcli/wallets/alice"] {
            let path = home.path().join(part);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        (home, name)
    }

    fn settings() -> ReceiveSettings {
        ReceiveSettings::new("mutinynet", "https://localhost:7070/").unwrap()
    }

    fn put(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    const VALID: &str = r#"{"version":2,"environment":"mutinynet","operator_rest_url":"https://localhost:7070","derivation_profile":"sdk65b383e-default-empty-passphrase-index0"}"#;

    #[test]
    fn metadata_is_strict_bounded_and_not_rewritten() {
        for invalid in [
            VALID.replace("\"version\":2", "\"version\":1"),
            VALID.replace("\"version\":2", "\"version\":3"),
            VALID.replace("\"version\":2", "\"version\":2,\"version\":2"),
            VALID.replace("\"version\":2", "\"version\":2,\"extra\":true"),
            VALID.replace("sdk65b383e-default-empty-passphrase-index0", "custom"),
            VALID.replace("mutinynet", "unknown"),
            VALID.replace("https://localhost:7070", "https://localhost:7070/"),
            "{}".to_owned(),
            " ".repeat(16 * 1024 + 1),
        ] {
            let (home, name) = fixture();
            let store = ReceiveStore::open_existing(home.path(), &name).unwrap();
            let path = store.database_path().with_file_name("metadata.json");
            put(&path, invalid.as_bytes());
            assert!(store.check_binding(&settings()).is_err());
            assert!(store.initialize(&settings()).is_err());
            assert!(!store.database_path().exists());
            assert_eq!(fs::read_to_string(path).unwrap(), invalid);
        }
    }

    #[test]
    fn metadata_only_retry_preserves_metadata_and_envelope() {
        let (home, name) = fixture();
        let store = ReceiveStore::open_existing(home.path(), &name).unwrap();
        let metadata = store.database_path().with_file_name("metadata.json");
        let envelope = store.database_path().with_file_name("alice.json");
        put(&metadata, VALID.as_bytes());
        put(&envelope, b"synthetic untouched envelope");
        store.check_binding(&settings()).unwrap();
        assert!(!store.database_path().exists());
        store.initialize(&settings()).unwrap();
        assert_eq!(fs::read_to_string(metadata).unwrap(), VALID);
        assert_eq!(fs::read(envelope).unwrap(), b"synthetic untouched envelope");
        for file in ["metadata.json", "contracts.db"] {
            assert_eq!(
                fs::metadata(store.database_path().with_file_name(file))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                0o600
            );
        }
    }

    #[test]
    fn orphan_database_and_auxiliaries_are_never_adopted() {
        for file in DATABASE_FILES {
            let (home, name) = fixture();
            let store = ReceiveStore::open_existing(home.path(), &name).unwrap();
            let path = store.database_path().with_file_name(file);
            put(&path, b"unbound");
            assert!(store.check_binding(&settings()).is_err());
            assert!(store.initialize(&settings()).is_err());
            assert_eq!(fs::read(path).unwrap(), b"unbound");
            assert!(!store
                .database_path()
                .with_file_name("metadata.json")
                .exists());
        }
    }

    #[test]
    fn unsafe_receive_files_are_rejected_without_repair() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        for file in [
            "metadata.json",
            "contracts.db",
            "contracts.db-journal",
            "contracts.db-wal",
            "contracts.db-shm",
        ] {
            for kind in ["permissions", "symlink", "directory", "fifo", "socket"] {
                let (home, name) = fixture();
                let store = ReceiveStore::open_existing(home.path(), &name).unwrap();
                let path = store.database_path().with_file_name(file);
                if file != "metadata.json" {
                    put(&path.with_file_name("metadata.json"), VALID.as_bytes());
                }
                if file.starts_with("contracts.db-") {
                    put(store.database_path(), b"");
                }
                let _listener = match kind {
                    "permissions" => {
                        put(&path, VALID.as_bytes());
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
                        None
                    }
                    "symlink" => {
                        symlink("absent-target", &path).unwrap();
                        None
                    }
                    "directory" => {
                        fs::create_dir(&path).unwrap();
                        None
                    }
                    "fifo" => {
                        assert!(std::process::Command::new("/usr/bin/mkfifo")
                            .env_clear()
                            .args(["-m", "600"])
                            .arg(&path)
                            .status()
                            .unwrap()
                            .success());
                        None
                    }
                    "socket" => Some(UnixListener::bind(&path).unwrap()),
                    _ => unreachable!(),
                };
                let before = fs::symlink_metadata(&path).unwrap();
                assert!(store.check_binding(&settings()).is_err(), "{file}: {kind}");
                assert!(store.initialize(&settings()).is_err(), "{file}: {kind}");
                let after = fs::symlink_metadata(&path).unwrap();
                assert_eq!(before.permissions().mode(), after.permissions().mode());
                assert_eq!(before.file_type(), after.file_type());
            }
        }
    }

    #[test]
    fn open_requires_existing_safe_directories_without_writes() {
        let empty = tempfile::tempdir().unwrap();
        let name = WalletName::new("alice").unwrap();
        assert!(ReceiveStore::open_existing(empty.path(), &name).is_err());
        assert_eq!(fs::read_dir(empty.path()).unwrap().count(), 0);
        let (home, _) = fixture();
        assert!(
            ReceiveStore::open_existing(home.path(), &WalletName::new("missing").unwrap()).is_err()
        );
        assert!(!home.path().join(".akcli/wallets/missing").exists());
        assert!(ReceiveStore::open_existing(Path::new("relative"), &name).is_err());
        for part in [".akcli", ".akcli/wallets", ".akcli/wallets/alice"] {
            let (home, name) = fixture();
            let path = home.path().join(part);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
            assert!(ReceiveStore::open_existing(home.path(), &name).is_err());
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                0o777
            );
        }
    }

    #[test]
    fn directory_symlinks_and_special_objects_are_not_followed() {
        for depth in 0..3 {
            for kind in ["symlink", "file", "fifo"] {
                let home = tempfile::tempdir().unwrap();
                let mut path = home.path().to_path_buf();
                for part in &[".akcli", "wallets", "alice"][..depth] {
                    path.push(part);
                    fs::create_dir(&path).unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                }
                path.push([".akcli", "wallets", "alice"][depth]);
                match kind {
                    "symlink" => std::os::unix::fs::symlink(home.path(), &path).unwrap(),
                    "file" => put(&path, b"not a directory"),
                    "fifo" => assert!(std::process::Command::new("/usr/bin/mkfifo")
                        .env_clear()
                        .args(["-m", "600"])
                        .arg(&path)
                        .status()
                        .unwrap()
                        .success()),
                    _ => unreachable!(),
                }
                assert!(ReceiveStore::open_existing(
                    home.path(),
                    &WalletName::new("alice").unwrap()
                )
                .is_err());
            }
        }
    }

    #[test]
    fn conflicting_initializers_cannot_overwrite_binding() {
        let (home, name) = fixture();
        let first = settings();
        let second = ReceiveSettings::new("signet", "https://other.invalid").unwrap();
        let barrier = std::sync::Barrier::new(2);
        let run = |settings: &ReceiveSettings| {
            let store = ReceiveStore::open_existing(home.path(), &name).unwrap();
            barrier.wait();
            store.initialize(settings)
        };
        let results = std::thread::scope(|scope| {
            let a = scope.spawn(|| run(&first));
            let b = scope.spawn(|| run(&second));
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(results.0.is_ok(), results.1.is_ok());
        let winner = if results.0.is_ok() { &first } else { &second };
        let store = ReceiveStore::open_existing(home.path(), &name).unwrap();
        store.check_binding(winner).unwrap();
        assert!(store.database_path().exists());
    }

    #[test]
    fn descriptor_is_retained_and_safe_auxiliaries_are_accepted() {
        let (home, name) = fixture();
        let store = ReceiveStore::open_existing(home.path(), &name).unwrap();
        store.initialize(&settings()).unwrap();
        for file in &DATABASE_FILES[1..] {
            put(
                &store.database_path().with_file_name(file),
                b"synthetic auxiliary",
            );
        }
        let original = store.database_path().parent().unwrap();
        fs::rename(original, original.with_file_name("moved")).unwrap();
        // Descriptor-relative checks still work, but the SDK path no longer does:
        // this demonstrates retention, not protection from hostile same-user mutation.
        assert!(!store.database_path().exists());
        store.check_binding(&settings()).unwrap();
    }

    #[test]
    fn receive_initialization_roundtrip_and_binding() {
        let (home, name) = fixture();
        let store = ReceiveStore::open_existing(home.path(), &name).unwrap();
        store.check_binding(&settings()).unwrap();
        assert_eq!(
            fs::read_dir(store.database_path().parent().unwrap())
                .unwrap()
                .count(),
            0
        );
        store.initialize(&settings()).unwrap();
        assert_eq!(fs::metadata(store.database_path()).unwrap().len(), 0);
        drop(store);
        let store = ReceiveStore::open_existing(home.path(), &name).unwrap();
        store.initialize(&settings()).unwrap();
        assert!(store
            .check_binding(&ReceiveSettings::new("signet", "https://localhost:7070").unwrap())
            .is_err());
        assert!(store
            .check_binding(&ReceiveSettings::new("mutinynet", "https://other.invalid").unwrap())
            .is_err());
    }
}
