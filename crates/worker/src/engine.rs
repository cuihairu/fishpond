//! Dual-engine selection and browser backpressure (F024): the
//! daemon-side seam that routes one capability call across the two
//! engines — the API engine (the F016 [`CapabilityRouter`], gated and
//! breaker-protected) and the browser engine (the F021/F023 Node
//! page worker, whose wire bridge lands with the control plane).
//!
//! Downgrade policy (按能力降级), in preference order:
//!
//! 1. **API lane** — route through the capability router. Its
//!    fail-closed gates are terminal (scope/config problems must not
//!    silently degrade onto a page browser); only a lane that cannot
//!    serve the capability (manifest lacks it) or an upstream fault
//!    degrades to the next lane.
//! 2. **Browser lane** — only when the browser engine declares the
//!    capability. Caller faults pass through untouched (a bad
//!    argument is bad on every engine); transient page faults are
//!    terminal once this lane is the last one.
//!
//! Browser backpressure (T25) mirrors the Node [`ResourceQueue`]
//! contract at the daemon boundary: `max_concurrent` slots run
//! immediately, the next `max_pending` wait in FIFO order, and
//! anything beyond that is rejected *now* with
//! [`EngineError::Backpressured`] naming the capacity — an unbounded
//! wait queue is how a stalled browser eats a daemon.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fishpond_provider_sdk::{Capability, ProtocolVersion, Provider, ProviderError};
use fishpond_router::CapabilityRouter;
use fishpond_storage::ResolvedKey;
use tokio::sync::{Semaphore, TryAcquireError};

/// Why a browser-lane attempt failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserFault {
    /// A page/upstream problem; another engine may still serve the
    /// call (downgrade).
    Transient {
        /// Secret-free reason text (no page content).
        reason: String,
    },
    /// The operation arguments are wrong; no engine can do better.
    CallerFault {
        /// Secret-free reason text (no page content).
        reason: String,
    },
}

/// The browser lane's reach, decided by the engine handle the daemon
/// holds (the Node bridge supplies this descriptor once it lands).
#[derive(Debug, Clone, Copy)]
pub struct BrowserLane {
    /// Capabilities the browser engine can serve.
    pub capabilities: &'static [Capability],
    /// Whether the engine is up (registered, queue alive). A down
    /// engine is skipped, not degraded onto.
    pub available: bool,
}

impl BrowserLane {
    /// Whether this lane serves `capability`.
    pub fn serves(&self, capability: Capability) -> bool {
        self.available && self.capabilities.contains(&capability)
    }
}

/// Why a dual-engine call failed.
#[derive(Debug)]
pub enum EngineError {
    /// A router gate refused the call (scope, unroutable capability,
    /// platform config). Terminal by design: fail closed, never
    /// degrade an authorization problem onto a browser.
    Route(fishpond_router::RouteError),
    /// The API engine's upstream fault (and no lane could serve).
    Upstream(ProviderError),
    /// The browser lane failed transiently with no lane left.
    Browser {
        /// Secret-free reason text (no page content).
        reason: String,
    },
    /// The operation arguments are wrong on every engine.
    CallerFault {
        /// Secret-free reason text (no page content).
        reason: String,
    },
    /// The browser gate rejected the call at the pending bound.
    Backpressured {
        /// The pending capacity that was exceeded.
        capacity: usize,
    },
    /// No registered lane serves the capability.
    NoEngine {
        /// The capability nobody could serve.
        capability: Capability,
    },
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Route(err) => write!(f, "route refused: {err}"),
            EngineError::Upstream(err) => write!(f, "api engine fault: {err}"),
            EngineError::Browser { reason } => write!(f, "browser engine fault: {reason}"),
            EngineError::CallerFault { reason } => write!(f, "caller fault: {reason}"),
            EngineError::Backpressured { capacity } => {
                write!(f, "browser queue full (pending capacity {capacity})")
            }
            EngineError::NoEngine { capability } => {
                write!(f, "no engine serves {}", capability.as_str())
            }
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EngineError::Route(err) => Some(err),
            EngineError::Upstream(err) => Some(err),
            _ => None,
        }
    }
}

impl From<fishpond_router::RouteError> for EngineError {
    fn from(err: fishpond_router::RouteError) -> Self {
        EngineError::Route(err)
    }
}

/// The daemon-side browser queue bound (T25). `max_concurrent` slots
/// run immediately; up to `max_pending` further callers wait FIFO on
/// a freed slot; everyone else is rejected with
/// [`EngineError::Backpressured`] naming the capacity.
#[derive(Debug)]
pub struct BackpressureGate {
    permits: Arc<Semaphore>,
    pending: AtomicUsize,
    max_pending: usize,
}

impl BackpressureGate {
    /// Builds the gate; both bounds must be positive (zero means
    /// "never run anything", which is a configuration error).
    pub fn new(max_concurrent: usize, max_pending: usize) -> Option<Self> {
        if max_concurrent == 0 || max_pending == 0 {
            return None;
        }
        Some(Self {
            permits: Arc::new(Semaphore::new(max_concurrent)),
            pending: AtomicUsize::new(0),
            max_pending,
        })
    }

    /// The pending capacity (named in rejections).
    pub fn capacity(&self) -> usize {
        self.max_pending
    }

    /// How many callers are currently queued behind a full slot set.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    /// Runs `work` under the bound: immediate when a slot is free,
    /// queued while pending has room, rejected past the bound.
    pub async fn run<T, Work>(&self, work: Work) -> Result<T, EngineError>
    where
        Work: Future<Output = T>,
    {
        let _permit = match self.permits.try_acquire() {
            Ok(permit) => permit,
            Err(TryAcquireError::Closed) => {
                return Err(EngineError::Backpressured {
                    capacity: self.max_pending,
                });
            }
            Err(TryAcquireError::NoPermits) => {
                let slot = self.pending.fetch_add(1, Ordering::SeqCst) + 1;
                if slot > self.max_pending {
                    self.pending.fetch_sub(1, Ordering::SeqCst);
                    return Err(EngineError::Backpressured {
                        capacity: self.max_pending,
                    });
                }
                let permit =
                    self.permits
                        .acquire()
                        .await
                        .map_err(|_| EngineError::Backpressured {
                            capacity: self.max_pending,
                        })?;
                self.pending.fetch_sub(1, Ordering::SeqCst);
                permit
            }
        };
        Ok(work.await)
    }
}

/// One capability call addressed to the dual-engine selector: the
/// routing inputs the API lane shares with [`CapabilityRouter::route`].
pub struct EngineRequest<'a> {
    /// The caller's resolved key (scopes, tenant, expiry).
    pub key: &'a ResolvedKey,
    /// The platform the call targets.
    pub platform: &'a str,
    /// The capability to serve.
    pub capability: Capability,
    /// Wire protocol versions the caller accepts.
    pub offered: &'a [ProtocolVersion],
    /// Decision instant (millis since epoch).
    pub now: i64,
}

/// The dual-engine selector: owns the API router, the browser lane
/// descriptor and the browser gate.
pub struct EngineSelector {
    api: CapabilityRouter,
    browser: Option<BrowserLane>,
    gate: BackpressureGate,
}

impl EngineSelector {
    /// Builds the selector. `browser: None` (or an unavailable lane)
    /// means every call is API-only.
    pub fn new(
        api: CapabilityRouter,
        browser: Option<BrowserLane>,
        gate: BackpressureGate,
    ) -> Self {
        Self { api, browser, gate }
    }

    /// The browser gate (for saturation diagnostics).
    pub fn gate(&self) -> &BackpressureGate {
        &self.gate
    }

    /// Routes one capability call across the lanes per the downgrade
    /// policy. `api_call` receives the router-selected provider (the
    /// F016 [`CapabilityRouter::execute`] closure shape);
    /// `browser_call` runs under the backpressure gate.
    pub async fn execute<T, ApiCall, BrowserCall>(
        &self,
        request: EngineRequest<'_>,
        ctx: &fishpond_provider_sdk::CallContext,
        api_call: ApiCall,
        browser_call: BrowserCall,
    ) -> Result<T, EngineError>
    where
        ApiCall: for<'a> Fn(
            &'a dyn Provider,
            &'a fishpond_provider_sdk::CallContext,
        )
            -> Pin<Box<dyn Future<Output = Result<T, ProviderError>> + Send + 'a>>,
        BrowserCall: for<'a> Fn(
            &'a fishpond_provider_sdk::CallContext,
        ) -> Pin<
            Box<dyn Future<Output = Result<T, BrowserFault>> + Send + 'a>,
        >,
    {
        let EngineRequest {
            key,
            platform,
            capability,
            offered,
            now,
        } = request;
        let mut first_error: Option<EngineError> = None;

        // Lane 1: the API engine, through the full gate ladder.
        match self.api.route(key, platform, capability, offered, now) {
            Err(err) => match err {
                // The API engine cannot serve the capability: that is
                // exactly the degrade case, not a refusal.
                fishpond_router::RouteError::CapabilityNotSupported { .. } => {}
                // Authorization and configuration are terminal: fail
                // closed, never degrade onto a browser.
                other => return Err(EngineError::Route(other)),
            },
            Ok(plan) => match self.api.execute(&plan, ctx, now, api_call).await {
                Ok(value) => return Ok(value),
                Err(fishpond_router::RouteError::Upstream(err)) => {
                    first_error = Some(EngineError::Upstream(err));
                }
                Err(other) => return Err(EngineError::Route(other)),
            },
        }

        // Lane 2: the browser engine, under the backpressure gate.
        if self.browser.is_some_and(|lane| lane.serves(capability)) {
            match self.gate.run(browser_call(ctx)).await {
                // Gate-level failure (pending bound exceeded).
                Err(err) => return Err(err),
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(BrowserFault::CallerFault { reason })) => {
                    return Err(EngineError::CallerFault { reason });
                }
                Ok(Err(BrowserFault::Transient { reason })) => {
                    // No lane left after this one: the browser
                    // fault becomes the answer only if the API
                    // lane never spoke.
                    first_error = first_error.or(Some(EngineError::Browser { reason }));
                }
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Err(EngineError::NoEngine { capability }),
        }
    }
}
