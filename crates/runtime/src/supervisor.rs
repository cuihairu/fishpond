//! Supervision: restart policies and per-account actor ownership.
//!
//! Every account runs inside its own supervised actor. A restart resets
//! the actor's state (via [`ActorBehavior::reset`]) but keeps its mailbox,
//! so work queued while the actor was crashing is not lost. The
//! supervisor never restarts a fatal failure or more than the configured
//! number of restartable ones.

use fishpond_domain::AccountId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::actor::{ActorBehavior, ActorOutcome, supervise};
use crate::mailbox::{MailboxPair, MailboxSender};

/// How many times a restartable failure may be retried, and with what
/// delay. `max_restarts = 0` means a single life: the first failure is
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Maximum number of restarts before giving up.
    pub max_restarts: u32,
    /// Backoff before restart #1; restart #n waits `n * delay` (capped).
    pub backoff: Duration,
    /// Upper bound for the linear backoff (prevents unbounded waits).
    pub max_backoff: Duration,
}

impl RestartPolicy {
    /// A test-friendly policy: three restarts, 10/20/30 ms backoff.
    pub fn standard() -> Self {
        Self {
            max_restarts: 3,
            backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(100),
        }
    }

    /// Never restart (the first restartable failure ends the actor).
    pub fn none() -> Self {
        Self {
            max_restarts: 0,
            backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
        }
    }

    /// Delay before restart number `attempt` (1-based), or `None` when
    /// restarts are exhausted.
    pub fn restart_delay(&self, attempt: u32) -> Option<Duration> {
        if attempt == 0 || attempt > self.max_restarts {
            return None;
        }
        Some(self.backoff.saturating_mul(attempt).min(self.max_backoff))
    }
}

/// Control block for one supervised actor (its typed mailbox sender
/// belongs to the caller, not the supervision bookkeeping).
struct ActorHandle {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<ActorOutcome>,
    restarts: Arc<AtomicU32>,
}

impl ActorHandle {
    /// Asks the actor to stop after its current message; idempotent.
    fn stop(&self) {
        let _ = self.shutdown.send(true);
    }
}

/// The per-account actor supervisor.
#[derive(Default)]
pub struct AccountSupervisor {
    actors: HashMap<AccountId, ActorHandle>,
}

impl AccountSupervisor {
    /// Creates an empty supervisor (policies are chosen per start).
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether an actor is registered for `account` (registration stays
    /// until [`Self::remove`], even after the actor stopped).
    pub fn contains(&self, account: &AccountId) -> bool {
        self.actors.contains_key(account)
    }

    /// The number of registered actors.
    pub fn len(&self) -> usize {
        self.actors.len()
    }

    /// Whether no actors are registered.
    pub fn is_empty(&self) -> bool {
        self.actors.is_empty()
    }

    /// Restarts observed for `account` so far (0 for unknown accounts).
    pub fn restarts(&self, account: &AccountId) -> u32 {
        self.actors
            .get(account)
            .map(|handle| handle.restarts.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Whether the supervised task of `account` has finished.
    pub fn is_finished(&self, account: &AccountId) -> bool {
        self.actors
            .get(account)
            .is_some_and(|handle| handle.task.is_finished())
    }

    /// Spawns a supervised actor for `account` and returns the typed
    /// sender end of its bounded mailbox. Starting an account that is
    /// already registered stops the old actor first (replacement, not an
    /// error — duplicate starts are an operator decision, and the old
    /// mailbox closes so its senders learn about it).
    pub fn start<B>(
        &mut self,
        account: AccountId,
        behavior: B,
        capacity: usize,
        policy: RestartPolicy,
    ) -> MailboxSender<B::Message>
    where
        B: ActorBehavior,
    {
        if let Some(old) = self.actors.remove(&account) {
            old.stop();
        }
        let (sender, receiver) = MailboxPair::<B::Message>::bounded(capacity).split();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let restarts = Arc::new(AtomicU32::new(0));
        let task_restarts = Arc::clone(&restarts);
        let task = tokio::spawn(supervise(
            behavior,
            receiver.into_inner(),
            shutdown_rx,
            policy,
            task_restarts,
        ));
        self.actors.insert(
            account,
            ActorHandle {
                shutdown: shutdown_tx,
                task,
                restarts,
            },
        );
        sender
    }

    /// Requests a graceful stop of one account's actor; reports whether
    /// it was registered.
    pub fn stop(&self, account: &AccountId) -> bool {
        match self.actors.get(account) {
            Some(handle) => {
                handle.stop();
                true
            }
            None => false,
        }
    }

    /// Stops every registered actor (daemon shutdown path).
    pub fn stop_all(&self) {
        for handle in self.actors.values() {
            handle.stop();
        }
    }

    /// Stops and removes one account's actor; reports whether it was
    /// registered. The final outcome is dropped (stop implies
    /// [`ActorOutcome::Stopped`] in the absence of a concurrent failure).
    pub fn remove(&mut self, account: &AccountId) -> bool {
        match self.actors.remove(account) {
            Some(handle) => {
                handle.stop();
                true
            }
            None => false,
        }
    }

    /// Waits for the final outcome of `account`'s actor, removing the
    /// registration; `None` when unknown.
    pub async fn join(&mut self, account: &AccountId) -> Option<ActorOutcome> {
        match self.actors.remove(account) {
            Some(handle) => Some(handle.task.await.unwrap_or(ActorOutcome::Stopped)),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_policy_is_bounded_and_gives_none_when_exhausted() {
        let policy = RestartPolicy::standard();
        assert_eq!(policy.restart_delay(0), None);
        assert_eq!(policy.restart_delay(1), Some(Duration::from_millis(10)));
        assert_eq!(policy.restart_delay(3), Some(Duration::from_millis(30)));
        assert_eq!(policy.restart_delay(4), None);
        assert_eq!(RestartPolicy::none().restart_delay(1), None);
    }
}
