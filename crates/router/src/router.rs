//! Capability router (F016): the single upstream seam — picks the
//! provider, enforces the authorization precedence, guards the route
//! with the circuit breaker and classifies the outcome.
//!
//! A routed call has five gates, in order, each fail-closed:
//!
//! 1. **RBAC scope** ([`crate::scope`]) — the resolved API key must
//!    hold the capability's frozen scope. Checked first so a denied
//!    key learns nothing about provider topology.
//! 2. **Routability** — the capability must map to a scope at all;
//!    unscoped upstream faces stay unreachable for every key.
//! 3. **Platform registration** — a provider must exist for the
//!    platform.
//! 4. **Capability support** — the provider's manifest must declare
//!    the capability, and the caller's protocol offer must negotiate
//!    against the manifest (F004 semantics).
//! 5. **Circuit** ([`crate::breaker`]) — the tenant+platform circuit
//!    must be closed (or offering a probe).
//!
//! [`CapabilityRouter::execute`] then performs the upstream call and
//! settles the circuit from the outcome per [`crate::classify`]:
//! caller faults pass through uncounted, upstream faults count
//! consecutively and eventually open the circuit.
//!
//! Only providers registered in *this* process are routable, and in
//! CI that means the mock (§7.3): real engines (F032+) plug in here
//! once authorized — the gates do not change.

use std::collections::HashMap;
use std::sync::Arc;

use fishpond_provider_sdk::{
    CallContext, Capability, ProtocolVersion, Provider, ProviderError, ProviderResult,
    version::negotiate,
};
use fishpond_storage::ResolvedKey;

use crate::breaker::{BreakerPolicy, CircuitBreaker};
use crate::classify::{FailureDecision, decide};
use crate::error::{RouteError, RouteResult};
use crate::scope::authorize_scope;

/// A routed, gated call ready to execute.
#[derive(Clone)]
pub struct RoutePlan {
    /// The selected provider.
    pub provider: Arc<dyn Provider>,
    /// The protocol version negotiated for this call.
    pub protocol: ProtocolVersion,
    /// The circuit key (`tenant/platform`) the call settles on.
    pub breaker_key: String,
    /// The capability the call was routed for (diagnostics).
    pub capability: Capability,
}

/// The capability router over a process-local provider registry.
#[derive(Clone)]
pub struct CapabilityRouter {
    providers: Arc<HashMap<String, Arc<dyn Provider>>>,
    breaker: Arc<CircuitBreaker>,
}

impl std::fmt::Debug for RoutePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutePlan")
            .field("provider_id", &self.provider.manifest().provider_id)
            .field("protocol", &self.protocol)
            .field("breaker_key", &self.breaker_key)
            .field("capability", &self.capability)
            .finish()
    }
}

impl CapabilityRouter {
    /// Builds a router with the given breaker policy over an initially
    /// empty registry.
    pub fn new(policy: BreakerPolicy) -> Self {
        Self {
            providers: Arc::new(HashMap::new()),
            breaker: Arc::new(CircuitBreaker::new(policy)),
        }
    }

    /// Registers (or replaces) the provider for `platform`.
    pub fn register(&mut self, platform: impl Into<String>, provider: Arc<dyn Provider>) {
        let providers = Arc::get_mut(&mut self.providers).expect(
            "register before the router is shared with callers (plans hold clones of the map)",
        );
        providers.insert(platform.into(), provider);
    }

    /// The breaker guarding every route.
    pub fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }

    /// Routes one capability call: applies the authorization
    /// precedence and circuit gates and returns the plan to execute.
    /// No upstream is touched here.
    pub fn route(
        &self,
        key: &ResolvedKey,
        platform: &str,
        capability: Capability,
        offered: &[ProtocolVersion],
        now: i64,
    ) -> RouteResult<RoutePlan> {
        // Gate 1: RBAC scope (cheapest, fail-closed, hides topology).
        authorize_scope(key, capability)?;
        // Gate 3: platform registered.
        let provider =
            self.providers
                .get(platform)
                .ok_or_else(|| RouteError::NoProviderForPlatform {
                    platform: platform.to_owned(),
                })?;
        // Gate 4a: the manifest declares the capability.
        if !provider.manifest().supports(capability) {
            return Err(RouteError::CapabilityNotSupported {
                provider: provider.manifest().provider_id.clone(),
                capability,
            });
        }
        // Gate 4b: protocol negotiation (F004 semantics; an empty or
        // disjoint offer is an upstream Unsupported).
        let protocol = negotiate(offered, &provider.manifest().protocol_versions).map_err(|e| {
            RouteError::Upstream(ProviderError::new(
                fishpond_provider_sdk::ErrorClass::Unsupported,
                "protocol_negotiation_failed",
                e.to_string(),
            ))
        })?;
        // Gate 5: the circuit (Open rejects; cooldown passed → probe).
        let breaker_key = format!("{}/{}", key.tenant_id.as_str(), platform);
        self.breaker.check(&breaker_key, now)?;
        Ok(RoutePlan {
            provider: Arc::clone(provider),
            protocol,
            breaker_key,
            capability,
        })
    }

    /// Executes a planned call and settles the circuit from the
    /// outcome: success closes/resets, an upstream fault counts a
    /// consecutive failure, a caller fault passes through uncounted.
    /// The closure receives the selected provider (as `&dyn`) and the
    /// context, e.g. `|p, c| Box::pin(p.send_message(c, request))`.
    pub async fn execute<T>(
        &self,
        plan: &RoutePlan,
        ctx: &CallContext,
        now: i64,
        call: impl for<'a> Fn(
            &'a dyn Provider,
            &'a CallContext,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = ProviderResult<T>> + Send + 'a>,
        >,
    ) -> RouteResult<T> {
        let outcome = call(plan.provider.as_ref(), ctx).await;
        match outcome {
            Ok(value) => {
                self.breaker.record_success(&plan.breaker_key);
                Ok(value)
            }
            Err(error) => {
                if decide(&error) == FailureDecision::UpstreamFault {
                    self.breaker.record_failure(&plan.breaker_key, now);
                }
                Err(RouteError::Upstream(error))
            }
        }
    }
}
