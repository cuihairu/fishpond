//! The provider contract suite: black-box invariants every provider must
//! hold, independent of engine.
//!
//! Run it against any [`Provider`] implementation (the stub in this crate's
//! integration tests does; provider-mock runs the same suite in F005).
//! Checks collect violations instead of panicking so one run reports every
//! broken invariant.

use std::future::Future;

use fishpond_domain::{AccountId, ConversationId, MessageDirection, TenantId};

use crate::manifest::ManifestError;
use crate::provider::{CallContext, Provider, SendMessageRequest};
use crate::result::{ErrorClass, ProviderError};
use crate::version::ProtocolVersion;

/// Known-good data a provider promises to serve for contract runs.
#[derive(Debug, Clone)]
pub struct ContractFixture {
    /// Tenant that owns the fixture data.
    pub tenant: TenantId,
    /// An account the provider can authorize.
    pub account: AccountId,
    /// An existing conversation on `account`.
    pub conversation: ConversationId,
}

/// Result of one contract check.
#[derive(Debug)]
pub struct CheckResult {
    /// Stable check name.
    pub name: &'static str,
    /// `Ok(())` when the invariant held, else the violation description.
    pub outcome: Result<(), String>,
}

/// The full suite report.
#[derive(Debug)]
pub struct ContractReport {
    /// One entry per check, in execution order.
    pub checks: Vec<CheckResult>,
}

impl ContractReport {
    /// Names of failed checks.
    pub fn failures(&self) -> Vec<&'static str> {
        self.checks
            .iter()
            .filter(|c| c.outcome.is_err())
            .map(|c| c.name)
            .collect()
    }

    /// Whether every check passed.
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.outcome.is_ok())
    }

    /// Panics listing every failed check; call from tests.
    pub fn assert_ok(&self) {
        assert!(
            self.ok(),
            "provider contract violated ({} of {} check(s)):\n  - {}",
            self.failures().len(),
            self.checks.len(),
            self.failure_details().join("\n  - ")
        );
    }

    fn failure_details(&self) -> Vec<String> {
        self.checks
            .iter()
            .filter_map(|c| c.outcome.as_ref().err().map(|e| format!("{}: {e}", c.name)))
            .collect()
    }
}

/// Runs a single check body and records it.
async fn record<F>(name: &'static str, checks: &mut Vec<CheckResult>, body: F)
where
    F: Future<Output = Result<(), String>>,
{
    checks.push(CheckResult {
        name,
        outcome: body.await,
    });
}

/// The idempotency key the suite uses, derived from the fixture account.
fn suite_key(account: &AccountId) -> String {
    let who = account.as_str();
    format!("contract-{who}")
}

/// Runs every contract check against `provider`.
pub async fn run_contract_suite(
    provider: &dyn Provider,
    fixture: &ContractFixture,
) -> ContractReport {
    let mut checks: Vec<CheckResult> = Vec::new();

    // 1. The manifest must be valid and declare at least one capability.
    record("manifest_valid", &mut checks, async {
        provider
            .manifest()
            .validate()
            .map_err(|e: ManifestError| e.to_string())?;
        Ok(())
    })
    .await;

    // 2. Version negotiation must agree on a major the provider declares.
    record("version_negotiation", &mut checks, async {
        let agreed = provider
            .negotiate_protocol(&[ProtocolVersion::V1_0, ProtocolVersion::V1_1])
            .map_err(|e| e.to_string())?;
        let declared = provider.manifest().protocol_versions.clone();
        if !declared.iter().any(|v| v.major == agreed.major) {
            return Err(format!("negotiated {agreed} outside the declared set"));
        }
        Ok(())
    })
    .await;

    // 3+. Write path needs a validated context; a bad one aborts the suite.
    let key = suite_key(&fixture.account);
    let ctx = match CallContext::write(fixture.tenant.clone(), fixture.account.clone(), &key) {
        Ok(ctx) => ctx,
        Err(e) => {
            for name in [
                "authorize_then_send",
                "idempotent_replay",
                "history_oldest_first",
                "conversations_listed",
                "empty_text_rejected",
                "unknown_conversation_rejected",
                "unauthorized_account_is_auth_error",
            ] {
                checks.push(CheckResult {
                    name,
                    outcome: Err(e.to_string()),
                });
            }
            return ContractReport { checks };
        }
    };

    // 3. Authorize, then send: first send is fresh and outbound.
    record("authorize_then_send", &mut checks, async {
        provider.authorize(&ctx).await.map_err(|e| e.to_string())?;
        let outcome = provider
            .send_message(
                &ctx,
                SendMessageRequest {
                    conversation: fixture.conversation.clone(),
                    text: "contract hello".to_owned(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        if outcome.deduplicated {
            return Err("first send with a fresh key must not be deduplicated".to_owned());
        }
        if outcome.message.direction != MessageDirection::Outbound {
            return Err("sent message must be outbound".to_owned());
        }
        if outcome.message.text != "contract hello" {
            return Err("sent message text must round-trip".to_owned());
        }
        if outcome.message.conversation_id != fixture.conversation {
            return Err("sent message must land in the requested conversation".to_owned());
        }
        Ok(())
    })
    .await;

    // 4. Replay with the same key returns the original message.
    record("idempotent_replay", &mut checks, async {
        let request = SendMessageRequest {
            conversation: fixture.conversation.clone(),
            text: "contract hello".to_owned(),
        };
        let first = provider
            .send_message(&ctx, request.clone())
            .await
            .map_err(|e| e.to_string())?;
        let second = provider
            .send_message(&ctx, request)
            .await
            .map_err(|e| e.to_string())?;
        if !second.deduplicated {
            return Err("replay with the same key must set deduplicated".to_owned());
        }
        if first.message.id != second.message.id {
            return Err("replay must return the original message id".to_owned());
        }
        Ok(())
    })
    .await;

    // 5. History is oldest-first and contains what was sent.
    record("history_oldest_first", &mut checks, async {
        let messages = provider
            .list_messages(&ctx, &fixture.conversation)
            .await
            .map_err(|e| e.to_string())?;
        if messages.is_empty() {
            return Err("history must contain the sent message".to_owned());
        }
        if !messages
            .iter()
            .all(|m| m.conversation_id == fixture.conversation)
        {
            return Err("history must only contain the requested conversation".to_owned());
        }
        for pair in messages.windows(2) {
            if pair[0].sent_at > pair[1].sent_at {
                return Err("history must be oldest first".to_owned());
            }
        }
        if !messages.iter().any(|m| m.text == "contract hello") {
            return Err("history must contain the contract message".to_owned());
        }
        Ok(())
    })
    .await;

    // 6. The fixture conversation is listed for the account.
    record("conversations_listed", &mut checks, async {
        let list = provider
            .list_conversations(&ctx)
            .await
            .map_err(|e| e.to_string())?;
        if !list.iter().any(|c| c.id == fixture.conversation) {
            return Err("fixture conversation missing from list_conversations".to_owned());
        }
        Ok(())
    })
    .await;

    // 7. Empty text is an Invalid, non-retryable error.
    record("empty_text_rejected", &mut checks, async {
        let err = provider
            .send_message(
                &ctx,
                SendMessageRequest {
                    conversation: fixture.conversation.clone(),
                    text: "   ".to_owned(),
                },
            )
            .await
            .expect_err("empty text must be rejected");
        check_class(&err, ErrorClass::Invalid)
    })
    .await;

    // 8. Unknown conversation is an Invalid, non-retryable error.
    record("unknown_conversation_rejected", &mut checks, async {
        let err = provider
            .send_message(
                &ctx,
                SendMessageRequest {
                    conversation: ConversationId::parse("contract-no-such-conv").unwrap(),
                    text: "hello".to_owned(),
                },
            )
            .await
            .expect_err("unknown conversation must be rejected");
        check_class(&err, ErrorClass::Invalid)
    })
    .await;

    // 9. An account that never authorized yields an Auth error.
    record("unauthorized_account_is_auth_error", &mut checks, async {
        let stranger = CallContext::write(
            fixture.tenant.clone(),
            AccountId::parse("contract-stranger-account").unwrap(),
            "contract-stranger-key",
        )
        .map_err(|e| e.to_string())?;
        let err = provider
            .send_message(
                &stranger,
                SendMessageRequest {
                    conversation: fixture.conversation.clone(),
                    text: "hello".to_owned(),
                },
            )
            .await
            .expect_err("unauthorized account must be rejected");
        check_class(&err, ErrorClass::Auth)
    })
    .await;

    ContractReport { checks }
}

/// Asserts the error class is `want` and that it is retryable exactly as
/// the class declares.
fn check_class(err: &ProviderError, want: ErrorClass) -> Result<(), String> {
    if err.class != want {
        let got = err.class;
        let code = &err.code;
        return Err(format!("expected error class {want}, got {got} ({code})"));
    }
    let expected_retryable = want.retryable();
    if err.retryable() != expected_retryable {
        return Err(format!(
            "error {want} must be retryable = {expected_retryable}"
        ));
    }
    Ok(())
}
