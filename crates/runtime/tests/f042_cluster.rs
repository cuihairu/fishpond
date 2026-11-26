//! F042 cluster load (10,000 simulated sessions, session plane): ten
//! thousand supervised actors — ten times the F031 face — must all
//! drain exactly once with zero restarts, and a poison storm ten
//! times wider (200 fatal sessions) must stay contained. Pure — no
//! database; the point is the supervisor's scaling, not the engine.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fishpond_domain::AccountId;
use fishpond_runtime::{AccountSupervisor, ActorBehavior, ActorError, RestartPolicy};

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

/// Per-run unique via the pid (stable within the process, so start
/// and later lookups resolve the same id — same idiom as f031).
fn account(tag: &str, i: usize) -> AccountId {
    AccountId::parse(format!("acc-f042-{tag}-{i}-{}", std::process::id()))
        .expect("account id is valid")
}

const SESSIONS: usize = 10_000;
const MESSAGES_PER_SESSION: u32 = 10;
const CAPACITY: usize = 64;

/// All ten thousand sessions started, fed ten messages each, drained
/// exactly once, never restarted — then quiesced within the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_thousand_sessions_drain_without_restarts() {
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
    assert_eq!(supervisor.len(), SESSIONS, "the whole fleet is registered");

    for (_i, sender) in &senders {
        for seq in 0..MESSAGES_PER_SESSION {
            sender.send(seq).await.expect("session alive");
        }
    }
    drop(senders);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let mut done = true;
        for i in 0..SESSIONS {
            if !supervisor.is_finished(&account("load", i)) {
                done = false;
                break;
            }
        }
        if done || tokio::time::Instant::now() >= deadline {
            assert!(done, "all 10,000 sessions must finish within the deadline");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        handled.load(Ordering::SeqCst),
        u64::from(SESSIONS as u32) * u64::from(MESSAGES_PER_SESSION),
        "every delivery must be handled exactly once"
    );
    for i in 0..SESSIONS {
        assert_eq!(supervisor.restarts(&account("load", i)), 0, "session {i}");
    }
    supervisor.stop_all();
}

/// Ten-times-wider poison storm: 200 fatal sessions die at message 3
/// and never restart; the 9,800 healthy ones lose nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_thousand_session_poison_storm_stays_contained() {
    let handled = handled_counter();
    let mut supervisor = AccountSupervisor::new();
    let poisoned: std::collections::HashSet<usize> =
        (0..SESSIONS).filter(|i| i % 50 == 0).collect(); // 200 fatal
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

    for (_i, sender) in &senders {
        for seq in 0..MESSAGES_PER_SESSION {
            let _ = sender.send(seq).await;
        }
    }
    drop(senders);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let mut done = true;
        for i in 0..SESSIONS {
            if !supervisor.is_finished(&account("storm", i)) {
                done = false;
                break;
            }
        }
        if done || tokio::time::Instant::now() >= deadline {
            assert!(done, "all sessions (healthy and fatal) must conclude");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let healthy = (SESSIONS - poisoned.len()) as u64 * u64::from(MESSAGES_PER_SESSION);
    let fatal = poisoned.len() as u64 * 3;
    assert_eq!(handled.load(Ordering::SeqCst), healthy + fatal);
    for i in 0..SESSIONS {
        assert_eq!(supervisor.restarts(&account("storm", i)), 0, "session {i}");
    }
    supervisor.stop_all();
}
