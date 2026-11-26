//! F031 initial load test (1,000 Mock sessions, session plane): a
//! thousand supervised session actors, each fed through its bounded
//! mailbox, must all complete without restarts — and a poison-message
//! storm must stay contained (fatal sessions never restart, healthy
//! ones never lose a message). Pure — no database.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fishpond_domain::AccountId;
use fishpond_runtime::{AccountSupervisor, ActorBehavior, ActorError, RestartPolicy};

/// Total handled deliveries across every session.
fn handled_counter() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(0))
}

/// A mock session actor: counts deliveries, dies fatally on poison.
struct SessionActor {
    handled: Arc<AtomicU64>,
    poison_at: Option<u32>,
    seen: u32,
}

impl SessionActor {
    fn healthy(handled: Arc<AtomicU64>) -> Self {
        Self {
            handled,
            poison_at: None,
            seen: 0,
        }
    }

    fn poisoned_at(handled: Arc<AtomicU64>, at: u32) -> Self {
        Self {
            handled,
            poison_at: Some(at),
            seen: 0,
        }
    }
}

impl ActorBehavior for SessionActor {
    type Message = u32;

    fn reset(&mut self) {
        // A fresh life sees no messages yet; the counter survives on
        // purpose (it belongs to the test, not the life).
        self.seen = 0;
    }

    async fn handle(&mut self, seq: u32) -> Result<(), ActorError> {
        self.seen += 1;
        if self.poison_at == Some(seq) {
            return Err(ActorError::fatal("poison message"));
        }
        self.handled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn account(tag: &str, i: usize) -> AccountId {
    AccountId::parse(format!("acc-{tag}-{i}-{}", std::process::id())).expect("account id is valid")
}

const SESSIONS: usize = 1_000;
const MESSAGES_PER_SESSION: u32 = 10;
const CAPACITY: usize = 32;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thousand_sessions_drain_their_mailboxes_without_restarts() {
    let handled = handled_counter();
    let mut supervisor = AccountSupervisor::new();
    let mut senders = Vec::with_capacity(SESSIONS);

    for i in 0..SESSIONS {
        let sender = supervisor.start(
            account("load", i),
            SessionActor::healthy(Arc::clone(&handled)),
            CAPACITY,
            RestartPolicy::standard(),
        );
        senders.push((i, sender));
    }
    assert_eq!(supervisor.len(), SESSIONS);

    // Feed every session; the async `send` waits on backpressure
    // instead of dropping, so nothing is lost by construction.
    for (_i, sender) in &senders {
        for seq in 0..MESSAGES_PER_SESSION {
            sender.send(seq).await.expect("session alive");
        }
    }

    // Dropping the senders closes the mailboxes; each actor drains
    // what it holds and stops cleanly.
    drop(senders);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let done = (0..SESSIONS).all(|i| supervisor.is_finished(&account("load", i)));
        if done || tokio::time::Instant::now() >= deadline {
            assert!(done, "all 1,000 sessions must finish within the deadline");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        handled.load(Ordering::SeqCst),
        u64::from(SESSIONS as u32 * MESSAGES_PER_SESSION),
        "every delivery must be handled exactly once"
    );
    for i in 0..SESSIONS {
        assert_eq!(supervisor.restarts(&account("load", i)), 0, "session {i}");
    }

    supervisor.stop_all();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn poison_storm_stays_contained() {
    let handled = handled_counter();
    let mut supervisor = AccountSupervisor::new();
    let poisoned: std::collections::HashSet<usize> =
        (0..SESSIONS).filter(|i| i % 50 == 0).collect(); // 20 fatal sessions
    let mut senders = Vec::with_capacity(SESSIONS);

    for i in 0..SESSIONS {
        let actor = if poisoned.contains(&i) {
            SessionActor::poisoned_at(Arc::clone(&handled), 3)
        } else {
            SessionActor::healthy(Arc::clone(&handled))
        };
        let sender = supervisor.start(
            account("storm", i),
            actor,
            CAPACITY,
            RestartPolicy::standard(),
        );
        senders.push((i, sender));
    }

    // Feed everything; poisoned sessions die at message 3 (fatal — no
    // restart), so their later sends fail with a closed mailbox and
    // that is the containment boundary.
    for (_i, sender) in &senders {
        for seq in 0..MESSAGES_PER_SESSION {
            // The final 7 sends into a poisoned session hit a closed
            // mailbox; send() reports Closed instead of hanging.
            let _ = sender.send(seq).await;
        }
    }
    drop(senders);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let done = (0..SESSIONS).all(|i| supervisor.is_finished(&account("storm", i)));
        if done || tokio::time::Instant::now() >= deadline {
            assert!(done, "all sessions (healthy and fatal) must conclude");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Healthy: 980 sessions × 10; fatal: 20 sessions × 3 before the
    // poison (message 3 itself is never counted).
    let healthy = (SESSIONS - poisoned.len()) as u64 * u64::from(MESSAGES_PER_SESSION);
    let fatal = poisoned.len() as u64 * 3;
    assert_eq!(handled.load(Ordering::SeqCst), healthy + fatal);

    // Fatal sessions never restart; healthy sessions never restart.
    for i in 0..SESSIONS {
        assert_eq!(supervisor.restarts(&account("storm", i)), 0, "session {i}");
    }
    supervisor.stop_all();
}
