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
        let output = Command::new(env!("CARGO_BIN_EXE_akcli"))
            .env_clear()
            .args(["--wallet-agent-internal", "ensure"])
            .arg(path)
            .arg(seconds.to_string())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        Self { home, agent }
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

// Real SDK transport, local HTTP/2 + official protobuf only; no operator or funds.
async fn mock_info() -> (String, tokio::task::JoinHandle<()>) {
    use prost::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(socket).await.unwrap();
            let (request, mut respond) = connection.accept().await.unwrap().unwrap();
            assert_eq!(request.uri().path(), "/ark.v1.ArkService/GetInfo");
            let signer: bitcoin::secp256k1::PublicKey =
                "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
                    .parse()
                    .unwrap();
            let message = ark_grpc::test_utils::GetInfoResponse {
                signer_pubkey: signer.to_string(),
                forfeit_pubkey: signer.to_string(),
                forfeit_address: bitcoin::Address::p2tr(
                    &bitcoin::secp256k1::Secp256k1::new(),
                    signer.x_only_public_key().0,
                    None,
                    bitcoin::Network::Regtest,
                )
                .to_string(),
                network: "regtest".into(),
                unilateral_exit_delay: 144,
                boarding_exit_delay: 144,
                ..Default::default()
            }
            .encode_to_vec();
            let mut body = vec![0];
            body.extend_from_slice(&(message.len() as u32).to_be_bytes());
            body.extend_from_slice(&message);
            let mut response = respond
                .send_response(
                    http::Response::builder()
                        .header("content-type", "application/grpc")
                        .body(())
                        .unwrap(),
                    false,
                )
                .unwrap();
            response.send_data(body.into(), false).unwrap();
            let mut trailers = http::HeaderMap::new();
            trailers.insert("grpc-status", "0".parse().unwrap());
            response.send_trailers(trailers).unwrap();
            // Keep polling the connection until the one-request SDK client drops it.
            while connection.accept().await.is_some() {}
        }
    });
    (endpoint, task)
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
    let (endpoint, mock) = mock_info().await;
    std::fs::write(
        fixture.home.path().join(".akcli/config.toml"),
        format!("[wallet.receive]\nenvironment='regtest'\ngrpc_endpoint='{endpoint}'\n"),
    )
    .unwrap();
    let first = receive_frame(&fixture).await;
    assert_eq!(&first[..2], &[1, 5]);
    let address = std::str::from_utf8(&first[2..]).unwrap();
    assert!(ark_core::ArkAddress::decode(address).is_ok());
    assert_eq!(receive_frame(&fixture).await, first);
    mock.await.unwrap();
    fixture.agent.lock().await.unwrap();
    assert_eq!(receive_frame(&fixture).await, [1, 4]);
}

#[tokio::test]
async fn receive_ipc_network_wait_keeps_controls_responsive_and_lock_cancels() {
    let fixture = Fixture::new(60);
    fixture.create("alice");
    fixture.unlock("alice").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::fs::write(
        fixture.home.path().join(".akcli/config.toml"),
        format!(
            "[wallet.receive]\nenvironment='regtest'\ngrpc_endpoint='http://{}'\n",
            listener.local_addr().unwrap()
        ),
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
