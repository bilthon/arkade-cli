use super::Status;
use crate::wallet::{Secret, WalletName};
use std::time::Duration;
use tokio::time::Instant;

/// Only the coordinator owns a retained mnemonic. Blocking workers return results
/// to their request task, never mutate this state themselves.
pub(super) struct State {
    retained: Option<(WalletName, Secret, Instant)>,
    generation: u64,
    session: tokio::sync::watch::Sender<u64>,
    stopped: bool,
    lease: Duration,
}
impl State {
    pub(super) fn new(lease: Duration) -> Self {
        Self {
            retained: None,
            generation: 0,
            session: tokio::sync::watch::channel(0).0,
            stopped: false,
            lease,
        }
    }
    pub(super) fn expire(&mut self) {
        if self.deadline().is_some_and(|d| Instant::now() >= d) {
            self.retained = None;
            self.invalidate_session();
        }
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.retained.as_ref().map(|r| r.2)
    }
    pub(super) fn status(&mut self) -> Status {
        self.expire();
        self.retained
            .as_ref()
            .map_or(Status::Locked, |r| Status::Unlocked(r.0.clone()))
    }
    pub(super) fn ticket(&self) -> Option<u64> {
        (!self.stopped).then_some(self.generation)
    }
    pub(super) fn install(&mut self, ticket: u64, name: WalletName, mnemonic: Secret) -> bool {
        self.expire();
        if self.ticket() != Some(ticket) {
            return false;
        }
        self.invalidate_session();
        self.retained = Some((name, mnemonic, Instant::now() + self.lease));
        true
    }
    fn invalidate_session(&self) {
        self.session.send_modify(|id| *id = id.wrapping_add(1));
    }
    pub(super) fn receive_session(
        &mut self,
    ) -> Option<(WalletName, Instant, tokio::sync::watch::Receiver<u64>)> {
        self.expire();
        self.retained
            .as_ref()
            .map(|(name, _, deadline)| (name.clone(), *deadline, self.session.subscribe()))
    }
    pub(super) fn mnemonic(&mut self, session: u64) -> Option<&Secret> {
        self.expire();
        if *self.session.borrow() != session {
            return None;
        }
        self.retained.as_ref().map(|r| &r.1)
    }
    pub(super) fn lock(&mut self, stop: bool) {
        self.invalidate_session();
        self.generation = self.generation.wrapping_add(1);
        self.retained = None;
        self.stopped |= stop;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn install(state: &mut State, name: &str) {
        assert!(state.install(
            state.ticket().unwrap(),
            WalletName::new(name).unwrap(),
            Secret::new("synthetic mnemonic".into())
        ));
    }
    #[tokio::test(start_paused = true)]
    async fn expiry_invalidates_receive_but_not_in_flight_unlock_ticket() {
        let mut state = State::new(Duration::from_secs(10));
        install(&mut state, "alice");
        let (_, _, mut receive) = state.receive_session().unwrap();
        let identity = *receive.borrow_and_update();
        let ticket = state.ticket().unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        state.expire();
        assert!(receive.has_changed().unwrap());
        assert!(state.mnemonic(identity).is_none());
        assert!(state.install(
            ticket,
            WalletName::new("bob").unwrap(),
            Secret::new("replacement".into())
        ));
        assert_eq!(
            state.status(),
            Status::Unlocked(WalletName::new("bob").unwrap())
        );
        assert!(state.mnemonic(identity).is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn exact_expiry_status_failure_and_replacement() {
        let mut state = State::new(Duration::from_secs(10));
        install(&mut state, "alice");
        tokio::time::advance(Duration::from_secs(9)).await;
        assert_eq!(
            state.status(),
            Status::Unlocked(WalletName::new("alice").unwrap())
        );
        // Failed worker results do not invoke install, and status does not renew.
        tokio::time::advance(Duration::from_secs(1)).await;
        state.expire();
        assert!(state.retained.is_none());
        install(&mut state, "alice");
        tokio::time::advance(Duration::from_secs(9)).await;
        install(&mut state, "bob");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            state.status(),
            Status::Unlocked(WalletName::new("bob").unwrap())
        );
        tokio::time::advance(Duration::from_secs(9)).await;
        assert_eq!(state.status(), Status::Locked);
    }
    #[tokio::test(start_paused = true)]
    async fn lock_stop_defeat_deferred_results_and_restart_is_locked() {
        let mut state = State::new(Duration::from_secs(10));
        for stop in [false, true] {
            let ticket = state.ticket().unwrap();
            install(&mut state, "alice");
            state.lock(stop);
            assert!(!state.install(
                ticket,
                WalletName::new("bob").unwrap(),
                Secret::new("late".into())
            ));
            assert_eq!(state.status(), Status::Locked);
        }
        assert!(state.ticket().is_none());
        assert_eq!(State::new(Duration::from_secs(10)).status(), Status::Locked);
    }
}
