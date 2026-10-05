use super::{
    protocol::{self, Request},
    runtime::{self, Runtime},
    state::State,
    Agent, Error, Status,
};
use crate::wallet::{Secret, WalletName, WalletStore};
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
}
impl Coordinator {
    fn new(lease: Duration) -> Self {
        Self {
            state: Mutex::new(State::new(lease)),
            changed: Notify::new(),
            kdf: Arc::new(Semaphore::new(1)),
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
