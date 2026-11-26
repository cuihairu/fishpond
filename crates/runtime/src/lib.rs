//! fishpond-runtime: actor/process runtime primitives — bounded
//! mailboxes, per-account supervision, cancellation and restarts (F010).
//!
//! One actor per account consumes its mailbox one message at a time. A
//! handler failure ends the actor's current *life*; the supervisor
//! restarts it under a [`supervisor::RestartPolicy`] with state reset,
//! while the mailbox (and everything queued in it) survives across
//! lives. Fatal errors are never restarted. Stopping is cooperative and
//! prompt: the actor exits at the next loop turn, never mid-queue.

pub mod actor;
pub mod mailbox;
pub mod supervisor;
pub mod wire;

pub use actor::{ActorBehavior, ActorError, ActorOutcome};
pub use mailbox::{MailboxError, MailboxPair, MailboxReceiver, MailboxSender};
pub use supervisor::{AccountSupervisor, RestartPolicy};
pub use wire::{IdentityError, sign_envelope, verify_envelope};

#[cfg(test)]
mod tests {
    use super::*;
    use fishpond_domain::{AccountId, UnixMillis};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::time::timeout;

    /// A counter actor that fails on poison values. When a log is
    /// attached, `handle` records the per-life occupancy *before* each
    /// message — a fresh life logs 0 first, a leaked state would log 1.
    #[derive(Default)]
    struct Counter {
        seen: Vec<u32>,
        log: Option<&'static Mutex<Vec<u32>>>,
    }

    impl Counter {
        fn tracking(log: &'static Mutex<Vec<u32>>) -> Self {
            Self {
                seen: Vec::new(),
                log: Some(log),
            }
        }

        fn note(&self) {
            if let Some(book) = self.log
                && let Ok(mut entries) = book.try_lock()
            {
                entries.push(self.seen.len() as u32);
            }
        }
    }

    impl ActorBehavior for Counter {
        type Message = u32;

        fn reset(&mut self) {
            self.seen.clear();
        }

        async fn handle(&mut self, message: u32) -> Result<(), ActorError> {
            if message == u32::MAX {
                self.note();
                Err(ActorError::failed("poison message"))
            } else {
                self.note();
                self.seen.push(message);
                Ok(())
            }
        }
    }

    fn account(tag: &str) -> AccountId {
        AccountId::parse(unique_account(tag)).unwrap()
    }

    fn unique_account(tag: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{tag}-{nanos}")
    }

    fn instant() -> UnixMillis {
        UnixMillis::from_millis(1_000).unwrap()
    }

    #[tokio::test]
    async fn an_actor_processes_its_mailbox_and_stops_on_request() {
        let account = account("clean");
        let mut supervisor = AccountSupervisor::new();
        static LOG: Mutex<Vec<u32>> = Mutex::new(Vec::new());
        let mailbox = supervisor.start(
            account.clone(),
            Counter::tracking(&LOG),
            8,
            RestartPolicy::none(),
        );
        for value in [1, 2, 3] {
            mailbox.send(value).await.unwrap();
        }
        // Stopping only promises the *current* message finishes; queued
        // work may be abandoned. Wait for all three to be observed.
        while LOG.lock().unwrap().len() < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(supervisor.stop(&account), "registered accounts stop");
        let outcome = timeout(Duration::from_secs(2), supervisor.join(&account))
            .await
            .expect("stop is prompt")
            .expect("joined account");
        assert_eq!(outcome, ActorOutcome::Stopped);
        // One life, occupancy 0/1/2 before each of the three messages.
        let log = LOG.lock().unwrap().clone();
        assert_eq!(log, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn a_handler_failure_restarts_with_fresh_state_and_keeps_the_mailbox() {
        static LOG: Mutex<Vec<u32>> = Mutex::new(Vec::new());
        let account = account("restart");
        let mut supervisor = AccountSupervisor::new();
        let mailbox = supervisor.start(
            account.clone(),
            Counter::tracking(&LOG),
            8,
            RestartPolicy {
                max_restarts: 2,
                backoff: Duration::from_millis(5),
                max_backoff: Duration::from_millis(20),
            },
        );
        mailbox.send(1).await.unwrap();
        mailbox.send(u32::MAX).await.unwrap(); // kills life #1
        mailbox.send(2).await.unwrap(); // queued across the restart
        mailbox.send(3).await.unwrap();

        // The restart counter increments as soon as life #1 dies.
        while supervisor.restarts(&account) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Give life #2 time to drain the surviving mailbox.
        tokio::time::sleep(Duration::from_millis(100)).await;
        supervisor.stop(&account);
        // Read the counter and the life log while the actor is still
        // registered — join removes the entry.
        assert_eq!(supervisor.restarts(&account), 1);
        let log = LOG.lock().unwrap().clone();
        // Life #1: message 1 (occupancy 0), then the poison (occupancy 1)
        // kills the life. Life #2 starts *fresh* — its first entry is
        // occupancy 0 again, not the leaked 1 — and takes both queued
        // messages (occupancy 0, then 1).
        assert_eq!(log, vec![0, 1, 0, 1], "state resets per life");
        let outcome = timeout(Duration::from_secs(2), supervisor.join(&account))
            .await
            .expect("stop is prompt")
            .expect("joined account");
        assert_eq!(outcome, ActorOutcome::Stopped);
    }

    #[tokio::test]
    async fn exhausted_restarts_give_up_and_close_the_mailbox() {
        struct AlwaysFails;
        impl ActorBehavior for AlwaysFails {
            type Message = ();
            fn reset(&mut self) {}
            async fn handle(&mut self, (): ()) -> Result<(), ActorError> {
                Err(ActorError::failed("always"))
            }
        }

        let account = account("gaveup");
        let mut supervisor = AccountSupervisor::new();
        let mailbox = supervisor.start(
            account.clone(),
            AlwaysFails,
            2,
            RestartPolicy {
                max_restarts: 2,
                backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(5),
            },
        );
        // Three messages: one per life. Life #1 fails, restart #1;
        // life #2 fails on the queued next message, restart #2; life #3
        // fails and the policy is exhausted.
        for _ in 0..3 {
            mailbox.send(()).await.unwrap();
        }
        // Poll until the policy is exhausted: life #1 fails (restart #1),
        // life #2 fails on the queued message (restart #2), life #3
        // fails and gives up.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while supervisor.restarts(&account) < 2 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "restarts were never exhausted"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // join removes the registration; capture the counter beforehand.
        assert_eq!(
            supervisor.restarts(&account),
            2,
            "two restarts then give up"
        );
        let outcome = timeout(Duration::from_secs(2), supervisor.join(&account))
            .await
            .expect("give-up must finish")
            .expect("joined account");
        assert_eq!(
            outcome,
            ActorOutcome::GaveUp(ActorError::failed("always")),
            "exhausted policy ends in GaveUp"
        );
        assert_eq!(
            mailbox.try_send(()),
            Err(MailboxError::Closed),
            "a given-up actor's mailbox is closed"
        );
    }

    #[tokio::test]
    async fn fatal_errors_are_never_restarted() {
        struct FatalOnFirst;
        impl ActorBehavior for FatalOnFirst {
            type Message = ();
            fn reset(&mut self) {}
            async fn handle(&mut self, (): ()) -> Result<(), ActorError> {
                Err(ActorError::fatal("unrecoverable"))
            }
        }

        let account = account("fatal");
        let mut supervisor = AccountSupervisor::new();
        let mailbox = supervisor.start(
            account.clone(),
            FatalOnFirst,
            2,
            RestartPolicy {
                max_restarts: 9,
                backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(5),
            },
        );
        mailbox.send(()).await.unwrap();
        let outcome = timeout(Duration::from_secs(2), supervisor.join(&account))
            .await
            .expect("fatal must finish promptly")
            .expect("joined account");
        assert_eq!(
            outcome,
            ActorOutcome::Fatal(ActorError::fatal("unrecoverable"))
        );
        assert_eq!(supervisor.restarts(&account), 0, "fatal is never restarted");
        assert_eq!(mailbox.try_send(()), Err(MailboxError::Closed));
    }

    #[tokio::test]
    async fn accounts_are_isolated_and_replaceable() {
        let a = account("iso-a");
        let b = account("iso-b");
        let mut supervisor = AccountSupervisor::new();
        let mailbox_a = supervisor.start(a.clone(), Counter::default(), 4, RestartPolicy::none());
        let mailbox_b = supervisor.start(b.clone(), Counter::default(), 4, RestartPolicy::none());
        assert_eq!(supervisor.len(), 2);
        mailbox_a.send(1).await.unwrap();
        mailbox_b.send(100).await.unwrap();
        assert!(mailbox_a.try_send(2).is_ok());

        // Stopping A leaves B untouched.
        supervisor.stop(&a);
        assert!(supervisor.stop(&a), "stop is idempotent while registered");
        assert!(supervisor.contains(&b));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(supervisor.is_finished(&a));
        assert!(!supervisor.is_finished(&b));

        // Re-registering A replaces the stopped actor with a fresh one.
        let mailbox_a2 = supervisor.start(a.clone(), Counter::default(), 4, RestartPolicy::none());
        assert!(mailbox_a2.try_send(9).is_ok(), "fresh mailbox accepts");
        // The old mailbox is closed by the replacement.
        assert_eq!(mailbox_a.try_send(3), Err(MailboxError::Closed));

        supervisor.stop_all();
        assert!(supervisor.remove(&a));
        assert!(supervisor.remove(&b));
        assert!(supervisor.is_empty());
        assert!(!supervisor.stop(&a), "unknown accounts do not stop");
    }

    #[tokio::test]
    async fn unstarted_accounts_reject_and_join_reports_none() {
        let account = account("ghost");
        let mut supervisor = AccountSupervisor::new();
        assert!(!supervisor.contains(&account));
        assert_eq!(supervisor.restarts(&account), 0);
        assert_eq!(supervisor.join(&account).await, None);
        assert_eq!(instant().as_millis(), 1_000);
    }
}
