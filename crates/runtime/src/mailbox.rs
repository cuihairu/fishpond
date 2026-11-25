//! Bounded mailboxes: fixed-capacity queues between a supervisor and its
//! actor, with observable occupancy and two send disciplines.
//!
//! `try_send` fails immediately when the box is full — the caller sees
//! backpressure and decides (drop, retry, shed). `send` awaits capacity —
//! the caller propagates pressure upstream. Both keep the bound: an actor
//! can never queue unbounded work behind itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

/// Occupancy counter shared by the sender and the receiver.
type Occupancy = Arc<AtomicUsize>;

/// A capacity-bounded mailbox split into its two ends.
pub struct MailboxPair<M> {
    capacity: usize,
    occupancy: Occupancy,
    sender: mpsc::Sender<M>,
    receiver: mpsc::Receiver<M>,
}

impl<M> MailboxPair<M> {
    /// Creates a mailbox that holds at most `capacity` queued messages.
    pub fn bounded(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        Self {
            capacity: capacity.max(1),
            occupancy: Arc::new(AtomicUsize::new(0)),
            sender,
            receiver,
        }
    }

    /// The configured capacity (a zero request becomes a box of one).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Splits into `(sender, receiver)`. The receiver is the actor's end
    /// and is intentionally not cloneable; senders are.
    pub fn split(self) -> (MailboxSender<M>, MailboxReceiver<M>) {
        (
            MailboxSender {
                inner: self.sender,
                occupancy: Arc::clone(&self.occupancy),
            },
            MailboxReceiver {
                inner: self.receiver,
                occupancy: self.occupancy,
            },
        )
    }
}

/// The producer end of a bounded mailbox.
#[derive(Clone)]
pub struct MailboxSender<M> {
    inner: mpsc::Sender<M>,
    occupancy: Occupancy,
}

impl<M> MailboxSender<M> {
    /// Enqueues without waiting; [`MailboxError::Full`] when the bound is
    /// reached (exact under sequential use; approximate under concurrent
    /// senders).
    pub fn try_send(&self, message: M) -> Result<(), MailboxError> {
        self.inner.try_send(message).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => MailboxError::Full,
            mpsc::error::TrySendError::Closed(_) => MailboxError::Closed,
        })?;
        self.occupancy.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Enqueues, waiting for capacity; [`MailboxError::Closed`] once the
    /// receiver is gone (or the actor stopped).
    pub async fn send(&self, message: M) -> Result<(), MailboxError> {
        self.inner
            .send(message)
            .await
            .map_err(|_| MailboxError::Closed)?;
        self.occupancy.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Approximate number of queued-and-unhandled messages.
    pub fn occupancy(&self) -> usize {
        self.occupancy.load(Ordering::Relaxed)
    }

    /// The configured capacity.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }
}

/// The actor's end of a bounded mailbox (not cloneable — exactly one
/// consumer per mailbox, across restarts).
pub struct MailboxReceiver<M> {
    inner: mpsc::Receiver<M>,
    occupancy: Occupancy,
}

impl<M> MailboxReceiver<M> {
    /// Dequeues the next message, or `None` once every sender is gone.
    pub async fn recv(&mut self) -> Option<M> {
        let message = self.inner.recv().await?;
        self.occupancy.fetch_sub(1, Ordering::Relaxed);
        Some(message)
    }

    /// Hands the raw receiver to a long-lived consumer (the supervised
    /// actor loop), keeping the occupancy counter wired.
    pub(crate) fn into_inner(self) -> mpsc::Receiver<M> {
        self.inner
    }
}

/// Mailbox failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxError {
    /// The bound is reached; nothing was enqueued.
    Full,
    /// The receiver is gone; nothing was enqueued.
    Closed,
}

impl std::fmt::Display for MailboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "mailbox full"),
            Self::Closed => write!(f, "mailbox closed"),
        }
    }
}

impl std::error::Error for MailboxError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_bound_rejects_and_frees_on_recv() {
        let pair = MailboxPair::<u32>::bounded(2);
        let (sender, mut receiver) = pair.split();
        sender.try_send(1).unwrap();
        sender.try_send(2).unwrap();
        assert_eq!(sender.try_send(3), Err(MailboxError::Full));
        assert_eq!(receiver.recv().await, Some(1));
        sender.try_send(3).unwrap();
        assert_eq!(receiver.recv().await, Some(2));
        assert_eq!(receiver.recv().await, Some(3));
        assert_eq!(sender.occupancy(), 0);
    }

    #[tokio::test]
    async fn send_waits_for_capacity_instead_of_failing() {
        let pair = MailboxPair::<u32>::bounded(1);
        let (sender, mut receiver) = pair.split();
        sender.try_send(1).unwrap();
        let waiter = tokio::spawn({
            let sender = sender.clone();
            async move { sender.send(2).await }
        });
        // The waiter cannot finish while the box is full.
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        assert_eq!(receiver.recv().await, Some(1));
        waiter.await.unwrap().unwrap();
        assert_eq!(receiver.recv().await, Some(2));
    }

    #[tokio::test]
    async fn dropped_receivers_close_the_mailbox() {
        let pair = MailboxPair::<u32>::bounded(4);
        let (sender, receiver) = pair.split();
        drop(receiver);
        assert_eq!(sender.try_send(1), Err(MailboxError::Closed));
        assert_eq!(sender.send(1).await, Err(MailboxError::Closed));
    }
}
