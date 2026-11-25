//! Actors: message-driven tasks with supervised lives.
//!
//! One actor consumes its bounded mailbox one message at a time. A
//! handler failure ends the current *life*; the supervisor decides
//! between restart (with [`ActorBehavior::reset`] re-initializing state)
//! and giving up. Cancellation is cooperative through a shutdown signal:
//! the actor finishes (or abandons) the in-flight message and exits at
//! the next loop turn, so stopping never races the mailbox.

use tokio::sync::{mpsc, watch};

/// Why an actor's life ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorError {
    /// Restartable failure (transient upstream error, poison message, ...).
    Failed {
        /// Human-readable cause.
        reason: String,
    },
    /// Terminal failure; the supervisor must not restart.
    Fatal {
        /// Human-readable cause (never contains secrets).
        reason: String,
    },
}

impl ActorError {
    /// Builds an [`ActorError::Failed`].
    pub fn failed(reason: impl Into<String>) -> Self {
        Self::Failed {
            reason: reason.into(),
        }
    }

    /// Builds an [`ActorError::Fatal`].
    pub fn fatal(reason: impl Into<String>) -> Self {
        Self::Fatal {
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ActorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed { reason } => write!(f, "actor failed: {reason}"),
            Self::Fatal { reason } => write!(f, "actor failed fatally: {reason}"),
        }
    }
}

impl std::error::Error for ActorError {}

/// The behavior of one actor: what it does with a message and how a
/// fresh life begins.
pub trait ActorBehavior: Send + 'static {
    /// The message type this actor consumes (the mailbox's element type).
    type Message: Send + 'static;

    /// Re-initializes in-place state. Called before *every* life,
    /// including the first — constructors only allocate, [`Self::reset`]
    /// defines what a fresh life looks like.
    fn reset(&mut self);

    /// Handles one message. `Err(ActorError::Failed{..})` ends the life
    /// and may restart; `Err(ActorError::Fatal{..})` ends the actor.
    fn handle(
        &mut self,
        message: Self::Message,
    ) -> impl std::future::Future<Output = Result<(), ActorError>> + Send;
}

/// How a single life ended (internal).
#[derive(Debug, PartialEq)]
enum Life {
    /// Shutdown signal or mailbox exhaustion — stop cleanly.
    Stopped,
    /// The handler returned an error.
    Ended(ActorError),
}

/// How the whole supervised actor ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorOutcome {
    /// Stopped on request (or every sender dropped).
    Stopped,
    /// Exhausted the restart policy; the last failure is reported.
    GaveUp(ActorError),
    /// A fatal error ended the actor; never restarted.
    Fatal(ActorError),
}

/// The shutdown signal shared by supervisor and actor.
pub(crate) type Shutdown = watch::Receiver<bool>;

/// Runs one life: dispatch messages until a failure, a shutdown signal
/// or a closed mailbox ends the loop.
async fn run_life<B: ActorBehavior>(
    behavior: &mut B,
    inbox: &mut mpsc::Receiver<B::Message>,
    mut shutdown: Shutdown,
) -> Life {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                // An `Err` means the supervisor itself is gone; either
                // way no more messages can be observed.
                if changed.is_err() || *shutdown.borrow_and_update() {
                    return Life::Stopped;
                }
            }
            message = inbox.recv() => {
                let Some(message) = message else {
                    return Life::Stopped;
                };
                if let Err(error) = behavior.handle(message).await {
                    return Life::Ended(error);
                }
            }
        }
    }
}

/// Supervises one actor: fresh lives with backoff until stopped, fatal,
/// or out of restarts. The mailbox receiver lives here, across restarts,
/// so messages enqueued during a crashed restart window wait in the
/// bounded box instead of being lost.
pub(crate) async fn supervise<B: ActorBehavior>(
    mut behavior: B,
    mut inbox: mpsc::Receiver<B::Message>,
    mut shutdown: watch::Receiver<bool>,
    policy: crate::supervisor::RestartPolicy,
    restarts: std::sync::Arc<std::sync::atomic::AtomicU32>,
) -> ActorOutcome {
    let mut attempt = 0u32;
    loop {
        behavior.reset();
        match run_life(&mut behavior, &mut inbox, shutdown.clone()).await {
            Life::Stopped => return ActorOutcome::Stopped,
            Life::Ended(ActorError::Fatal { reason }) => {
                return ActorOutcome::Fatal(ActorError::Fatal { reason });
            }
            Life::Ended(ActorError::Failed { reason }) => {
                attempt += 1;
                // The counter counts restarts that actually happened, so
                // it only moves when the policy allows one more.
                match policy.restart_delay(attempt) {
                    Some(delay) => {
                        restarts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Back off, but keep honoring shutdown.
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            changed = shutdown.changed() => {
                                if changed.is_err() || *shutdown.borrow_and_update() {
                                    return ActorOutcome::Stopped;
                                }
                            }
                        }
                    }
                    None => {
                        return ActorOutcome::GaveUp(ActorError::Failed { reason });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A behavior that answers every message and counts them, failing on
    /// poison values.
    #[derive(Default)]
    struct Counter {
        seen: Vec<u32>,
    }

    impl ActorBehavior for Counter {
        type Message = u32;

        fn reset(&mut self) {
            self.seen.clear();
        }

        async fn handle(&mut self, message: u32) -> Result<(), ActorError> {
            if message == u32::MAX {
                Err(ActorError::failed("poison message"))
            } else {
                self.seen.push(message);
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn a_life_processes_messages_until_the_mailbox_drains() {
        let (tx, mut rx) = mpsc::channel(8);
        let (_shutdown_tx, shutdown) = watch::channel(false);
        let mut counter = Counter::default();
        for value in [1, 2, 3] {
            tx.send(value).await.unwrap();
        }
        drop(tx);
        let outcome = run_life(&mut counter, &mut rx, shutdown).await;
        assert_eq!(outcome, Life::Stopped);
        assert_eq!(counter.seen, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn a_poison_message_ends_the_life_with_a_failure() {
        let (tx, mut rx) = mpsc::channel(8);
        let (_shutdown_tx, shutdown) = watch::channel(false);
        let mut counter = Counter::default();
        tx.send(u32::MAX).await.unwrap();
        let outcome = run_life(&mut counter, &mut rx, shutdown).await;
        assert_eq!(outcome, Life::Ended(ActorError::failed("poison message")));
    }

    #[tokio::test]
    async fn the_shutdown_signal_ends_the_life_immediately() {
        let (_tx, mut rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown) = watch::channel(false);
        let mut counter = Counter::default();
        shutdown_tx.send(true).unwrap();
        let outcome = run_life(&mut counter, &mut rx, shutdown).await;
        assert_eq!(outcome, Life::Stopped);
    }
}
