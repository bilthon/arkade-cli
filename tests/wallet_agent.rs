#![cfg(unix)]

use arkade_cli::wallet::{
    agent::{Agent, Options, Status},
    Secret, WalletName, WalletStore,
};
use std::{
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixStream as StdStream,
    },
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

const MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
struct Fixture {
    home: tempfile::TempDir,
    agent: Agent,
}
impl Fixture {
    fn new(seconds: u64) -> Self {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let path = home.path().canonicalize().unwrap();
        let agent = Agent::new(
            path.clone(),
            Options::new(Duration::from_secs(seconds)).unwrap(),
        )
        .unwrap();
        let fixture = Self { home, agent };
        fixture.start(seconds);
        fixture
    }
    fn start(&self, seconds: u64) {
        let output = Command::new(env!("CARGO_BIN_EXE_akcli"))
            .env_clear()
            .args(["--wallet-agent-internal", "ensure"])
            .arg(self.home.path().canonicalize().unwrap())
            .arg(seconds.to_string())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    async fn cli_receive(&self, json: bool) -> std::process::Output {
        let home = self.home.path().to_owned();
        tokio::task::spawn_blocking(move || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_akcli"));
            command
                .env_clear()
                .env("HOME", home)
                .args(["wallet", "receive"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if json {
                command.arg("--json");
            }
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(35);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("receive CLI timed out");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap()
    }
    fn socket(&self) -> PathBuf {
        self.home.path().join(".akcli/run/agent.sock")
    }
    fn create(&self, name: &str) {
        WalletStore::open(self.home.path())
            .unwrap()
            .create(
                &WalletName::new(name).unwrap(),
                Secret::new(MNEMONIC.into()),
                Secret::new("test password".into()),
            )
            .unwrap();
    }
    async fn unlock(&self, name: &str) -> Status {
        self.agent
            .unlock(
                &WalletName::new(name).unwrap(),
                Secret::new("test password".into()),
            )
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(mut stream) = StdStream::connect(self.socket()) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let _ = stream.write_all(&[0, 0, 0, 2, 1, 4]);
            let mut ack = [0; 6];
            let _ = stream.read_exact(&mut ack);
        }
    }
}

// Real SDK REST conversion against synthetic localhost data; no operator or funds.
fn info_body(network: &str) -> serde_json::Value {
    let signer: bitcoin::secp256k1::PublicKey =
        "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
            .parse()
            .unwrap();
    let sdk_network: ark_core::server::Network = network.parse().unwrap();
    serde_json::json!({
        "version": "synthetic", "signerPubkey": signer.to_string(),
        "forfeitPubkey": signer.to_string(),
        "forfeitAddress": bitcoin::Address::p2tr(
            &bitcoin::secp256k1::Secp256k1::new(), signer.x_only_public_key().0,
            None, bitcoin::Network::from(sdk_network)).to_string(),
        "network": network, "checkpointTapscript": "",
        "unilateralExitDelay": "144", "boardingExitDelay": "144",
        "sessionDuration": "10", "dust": "330",
        "maxTxWeight": "0", "maxOpReturnOutputs": "0"
    })
}
async fn mock_info(requests: usize) -> (String, wiremock::MockServer) {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/info"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(info_body("regtest")))
        .expect(requests as u64)
        .mount(&server)
        .await;
    (server.uri(), server)
}
async fn receive_frame(fixture: &Fixture) -> Vec<u8> {
    let mut stream = UnixStream::connect(fixture.socket()).await.unwrap();
    stream.write_all(&[0, 0, 0, 2, 1, 5]).await.unwrap();
    let length = stream.read_u32().await.unwrap();
    assert!(length <= 514);
    let mut response = vec![0; length as usize];
    stream.read_exact(&mut response).await.unwrap();
    response
}
#[tokio::test]
async fn receive_ipc_uses_real_sdk_local_info_and_reuses_persisted_address() {
    let fixture = Fixture::new(60);
    fixture.create("alice");
    fixture.unlock("alice").await;
    let (endpoint, mock) = mock_info(2).await;
    std::fs::write(
        fixture.home.path().join(".akcli/config.toml"),
        format!("operator='{endpoint}'\n"),
    )
    .unwrap();
    let first = receive_frame(&fixture).await;
    assert_eq!(&first[..2], &[1, 5]);
    let address = std::str::from_utf8(&first[2..]).unwrap();
    assert!(ark_core::ArkAddress::decode(address).is_ok());
    assert_eq!(receive_frame(&fixture).await, first);
    mock.verify().await;
    fixture.agent.lock().await.unwrap();
    assert_eq!(receive_frame(&fixture).await, [1, 4]);
}

#[tokio::test]
async fn receive_cli_reuses_persisted_contract_and_fails_closed() {
    let fixture = Fixture::new(60);
    let wallets = fixture.home.path().join(".akcli/wallets");
    let locked = fixture.cli_receive(false).await;
    assert_eq!(locked.status.code(), Some(1));
    assert!(locked.stdout.is_empty());
    assert!(!wallets.exists());
    assert!(!fixture.home.path().join(".akcli/config.toml").exists());

    fixture.create("alice");
    fixture.unlock("alice").await;
    let config = fixture.home.path().join(".akcli/config.toml");
    std::fs::write(&config, "operator=42\n").unwrap();
    let invalid = fixture.cli_receive(false).await;
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    assert!(!wallets.join("alice/metadata.json").exists());
    assert!(!wallets.join("alice/contracts.db").exists());

    let (endpoint, mock) = mock_info(3).await;
    std::fs::write(&config, format!("operator='{endpoint}'\n")).unwrap();
    let first = fixture.cli_receive(false).await;
    assert!(first.status.success(), "{first:?}");
    let text = String::from_utf8(first.stdout).unwrap();
    let address = text
        .strip_prefix("Address: ")
        .unwrap()
        .strip_suffix('\n')
        .unwrap();
    assert!(ark_core::ArkAddress::decode(address).is_ok());
    assert_eq!(address, "tark1qpumuen7l8wthtz45p3ftn58pvrs9xlumvkuu2xet8egzkcklqte3egkjh5ls9mvgks7uczg0xv5s4pgkqt6ufmrt3qx0cfym0ja4y7t4pgslm");
    assert!(wallets.join("alice/contracts.db").is_file());
    let metadata = std::fs::read(wallets.join("alice/metadata.json")).unwrap();
    let repeated = fixture.cli_receive(true).await;
    assert!(repeated.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&repeated.stdout).unwrap(),
        serde_json::json!({"address": address})
    );

    fixture.agent.stop().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.socket().exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let absent = fixture.cli_receive(false).await;
    assert_eq!(absent.status.code(), Some(1));
    assert!(absent.stdout.is_empty());
    assert!(!fixture.socket().exists());
    fixture.start(60);
    fixture.unlock("alice").await;
    let restarted = fixture.cli_receive(false).await;
    assert!(restarted.status.success());
    assert_eq!(restarted.stdout, text.as_bytes());
    assert_eq!(
        std::fs::read(wallets.join("alice/metadata.json")).unwrap(),
        metadata
    );

    mock.verify().await;
    mock.reset().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/info"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(info_body("bitcoin")))
        .mount(&mock)
        .await;
    let database = std::fs::read(wallets.join("alice/contracts.db")).unwrap();
    let mismatch = fixture.cli_receive(false).await;
    assert_eq!(mismatch.status.code(), Some(1));
    assert!(mismatch.stdout.is_empty());
    assert_eq!(String::from_utf8(mismatch.stderr).unwrap(), "error: unsafe or incompatible receive storage binding; check operator and wallet association\n");
    assert_eq!(
        std::fs::read(wallets.join("alice/metadata.json")).unwrap(),
        metadata
    );
    assert_eq!(
        std::fs::read(wallets.join("alice/contracts.db")).unwrap(),
        database
    );
}

#[tokio::test]
async fn receive_rest_errors_are_static_and_never_initialize_storage() {
    let fixture = Fixture::new(60);
    fixture.create("alice");
    fixture.unlock("alice").await;
    let config = fixture.home.path().join(".akcli/config.toml");
    let missing = fixture.cli_receive(false).await;
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        missing.stderr,
        b"error: set operator in .akcli/config.toml to receive\n"
    );
    assert!(missing.stdout.is_empty());
    assert_eq!(receive_frame(&fixture).await, [1, 6]);
    std::fs::write(&config, "operator='http://secret-marker@localhost'\n").unwrap();
    assert_eq!(receive_frame(&fixture).await, [1, 7]);

    let mock = wiremock::MockServer::start().await;
    std::fs::write(&config, format!("operator='{}'", mock.uri())).unwrap();
    let mut invalid = info_body("regtest");
    invalid["forfeitPubkey"] = "secret-marker".into();
    let mut invalid_network = info_body("regtest");
    invalid_network["network"] = "secret-marker".into();
    for (response, tag) in [
        (
            wiremock::ResponseTemplate::new(503).set_body_string("secret-marker"),
            9,
        ),
        (
            wiremock::ResponseTemplate::new(200).set_body_string("secret-marker"),
            10,
        ),
        (
            wiremock::ResponseTemplate::new(200).set_body_json(invalid),
            10,
        ),
        (
            wiremock::ResponseTemplate::new(200).set_body_json(invalid_network),
            10,
        ),
        (
            wiremock::ResponseTemplate::new(200)
                .set_body_string("secret-marker")
                .set_delay(Duration::from_secs(15)),
            8,
        ),
    ] {
        mock.reset().await;
        wiremock::Mock::given(wiremock::matchers::path("/v1/info"))
            .respond_with(response)
            .mount(&mock)
            .await;
        assert_eq!(receive_frame(&fixture).await, [1, tag]);
        let dir = fixture.home.path().join(".akcli/wallets/alice");
        assert!(!dir.join("metadata.json").exists());
        assert!(!dir.join("contracts.db").exists());
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    std::fs::write(&config, format!("operator='http://{address}'")).unwrap();
    assert_eq!(receive_frame(&fixture).await, [1, 8]);
}

#[tokio::test]
async fn receive_preserves_mutinynet_binding_and_rejects_url_and_legacy_changes() {
    let fixture = Fixture::new(60);
    fixture.create("alice");
    fixture.unlock("alice").await;
    let mock = wiremock::MockServer::start().await;
    let config = fixture.home.path().join(".akcli/config.toml");
    std::fs::write(&config, format!("operator='{}'", mock.uri())).unwrap();
    wiremock::Mock::given(wiremock::matchers::path("/v1/info"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(info_body("mutinynet")))
        .mount(&mock)
        .await;
    assert_eq!(&receive_frame(&fixture).await[..2], &[1, 5]);
    let dir = fixture.home.path().join(".akcli/wallets/alice");
    let metadata = std::fs::read(dir.join("metadata.json")).unwrap();
    let database = std::fs::read(dir.join("contracts.db")).unwrap();
    let mut binding: serde_json::Value = serde_json::from_slice(&metadata).unwrap();
    assert_eq!(binding["version"], 2);
    assert_eq!(binding["environment"], "mutinynet");
    assert_eq!(binding["operator_rest_url"], mock.uri());
    mock.reset().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/info"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(info_body("signet")))
        .mount(&mock)
        .await;
    assert_eq!(receive_frame(&fixture).await, [1, 11]);
    assert_eq!(std::fs::read(dir.join("metadata.json")).unwrap(), metadata);
    assert_eq!(std::fs::read(dir.join("contracts.db")).unwrap(), database);

    let other = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/info"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(info_body("mutinynet")))
        .mount(&other)
        .await;
    std::fs::write(&config, format!("operator='{}'", other.uri())).unwrap();
    assert_eq!(receive_frame(&fixture).await, [1, 11]);
    assert_eq!(std::fs::read(dir.join("metadata.json")).unwrap(), metadata);
    assert_eq!(std::fs::read(dir.join("contracts.db")).unwrap(), database);
    binding["version"] = 1.into();
    binding["grpc_endpoint"] = binding["operator_rest_url"].take();
    binding.as_object_mut().unwrap().remove("operator_rest_url");
    let legacy = serde_json::to_vec(&binding).unwrap();
    std::fs::write(dir.join("metadata.json"), &legacy).unwrap();
    assert_eq!(receive_frame(&fixture).await, [1, 11]);
    assert_eq!(std::fs::read(dir.join("metadata.json")).unwrap(), legacy);
    assert_eq!(std::fs::read(dir.join("contracts.db")).unwrap(), database);
}

#[tokio::test]
async fn receive_ipc_network_wait_keeps_controls_responsive_and_lock_cancels() {
    let fixture = Fixture::new(60);
    fixture.create("alice");
    fixture.unlock("alice").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::fs::write(
        fixture.home.path().join(".akcli/config.toml"),
        format!("operator='http://{}'\n", listener.local_addr().unwrap()),
    )
    .unwrap();
    let mut stream = UnixStream::connect(fixture.socket()).await.unwrap();
    stream.write_all(&[0, 0, 0, 2, 1, 5]).await.unwrap();
    let (_socket, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), fixture.agent.status())
            .await
            .unwrap()
            .unwrap(),
        Status::Unlocked(WalletName::new("alice").unwrap())
    );
    tokio::time::timeout(Duration::from_secs(1), fixture.agent.lock())
        .await
        .unwrap()
        .unwrap();
    let mut response = [0; 6];
    tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response, [0, 0, 0, 2, 1, 4]);
    assert!(!fixture
        .home
        .path()
        .join(".akcli/wallets/alice/metadata.json")
        .exists());
}

#[tokio::test]
async fn balance_absent_is_non_creating_and_real_ipc_is_typed_and_fail_closed() {
    use arkade_cli::wallet::{agent::Error, balance::Balance};
    use ark_client::{ContractStore, SqliteContractStore};
    use wiremock::{matchers::path, Mock, ResponseTemplate};

    let absent_home = tempfile::tempdir_in("/tmp").unwrap();
    let absent = Agent::new(absent_home.path().canonicalize().unwrap(), Options::default()).unwrap();
    assert!(matches!(absent.balance().await, Err(Error::Absent)));
    assert!(!absent_home.path().join(".akcli").exists());

    let fixture = Fixture::new(60);
    assert!(matches!(fixture.agent.balance().await, Err(Error::BalanceSession)));
    assert!(!fixture.home.path().join(".akcli/wallets").exists());
    fixture.create("alice");
    fixture.unlock("alice").await;
    assert!(matches!(fixture.agent.balance().await, Err(Error::BalanceMissingOperator)));
    let mock = wiremock::MockServer::start().await;
    let config = fixture.home.path().join(".akcli/config.toml");
    std::fs::write(&config, format!("operator='{}'", mock.uri())).unwrap();
    Mock::given(path("/v1/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(info_body("regtest")))
        .mount(&mock).await;
    assert!(matches!(fixture.agent.balance().await, Err(Error::BalanceMissingState)));
    let dir = fixture.home.path().join(".akcli/wallets/alice");
    assert!(!dir.join("contracts.db").exists());
    fixture.agent.receive().await.unwrap();
    let store = SqliteContractStore::new(dir.join("contracts.db")).unwrap();
    let before = store.list().unwrap();
    let script = before[0].script_pubkey.to_hex_string();
    let record = |id, amount: u64| serde_json::json!({
        "outpoint": {"txid": "11".repeat(32), "vout": id},
        "script": script, "amount": amount.to_string(),
        "createdAt": "1", "expiresAt": "4102444800"
    });
    for mode in ["zero", "success", "invalid", "overflow", "duplicate", "query", "operator"] {
        mock.reset().await;
        Mock::given(path("/v1/info"))
            .respond_with(if mode == "operator" {
                ResponseTemplate::new(503).set_body_string("private-backend-marker")
            } else {
                ResponseTemplate::new(200).set_body_json(info_body("regtest"))
            }).mount(&mock).await;
        let records = match mode {
            "zero" => vec![],
            "overflow" => vec![record(0, u64::MAX), record(1, 1000)],
            "duplicate" => vec![record(0, 1000), record(0, 1000)],
            "invalid" => {
                let mut invalid = record(0, 1000);
                invalid["amount"] = "private-backend-marker".into();
                vec![invalid]
            }
            _ => vec![record(0, 1000)],
        };
        Mock::given(path("/v1/indexer/vtxos"))
            .respond_with(if mode == "query" {
                ResponseTemplate::new(500).set_body_string("private-backend-marker")
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "vtxos": records, "page": {"next": 0, "total": 0}
                }))
            }).mount(&mock).await;
        let result = fixture.agent.balance().await;
        match mode {
            "zero" => assert_eq!(result.unwrap(), Balance::default()),
            "success" => assert_eq!(result.unwrap(), Balance {
                offchain_balance_sats: 1000, confirmed_sats: 1000, ..Balance::default()
            }),
            "invalid" | "overflow" => assert!(matches!(result, Err(Error::BalanceInvalidResponse))),
            "duplicate" | "query" => assert!(matches!(result, Err(Error::BalanceQuery))),
            "operator" => assert!(matches!(result, Err(Error::BalanceOperator))),
            _ => unreachable!(),
        }
    }
    assert_eq!(store.list().unwrap(), before);
    std::fs::write(&config, "operator='http://private-marker@localhost'").unwrap();
    assert!(matches!(fixture.agent.balance().await, Err(Error::BalanceConfig)));
    fixture.agent.lock().await.unwrap();
    assert!(matches!(fixture.agent.balance().await, Err(Error::BalanceSession)));
    fixture.agent.stop().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.socket().exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(matches!(fixture.agent.balance().await, Err(Error::Absent)));
}

#[test]
fn agent_requires_absolute_home_and_bounded_lease() {
    assert!(Agent::new(PathBuf::from("relative"), Options::default()).is_err());
    let home = tempfile::tempdir_in("/tmp").unwrap();
    assert!(Agent::new(home.path().canonicalize().unwrap(), Options::default()).is_ok());
    assert!(Options::new(Duration::ZERO).is_err());
    assert!(Options::new(Duration::from_millis(1500)).is_err());
    assert_eq!(Options::default().lease(), Duration::from_secs(600));
    assert!(Options::new(Duration::from_secs(86_401)).is_err());
}

#[tokio::test]
async fn private_socket_real_uid_and_global_lock() {
    let fixture = Fixture::new(600);
    for (path, mode) in [
        (".akcli/run", 0o700),
        (".akcli/run/agent.sock", 0o600),
        (".akcli/run/agent.lock", 0o600),
    ] {
        let metadata = std::fs::symlink_metadata(fixture.home.path().join(path)).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, mode);
        assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
    }
    let stream = UnixStream::connect(fixture.socket()).await.unwrap();
    assert_eq!(
        stream.peer_cred().unwrap().uid(),
        rustix::process::geteuid().as_raw()
    );
    fixture.create("alice");
    assert_eq!(
        fixture.unlock("alice").await,
        Status::Unlocked(WalletName::new("alice").unwrap())
    );
    fixture.agent.lock().await.unwrap();
    assert_eq!(fixture.agent.status().await.unwrap(), Status::Locked);
}

#[tokio::test]
async fn failed_unlock_preserves_without_renewing_and_success_replaces() {
    let fixture = Fixture::new(5);
    fixture.create("alice");
    fixture.create("bob");
    fixture.unlock("alice").await;
    let start = Instant::now();
    let error = fixture
        .agent
        .unlock(
            &WalletName::new("bob").unwrap(),
            Secret::new("wrong".into()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "unable to unlock wallet");
    assert_eq!(
        fixture.agent.status().await.unwrap(),
        Status::Unlocked(WalletName::new("alice").unwrap())
    );
    tokio::time::sleep(
        Duration::from_secs(5).saturating_sub(start.elapsed()) + Duration::from_millis(100),
    )
    .await;
    assert_eq!(fixture.agent.status().await.unwrap(), Status::Locked);
    fixture.unlock("alice").await;
    assert_eq!(
        fixture.unlock("bob").await,
        Status::Unlocked(WalletName::new("bob").unwrap())
    );
}

async fn send_frame(stream: &mut UnixStream, body: &[u8]) {
    stream.write_u32(body.len() as u32).await.unwrap();
    stream.write_all(body).await.unwrap();
}
fn unlock_body(name: &str) -> Vec<u8> {
    let mut body = vec![1, 2, name.len() as u8];
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(&13u16.to_be_bytes());
    body.extend_from_slice(b"test password");
    body
}

#[tokio::test]
async fn malformed_truncated_oversized_and_slow_frames_are_bounded() {
    let fixture = Fixture::new(600);
    for body in [
        vec![2, 1],
        vec![1, 99],
        vec![1, 1, 0],
        vec![1, 2, 1, b'A', 0, 1, b'x'],
        vec![1, 2, 1, b'a', 0, 2, b'x'],
    ] {
        let mut stream = UnixStream::connect(fixture.socket()).await.unwrap();
        send_frame(&mut stream, &body).await;
        let mut reply = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut reply))
            .await
            .unwrap();
        assert!(reply.is_empty());
    }
    let mut stream = UnixStream::connect(fixture.socket()).await.unwrap();
    stream.write_u32(u32::MAX).await.unwrap();
    let mut reply = [0; 6];
    assert!(stream.read_exact(&mut reply).await.is_err());
    let mut stream = UnixStream::connect(fixture.socket()).await.unwrap();
    stream.write_u32(20).await.unwrap();
    stream.write_all(&[1, 2]).await.unwrap();
    stream.shutdown().await.unwrap();
    assert!(stream.read_exact(&mut reply).await.is_err());
    let mut stream = UnixStream::connect(fixture.socket()).await.unwrap();
    let start = Instant::now();
    stream.write_all(&[0]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    stream.write_all(&[0]).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut reply))
            .await
            .unwrap()
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(fixture.agent.status().await.unwrap(), Status::Locked);
}

#[tokio::test]
async fn running_kdf_does_not_block_lock_and_disconnected_results_stay_invalid() {
    let fixture = Fixture::new(600);
    fixture.create("alice");
    let mut request = UnixStream::connect(fixture.socket()).await.unwrap();
    send_frame(&mut request, &unlock_body("alice")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Production W1 KDF is deliberately not weakened in tests.
    let start = Instant::now();
    fixture.agent.lock().await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    let mut response = Vec::new();
    request.read_to_end(&mut response).await.unwrap();
    assert!(!response
        .windows(MNEMONIC.len())
        .any(|b| b == MNEMONIC.as_bytes()));
    assert_eq!(fixture.agent.status().await.unwrap(), Status::Locked);
    let mut request = UnixStream::connect(fixture.socket()).await.unwrap();
    send_frame(&mut request, &unlock_body("alice")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(request);
    let error = fixture
        .agent
        .unlock(
            &WalletName::new("alice").unwrap(),
            Secret::new("test password".into()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "agent is busy");
    // Poll busy with a nonexistent wallet (no additional KDF) until the worker
    // really finished. The permit must not be released by disconnecting.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let error = fixture
            .agent
            .unlock(&WalletName::new("absent").unwrap(), Secret::new("x".into()))
            .await
            .unwrap_err();
        if error.to_string() != "agent is busy" {
            break;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(fixture.agent.status().await.unwrap(), Status::Locked);
}

#[tokio::test]
async fn connection_limit_rejects_excess_then_recovers_after_deadlines() {
    let fixture = Fixture::new(600);
    let mut idle = Vec::new();
    for _ in 0..32 {
        let mut stream = UnixStream::connect(fixture.socket()).await.unwrap();
        stream.write_all(&[0]).await.unwrap();
        idle.push(stream);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut extra = UnixStream::connect(fixture.socket()).await.unwrap();
    let mut byte = [0];
    let closed = tokio::time::timeout(Duration::from_secs(1), extra.read(&mut byte))
        .await
        .unwrap();
    assert!(closed.is_err() || closed.unwrap() == 0);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(fixture.agent.status().await.unwrap(), Status::Locked);
}

#[tokio::test]
async fn expiry_remains_active_during_kdf_and_stop_invalidates_worker() {
    let fixture = Fixture::new(1);
    fixture.create("alice");
    fixture.unlock("alice").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let mut request = UnixStream::connect(fixture.socket()).await.unwrap();
    send_frame(&mut request, &unlock_body("alice")).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(fixture.agent.status().await.unwrap(), Status::Locked);
    let start = Instant::now();
    fixture.agent.stop().await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    let mut response = Vec::new();
    let _ = request.read_to_end(&mut response).await;
    assert!(response.is_empty());
}
