use super::{
    protocol::{self, Request},
    receive,
    runtime::{self, Runtime},
    state::State,
    Agent, Error, Status,
};
use crate::{
    config::load_receive_settings,
    wallet::{ReceiveStore, Secret, WalletName, WalletStore},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    net::UnixStream,
    sync::{Notify, Semaphore},
    task::JoinSet,
    time::timeout,
};
use zeroize::Zeroizing;

struct Coordinator {
    state: Mutex<State>,
    changed: Notify,
    kdf: Arc<Semaphore>,
    receive: tokio::sync::Mutex<()>,
}
impl Coordinator {
    fn new(lease: Duration) -> Self {
        Self {
            state: Mutex::new(State::new(lease)),
            changed: Notify::new(),
            kdf: Arc::new(Semaphore::new(1)),
            receive: tokio::sync::Mutex::new(()),
        }
    }
    // A separate active future, not lazy expiry in status(). Used by the serving
    // loop and exercised with Tokio's deterministic monotonic clock in tests.
    async fn maintain(&self) {
        loop {
            let deadline = {
                let mut state = self.state.lock().unwrap();
                state.expire();
                if state.ticket().is_none() {
                    return;
                }
                state.deadline()
            };
            tokio::select! {
                _ = self.changed.notified() => (),
                _ = runtime::expire_at(deadline) => (),
            }
        }
    }
}

fn still_connected(stream: &UnixStream) -> Result<(), Error> {
    let mut extra = Zeroizing::new([0; 1]);
    match stream.try_read(&mut *extra) {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
        _ => Err(Error::Protocol),
    }
}

// The worker seam varies only the expensive blocking operation in deterministic
// cancellation tests. Production always uses W1's unchanged fixed-cost unlock.
async fn unlock<F>(
    stream: &mut UnixStream,
    coordinator: &Coordinator,
    name: WalletName,
    work: F,
) -> Result<Status, Error>
where
    F: FnOnce() -> Result<Secret, Error> + Send + 'static,
{
    let permit = coordinator
        .kdf
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    let ticket = coordinator
        .state
        .lock()
        .unwrap()
        .ticket()
        .ok_or(Error::Busy)?;
    let (completed, result) = tokio::sync::oneshot::channel();
    let _worker = tokio::task::spawn_blocking(move || {
        // The actual worker owns the permit, even after request cancellation.
        // Sending to a cancelled receiver drops the result BEFORE this permit;
        // no secret is left in an abandoned JoinHandle after shutdown's wait.
        let _permit = permit;
        let _ = completed.send(work());
    });
    let mut extra = Zeroizing::new([0u8; 1]);
    let mnemonic = tokio::select! {
        result = result => result.map_err(|_|Error::Unlock)??,
        _ = stream.read(&mut *extra) => return Err(Error::Protocol),
    };
    still_connected(stream)?;
    let mut state = coordinator.state.lock().unwrap();
    if !state.install(ticket, name, mnemonic) {
        return Err(Error::Busy);
    }
    let status = state.status();
    drop(state);
    coordinator.changed.notify_one();
    Ok(status)
}

// The only test seam is the operator-info network future; SDK derivation and
// persistence always run unchanged. Caller owns this future, never a spawned job.
async fn receive<F, Fut>(
    stream: &mut UnixStream,
    agent: &Agent,
    coordinator: &Coordinator,
    info: F,
) -> Result<String, Error>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<
        Output = Result<(crate::config::ReceiveSettings, ark_core::server::Info), Error>,
    >,
{
    let (name, deadline, mut session, identity) = {
        let mut state = coordinator.state.lock().unwrap();
        let (name, deadline, session) = state.receive_session().ok_or(Error::Receive)?;
        let identity = *session.borrow();
        (name, deadline, session, identity)
    };
    let operation = async {
        // At most the existing 32 admitted connections can wait here. Controls
        // bypass this lock. Cancellation removes the waiter or drops the guard.
        let _serial = coordinator.receive.lock().await;
        let operator = load_receive_settings(&agent.home).map_err(|error| match error {
            crate::config::ReceiveConfigError::MissingOperator => Error::ReceiveMissingOperator,
            crate::config::ReceiveConfigError::Invalid => Error::ReceiveConfig,
        })?;
        let store =
            ReceiveStore::open_existing(&agent.home, &name).map_err(|_| Error::ReceiveStorage)?;
        let (settings, info) = info(operator.url().to_owned()).await?;
        store
            .check_binding(&settings)
            .map_err(|_| Error::ReceiveStorage)?;
        still_connected(stream)?;
        let owner = {
            let mut state = coordinator.state.lock().unwrap();
            receive::owner(
                state.mnemonic(identity).ok_or(Error::Receive)?,
                info.network,
            )?
        };
        // No network waits or key-bearing objects beyond this point. SQLite is
        // synchronous and cannot be preempted, but stale results are discarded.
        if coordinator
            .state
            .lock()
            .unwrap()
            .mnemonic(identity)
            .is_none()
        {
            return Err(Error::Receive);
        }
        let address = receive::persist(&store, &settings, &info, owner)?;
        // Let the enclosing whole-request timeout/abort observe a synchronous
        // SQLite overrun before a successful response can escape.
        tokio::task::yield_now().await;
        still_connected(stream)?;
        if coordinator
            .state
            .lock()
            .unwrap()
            .mnemonic(identity)
            .is_none()
        {
            return Err(Error::Receive);
        }
        Ok(address)
    };
    let disconnected = async {
        loop {
            if stream.readable().await.is_err() || still_connected(stream).is_err() {
                return;
            }
        }
    };
    tokio::select! {
        biased;
        _ = session.changed() => Err(Error::Receive),
        _ = tokio::time::sleep_until(deadline) => Err(Error::Receive),
        _ = disconnected => Err(Error::Protocol),
        result = operation => result,
    }
}

struct ChangedOnDrop<'a>(&'a Notify);
impl Drop for ChangedOnDrop<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
async fn handle(
    mut stream: UnixStream,
    agent: Agent,
    coordinator: Arc<Coordinator>,
) -> Result<(), Error> {
    protocol::authenticate(&stream)?;
    let frame = timeout(runtime::IO_DEADLINE, protocol::read(&mut stream))
        .await
        .map_err(|_| Error::Timeout)??;
    let request = protocol::parse(&frame)?;
    drop(frame);
    still_connected(&stream)?;
    let result = match request {
        Request::Status => Ok(coordinator.state.lock().unwrap().status()),
        Request::Receive => {
            let result = receive(&mut stream, &agent, &coordinator, |endpoint| async move {
                receive::operator(&endpoint).await
            })
            .await;
            return protocol::write(&mut stream, &protocol::receive_response(result)).await;
        }
        Request::Lock | Request::Stop => {
            let stop = matches!(request, Request::Stop);
            coordinator.state.lock().unwrap().lock(stop);
            // Invalidate first. Notify after acknowledgement, including if this
            // write is cancelled, so stop cannot strand a locked serving loop.
            let _notify = ChangedOnDrop(&coordinator.changed);
            return protocol::write(&mut stream, &protocol::response(Ok(Status::Locked))).await;
        }
        Request::Unlock(name, password) => {
            let worker_name = name.clone();
            unlock(&mut stream, &coordinator, name, move || {
                WalletStore::open(&agent.home)
                    .and_then(|store| store.unlock(&worker_name, password))
                    .map_err(|_| Error::Unlock)
            })
            .await
        }
    };
    protocol::write(&mut stream, &protocol::response(result)).await
}

pub(super) async fn serve(agent: Agent) -> Result<(), Error> {
    let mut runtime = Runtime::open(&agent)?;
    let listener = runtime.bind().await?;
    let coordinator = Arc::new(Coordinator::new(agent.options.lease()));
    let connections = Arc::new(Semaphore::new(32));
    let mut tasks = JoinSet::new();
    let maintenance = coordinator.maintain();
    tokio::pin!(maintenance);
    let result = loop {
        tokio::select! {
            biased;
            _ = &mut maintenance => break Ok(()),
            Some(_) = tasks.join_next() => (),
            accepted = listener.accept() => {
                let (stream,_) = match accepted {
                    Ok(accepted) => accepted,
                    Err(_) => break Err(Error::Runtime),
                };
                let Ok(permit) = connections.clone().try_acquire_owned() else { drop(stream); continue; };
                let agent = agent.clone();
                let coordinator = coordinator.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    // One whole-request budget, not a deadline reset for each byte.
                    let _ = timeout(Duration::from_secs(25),handle(stream,agent,coordinator)).await;
                });
            }
        }
    };
    coordinator.state.lock().unwrap().lock(true);
    drop(listener);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    // Running Tokio blocking work cannot be aborted. Keep the lock until the
    // bounded W1 worker finishes. Its abandoned result is dropped, not installed.
    let _finished = coordinator
        .kdf
        .acquire()
        .await
        .map_err(|_| Error::Runtime)?;
    drop(runtime);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_client::{ContractStore, SqliteContractStore};
    use ark_core::{contract::ContractState, server::Info};
    use bitcoin::{Network, PublicKey, Sequence};
    use std::{fs, os::unix::fs::PermissionsExt, str::FromStr};

    fn info() -> (crate::config::ReceiveSettings, Info) {
        let signer = PublicKey::from_str(
            "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        )
        .unwrap();
        (
            crate::config::ReceiveSettings::new("regtest", "http://127.0.0.1:1").unwrap(),
            Info {
                version: "synthetic".into(),
                signer_pk: signer.inner,
                forfeit_pk: signer.inner,
                forfeit_address: bitcoin::Address::p2tr(
                    &bitcoin::secp256k1::Secp256k1::new(),
                    signer.inner.x_only_public_key().0,
                    None,
                    Network::Regtest,
                ),
                checkpoint_tapscript: bitcoin::ScriptBuf::new(),
                network: Network::Regtest,
                session_duration: 10,
                unilateral_exit_delay: Sequence::from_height(144),
                boarding_exit_delay: Sequence::from_height(144),
                utxo_min_amount: None,
                utxo_max_amount: None,
                vtxo_min_amount: None,
                vtxo_max_amount: None,
                dust: bitcoin::Amount::from_sat(330),
                fees: None,
                scheduled_session: None,
                deprecated_signers: vec![],
                service_status: Default::default(),
                digest: "synthetic".into(),
                max_tx_weight: 0,
                max_op_return_outputs: 0,
            },
        )
    }
    fn receive_fixture() -> (tempfile::TempDir, Agent, Arc<Coordinator>) {
        let home = tempfile::tempdir().unwrap();
        for part in [".akcli", ".akcli/wallets", ".akcli/wallets/alice"] {
            let path = home.path().join(part);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(
            home.path().join(".akcli/config.toml"),
            "operator = 'http://127.0.0.1:1'\n",
        )
        .unwrap();
        let agent = Agent::new(home.path().to_owned(), Default::default()).unwrap();
        let coordinator = Arc::new(Coordinator::new(Duration::from_secs(60)));
        {
            let mut state = coordinator.state.lock().unwrap();
            let ticket = state.ticket().unwrap();
            let mnemonic = bip39::Mnemonic::from_entropy(&[0; 16]).unwrap();
            assert!(state.install(ticket, name(), Secret::new(mnemonic.to_string())));
        }
        (home, agent, coordinator)
    }

    #[tokio::test]
    async fn receive_persists_index_zero_reopens_and_does_not_renew() {
        let (home, agent, coordinator) = receive_fixture();
        let (_client, mut stream) = UnixStream::pair().unwrap();
        let deadline = coordinator.state.lock().unwrap().deadline();
        let first = receive(&mut stream, &agent, &coordinator, |_| async { Ok(info()) })
            .await
            .unwrap();
        let second = receive(&mut stream, &agent, &coordinator, |_| async { Ok(info()) })
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(coordinator.state.lock().unwrap().deadline(), deadline);
        let path = home.path().join(".akcli/wallets/alice/contracts.db");
        let sqlite = SqliteContractStore::new(&path).unwrap();
        let rows = sqlite.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key_index, Some(0));
        assert_eq!(rows[0].state, ContractState::Active);
        // Independent BIP32 path check: do not reuse the production provider or
        // SDK default-path constant when checking the approved derivation.
        let mnemonic = bip39::Mnemonic::from_entropy(&[0; 16]).unwrap();
        let seed = Zeroizing::new(mnemonic.to_seed(""));
        let key = bitcoin::bip32::Xpriv::new_master(Network::Regtest, seed.as_ref())
            .unwrap()
            .derive_priv(
                &bitcoin::secp256k1::Secp256k1::new(),
                &bitcoin::bip32::DerivationPath::from_str("m/83696968'/11811'/0/0").unwrap(),
            )
            .unwrap()
            .to_keypair(&bitcoin::secp256k1::Secp256k1::new())
            .x_only_public_key()
            .0;
        assert_eq!(rows[0].data["owner"], key.to_string());
        // Golden address from this pinned SDK fixture, with the owner independently
        // checked above (zero entropy, empty passphrase, signer G, 144 blocks).
        assert_eq!(first, "tark1qpumuen7l8wthtz45p3ftn58pvrs9xlumvkuu2xet8egzkcklqte3egkjh5ls9mvgks7uczg0xv5s4pgkqt6ufmrt3qx0cfym0ja4y7t4pgslm");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn receive_failures_never_return_an_address_or_initialize_on_mismatch() {
        let (home, agent, coordinator) = receive_fixture();
        let (_client, mut stream) = UnixStream::pair().unwrap();
        let dir = home.path().join(".akcli/wallets/alice");
        assert!(receive(&mut stream, &agent, &coordinator, |_| async {
            Err(Error::ReceiveInfo)
        })
        .await
        .is_err());
        assert!(!dir.join("metadata.json").exists());
        assert!(!dir.join("contracts.db").exists());
        assert!(receive(&mut stream, &agent, &coordinator, |_| async {
            Err(Error::Receive)
        })
        .await
        .is_err());
        assert!(!dir.join("metadata.json").exists());
        let config_path = home.path().join(".akcli/config.toml");
        let valid_config = fs::read(&config_path).unwrap();
        fs::write(&config_path, "[wallet.receive]\nenvironment='unknown'\n").unwrap();
        assert!(receive(&mut stream, &agent, &coordinator, |_| async {
            panic!("invalid config must fail before network")
        })
        .await
        .is_err());
        assert!(!dir.join("metadata.json").exists());
        fs::write(&config_path, valid_config).unwrap();
        let settings = info().0;
        let store = ReceiveStore::open_existing(&agent.home, &name()).unwrap();
        store.initialize(&settings).unwrap();
        fs::write(store.database_path(), b"not a sqlite database").unwrap();
        assert!(
            receive(&mut stream, &agent, &coordinator, |_| async { Ok(info()) })
                .await
                .is_err()
        );
        assert_eq!(
            fs::read(store.database_path()).unwrap(),
            b"not a sqlite database"
        );
        fs::write(
            home.path().join(".akcli/config.toml"),
            "operator = 'invalid'\n",
        )
        .unwrap();
        assert!(receive(&mut stream, &agent, &coordinator, |_| async {
            panic!("invalid URL must be rejected before network")
        })
        .await
        .is_err());
    }

    #[tokio::test]
    async fn sqlite_auxiliaries_are_private_and_failed_insert_returns_no_address() {
        let (_home, agent, coordinator) = receive_fixture();
        let settings = info().0;
        let store = ReceiveStore::open_existing(&agent.home, &name()).unwrap();
        store.initialize(&settings).unwrap();
        let sqlite = SqliteContractStore::new(store.database_path()).unwrap();
        let connection = rusqlite::Connection::open(store.database_path()).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TRIGGER reject_contract BEFORE INSERT ON contracts BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;").unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let path = store
                .database_path()
                .with_file_name(format!("contracts.db{suffix}"));
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let (_client, mut stream) = UnixStream::pair().unwrap();
        assert!(
            receive(&mut stream, &agent, &coordinator, |_| async { Ok(info()) })
                .await
                .is_err()
        );
        assert!(sqlite.list().unwrap().is_empty());
        let wal = store.database_path().with_file_name("contracts.db-wal");
        fs::set_permissions(&wal, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            receive(&mut stream, &agent, &coordinator, |_| async { Ok(info()) })
                .await
                .is_err()
        );
    }

    struct Dropped(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pending_receive_cancels_on_lock_replacement_expiry_stop_and_caller_drop() {
        for mode in 0..7 {
            let (home, agent, coordinator) = receive_fixture();
            let coordinator = if mode == 2 {
                let shorter = Arc::new(Coordinator::new(Duration::from_secs(10)));
                install(&shorter);
                shorter
            } else {
                coordinator
            };
            let (client, mut stream) = UnixStream::pair().unwrap();
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let guard = Dropped(dropped.clone());
            let (started, ready) = tokio::sync::oneshot::channel();
            let shared = coordinator.clone();
            let request = tokio::spawn(async move {
                timeout(
                    Duration::from_secs(25),
                    receive(&mut stream, &agent, &shared, |_| async move {
                        let _guard = guard;
                        let _ = started.send(());
                        std::future::pending::<Result<(crate::config::ReceiveSettings, Info), Error>>().await
                    }),
                )
                .await
                .map_err(|_| Error::Timeout)?
            });
            ready.await.unwrap();
            assert_eq!(
                coordinator.state.lock().unwrap().status(),
                Status::Unlocked(name())
            );
            match mode {
                0 => coordinator.state.lock().unwrap().lock(false),
                1 => install(&coordinator),
                2 => tokio::time::advance(Duration::from_secs(10)).await,
                3 => coordinator.state.lock().unwrap().lock(true),
                4 => request.abort(),
                5 => drop(client),
                6 => tokio::time::advance(Duration::from_secs(25)).await,
                _ => unreachable!(),
            }
            assert!(matches!(request.await, Err(_) | Ok(Err(_))));
            assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
            assert!(coordinator.receive.try_lock().is_ok());
            assert!(!home
                .path()
                .join(".akcli/wallets/alice/metadata.json")
                .exists());
        }
    }

    #[tokio::test]
    async fn receive_operations_serialize_without_blocking_status() {
        let (_home, agent, coordinator) = receive_fixture();
        let (_client1, mut stream1) = UnixStream::pair().unwrap();
        let (_client2, mut stream2) = UnixStream::pair().unwrap();
        let (release, waiting) = tokio::sync::oneshot::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let shared = coordinator.clone();
        let first_agent = agent.clone();
        let first = tokio::spawn(async move {
            receive(&mut stream1, &first_agent, &shared, |_| async move {
                let _ = started.send(());
                waiting.await.unwrap();
                Ok(info())
            })
            .await
        });
        ready.await.unwrap();
        let (started2, mut ready2) = tokio::sync::oneshot::channel();
        let shared = coordinator.clone();
        let second = tokio::spawn(async move {
            receive(&mut stream2, &agent, &shared, |_| async move {
                let _ = started2.send(());
                Ok(info())
            })
            .await
        });
        tokio::task::yield_now().await;
        assert!(matches!(
            ready2.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            coordinator.state.lock().unwrap().status(),
            Status::Unlocked(name())
        );
        let deadline = coordinator.state.lock().unwrap().deadline();
        let (_unlock_client, mut unlock_stream) = UnixStream::pair().unwrap();
        assert!(unlock(&mut unlock_stream, &coordinator, name(), || Err(
            Error::Unlock
        ))
        .await
        .is_err());
        assert_eq!(coordinator.state.lock().unwrap().deadline(), deadline);
        release.send(()).unwrap();
        let address = first.await.unwrap().unwrap();
        assert_eq!(second.await.unwrap().unwrap(), address);
    }

    fn name() -> WalletName {
        WalletName::new("alice").unwrap()
    }
    fn secret() -> Secret {
        Secret::new("synthetic mnemonic".into())
    }
    fn install(coordinator: &Coordinator) {
        let mut state = coordinator.state.lock().unwrap();
        let ticket = state.ticket().unwrap();
        assert!(state.install(ticket, name(), secret()));
        coordinator.changed.notify_one();
    }
    #[tokio::test(start_paused = true)]
    async fn active_timer_drops_secret_without_status_calls() {
        let coordinator = Arc::new(Coordinator::new(Duration::from_secs(10)));
        install(&coordinator);
        let timer = coordinator.clone();
        let task = tokio::spawn(async move { timer.maintain().await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert!(coordinator.state.lock().unwrap().deadline().is_none());
        coordinator.state.lock().unwrap().lock(true);
        coordinator.changed.notify_one();
        task.await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn failed_worker_and_status_never_extend_lease() {
        let coordinator = Coordinator::new(Duration::from_secs(10));
        install(&coordinator);
        let deadline = coordinator.state.lock().unwrap().deadline();
        let (_client, mut server) = UnixStream::pair().unwrap();
        assert!(
            unlock(&mut server, &coordinator, name(), || Err(Error::Unlock))
                .await
                .is_err()
        );
        assert_eq!(
            coordinator.state.lock().unwrap().status(),
            Status::Unlocked(name())
        );
        assert_eq!(coordinator.state.lock().unwrap().deadline(), deadline);
    }
    #[tokio::test]
    async fn cancellation_keeps_actual_worker_permit_and_discards_completion() {
        for disconnect in [false, true] {
            let coordinator = Arc::new(Coordinator::new(Duration::from_secs(10)));
            let (client, mut server) = UnixStream::pair().unwrap();
            let (release, waiting) = std::sync::mpsc::channel();
            let (started, ready) = tokio::sync::oneshot::channel();
            let shared = coordinator.clone();
            let request = tokio::spawn(async move {
                unlock(&mut server, &shared, name(), move || {
                    let _ = started.send(());
                    waiting.recv().unwrap();
                    Ok(secret())
                })
                .await
            });
            ready.await.unwrap();
            if disconnect {
                drop(client);
                let _ = request.await;
            } else {
                request.abort();
                let _ = request.await;
            }
            assert!(coordinator.kdf.clone().try_acquire_owned().is_err());
            release.send(()).unwrap();
            let _finished = coordinator.kdf.acquire().await.unwrap();
            assert_eq!(coordinator.state.lock().unwrap().status(), Status::Locked);
        }
    }
    #[tokio::test(start_paused = true)]
    async fn whole_request_timeout_does_not_free_worker_or_install_late_result() {
        let coordinator = Arc::new(Coordinator::new(Duration::from_secs(10)));
        let (_client, mut server) = UnixStream::pair().unwrap();
        let (release, waiting) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let shared = coordinator.clone();
        let request = tokio::spawn(async move {
            timeout(
                Duration::from_secs(25),
                unlock(&mut server, &shared, name(), move || {
                    let _ = started.send(());
                    waiting.recv().unwrap();
                    Ok(secret())
                }),
            )
            .await
        });
        ready.await.unwrap();
        tokio::time::advance(Duration::from_secs(25)).await;
        assert!(request.await.unwrap().is_err());
        assert!(coordinator.kdf.clone().try_acquire_owned().is_err());
        release.send(()).unwrap();
        let _finished = coordinator.kdf.acquire().await.unwrap();
        assert_eq!(coordinator.state.lock().unwrap().status(), Status::Locked);
    }
}
