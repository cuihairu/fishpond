//! SSE subscription sessions (F019): per-consumer event streaming with
//! permission gating, replay from the log, and slow-consumer handling.
//!
//! The HTTP/SSE socket layer lands with the REST work (F025); this is
//! the session machinery it will drive, deterministic and mock-only:
//!
//! * **权限** — opening a session requires the F007 `events:read`
//!   scope on the caller's resolved key; the gate is fail-closed and
//!   the session is bound to the key's tenant, so a valid key simply
//!   cannot stream another tenant's log.
//! * **回放 / 续订** — a session streams the tenant log from a cursor:
//!   a live subscribe starts at the log head ("from now", or at the
//!   stored checkpoint when that is further ahead), and the client's
//!   explicit `Last-Event-ID` opens replay (the client is
//!   authoritative for history). Pages are bounded and the log is the
//!   truth, so replay never skips.
//! * **慢消费者** — live pages check the consumer's lag (log head
//!   minus cursor) before fetching. Past [`StreamPolicy::lag_horizon`]
//!   the session reports [`StreamError::SlowConsumer`] with the
//!   `Last-Event-ID` to reconnect at: the server drops the connection
//!   instead of buffering without bound. Replay sessions are exempt —
//!   they asked for history and drain it page by page.
//!
//! Like the rest of the crate: explicit instants, no ambient state,
//! tenant-bound surfaces with row-level security underneath.

use fishpond_domain::{EventEnvelope, TenantId, UnixMillis};
use fishpond_storage::{
    ResolvedKey, SCOPE_EVENTS_READ, StorageError, StorageResult, Store, require_scope,
};

/// Per-session streaming policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPolicy {
    /// Upper bound on how many events one page buffers (the SSE
    /// server's per-flush batch size).
    pub buffer_max: usize,
    /// How many undelivered sequences a consumer may lag the log head
    /// before the session is declared slow and dropped.
    pub lag_horizon: u64,
}

impl StreamPolicy {
    /// Validates the policy: both bounds must be positive.
    pub fn new(buffer_max: usize, lag_horizon: u64) -> StorageResult<Self> {
        if buffer_max == 0 {
            return Err(StorageError::Invalid {
                what: "stream policy",
                reason: "buffer_max must be positive".to_owned(),
            });
        }
        if lag_horizon == 0 {
            return Err(StorageError::Invalid {
                what: "stream policy",
                reason: "lag_horizon must be positive".to_owned(),
            });
        }
        Ok(Self {
            buffer_max,
            lag_horizon,
        })
    }
}

/// Why a stream operation failed. Error text carries scopes and
/// sequence numbers only — never payload content or credentials.
#[derive(Debug)]
pub enum StreamError {
    /// The key lacks `events:read` (the F007 fail-closed gate).
    ScopeDenied {
        /// The missing scope name.
        scope: &'static str,
    },
    /// The consumer lags too far: drop the connection; the client
    /// reconnects with `Last-Event-ID: resume_from`.
    SlowConsumer {
        /// The `Last-Event-ID` to reconnect with: the last sequence
        /// the session had delivered (the server resumes at the next
        /// one).
        resume_from: u64,
        /// How far behind the consumer was.
        lag: u64,
    },
    /// The store refused an operation.
    Storage(StorageError),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::ScopeDenied { scope } => {
                write!(f, "stream requires the {scope} scope")
            }
            StreamError::SlowConsumer { resume_from, lag } => write!(
                f,
                "slow consumer {lag} sequences behind; reconnect with last-event-id {resume_from}"
            ),
            StreamError::Storage(err) => write!(f, "event store error: {err}"),
        }
    }
}

impl std::error::Error for StreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StreamError::Storage(err) => Some(err),
            _ => None,
        }
    }
}

impl From<StorageError> for StreamError {
    fn from(err: StorageError) -> Self {
        StreamError::Storage(err)
    }
}

/// One bounded page of the stream.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamPage {
    /// The events delivered in this page (≤ the policy's buffer).
    pub events: Vec<EventEnvelope>,
    /// The cursor after this page (also the persisted checkpoint).
    pub cursor: u64,
    /// Whether the cursor has reached the log head.
    pub caught_up: bool,
}

/// One consumer's live SSE session: a cursor into its tenant log.
/// The manual `Debug` renders identity fields only — never payloads.
pub struct StreamSession {
    store: Store,
    tenant: TenantId,
    consumer: String,
    cursor: u64,
    policy: StreamPolicy,
    /// Opened with an explicit `Last-Event-ID` (replay mode): the
    /// client asked for history, so the lag drop never applies.
    replay: bool,
}

impl std::fmt::Debug for StreamSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamSession")
            .field("tenant", &self.tenant.as_str())
            .field("consumer", &self.consumer)
            .field("cursor", &self.cursor)
            .field("replay", &self.replay)
            .finish()
    }
}

impl StreamSession {
    /// The session's tenant.
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The consumer this session streams for.
    pub fn consumer(&self) -> &str {
        &self.consumer
    }

    /// The current cursor (the last sequence delivered to the socket).
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Streams the next bounded page from the log. Advances (and
    /// persists) the consumer's checkpoint to the page's last sequence,
    /// so the lag always means "undelivered backlog".
    ///
    /// Live sessions check the lag *before* fetching: a consumer more
    /// than [`StreamPolicy::lag_horizon`] sequences behind the head is
    /// dropped (fail with [`StreamError::SlowConsumer`], cursor
    /// untouched) instead of having the server buffer unboundedly.
    /// Replay sessions — opened with an explicit `Last-Event-ID` — are
    /// exempt: they asked for history and drain it page by page.
    pub async fn page(&mut self, now: UnixMillis) -> Result<StreamPage, StreamError> {
        let head = self.store.latest_seq(&self.tenant).await?;
        if !self.replay {
            let lag = head.saturating_sub(self.cursor);
            if lag > self.policy.lag_horizon {
                return Err(StreamError::SlowConsumer {
                    resume_from: self.cursor,
                    lag,
                });
            }
        }
        let events = self
            .store
            .list_events(
                &self.tenant,
                self.cursor,
                u32::try_from(self.policy.buffer_max).unwrap_or(u32::MAX),
            )
            .await?;
        if let Some(last) = events.last() {
            self.cursor = last.seq;
            self.store
                .advance_checkpoint(&self.tenant, &self.consumer, self.cursor, now)
                .await?;
        }
        Ok(StreamPage {
            caught_up: self.cursor >= head,
            cursor: self.cursor,
            events,
        })
    }
}

/// Opens SSE sessions for tenants, gated by F007 scopes.
pub struct SseStreams {
    store: Store,
    policy: StreamPolicy,
}

impl SseStreams {
    /// Wraps a store with a streaming policy.
    pub fn new(store: Store, policy: StreamPolicy) -> Self {
        Self { store, policy }
    }

    /// The underlying store (for callers that compose other flows
    /// around the same connection pool).
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The configured policy.
    pub fn policy(&self) -> StreamPolicy {
        self.policy
    }

    /// Opens a session for `consumer` in the key's tenant. Gate 1 is
    /// the F007 scope check (fail-closed). The cursor comes from the
    /// client's `last_event_id` when present (reconnect/replay — the
    /// client is authoritative), else it is a live tail starting at
    /// the log head or the consumer's stored checkpoint, whichever is
    /// further ahead.
    pub async fn open(
        &self,
        key: &ResolvedKey,
        consumer: &str,
        last_event_id: Option<u64>,
    ) -> Result<StreamSession, StreamError> {
        require_scope(key, SCOPE_EVENTS_READ).map_err(|_| StreamError::ScopeDenied {
            scope: SCOPE_EVENTS_READ,
        })?;
        // Live tail (no Last-Event-ID): start at the log head or the
        // stored checkpoint, whichever is further — a fresh subscribe
        // means "from now", a returning consumer picks up its backlog
        // if it fits the horizon. Replay is always an explicit
        // Last-Event-ID.
        let cursor = match last_event_id {
            Some(seq) => seq,
            None => {
                let checkpoint = self.store.checkpoint(&key.tenant_id, consumer).await?;
                let head = self.store.latest_seq(&key.tenant_id).await?;
                checkpoint.max(head)
            }
        };
        Ok(StreamSession {
            store: self.store.clone(),
            tenant: key.tenant_id.clone(),
            consumer: consumer.to_owned(),
            cursor,
            policy: self.policy,
            replay: last_event_id.is_some(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{StreamError, StreamPolicy};
    use fishpond_storage::StorageError;

    #[test]
    fn policy_rejects_zero_bounds() {
        assert!(matches!(
            StreamPolicy::new(0, 10),
            Err(StorageError::Invalid {
                what: "stream policy",
                ..
            })
        ));
        assert!(matches!(
            StreamPolicy::new(10, 0),
            Err(StorageError::Invalid {
                what: "stream policy",
                ..
            })
        ));
        assert!(StreamPolicy::new(8, 64).is_ok());
    }

    #[test]
    fn slow_consumer_error_names_the_resume_point() {
        let err = StreamError::SlowConsumer {
            resume_from: 42,
            lag: 100,
        };
        let text = err.to_string();
        assert!(text.contains("42") && text.contains("100"), "{text}");
        assert!(!text.to_lowercase().contains("payload"), "{text}");
        assert_eq!(
            StreamError::ScopeDenied {
                scope: "events:read"
            }
            .to_string(),
            "stream requires the events:read scope"
        );
    }
}
