// @formal entrypoint remote_tool.cancelled_channel_never_correlates_a_late_reply_to_another_call
// @formal entrypoint remote_tool.indeterminate_never_false_success
// @formal entrypoint remote_tool.stable_operation_identity
//! The brain side (ADR-0044 D1): a remote [`ToolExecutor`].
//!
//! [`RemoteToolExecutor`] frames one already-authorized [`ToolCall`] onto a
//! channel to a hand and awaits its [`HandReply`]. It is a drop-in `ToolExecutor`:
//! the kernel loop calls it exactly as it calls the in-process `LocalToolExecutor`,
//! and never learns where the tool ran.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_tool_contract::tool::{
    ToolCall, ToolError, ToolExecutor, ToolOutput, ToolRecoveryCapability,
    current_tool_operation_token,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::wire::{HandErrorKind, HandReply, HandRequest, HandResult};

const MISSING_DURABLE_OPERATION_TOKEN: &str =
    "durable hand dispatch requires a Runtime-owned tool operation token";

#[must_use]
const fn hand_channel_reply_admitted(decoded: bool, correlation_matches: bool) -> bool {
    decoded && correlation_matches
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HandResultDisposition {
    Success,
    Failure,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationIdentityDecision {
    RuntimeToken,
    LegacyCallIdentity,
    RejectMissingRuntimeToken,
}

#[must_use]
const fn operation_identity_decision(
    has_runtime_token: bool,
    durable_recovery: bool,
) -> OperationIdentityDecision {
    match (has_runtime_token, durable_recovery) {
        (true, _) => OperationIdentityDecision::RuntimeToken,
        (false, false) => OperationIdentityDecision::LegacyCallIdentity,
        (false, true) => OperationIdentityDecision::RejectMissingRuntimeToken,
    }
}

#[must_use]
const fn hand_result_disposition(
    explicit_success: bool,
    indeterminate: bool,
) -> HandResultDisposition {
    if explicit_success {
        HandResultDisposition::Success
    } else if indeterminate {
        HandResultDisposition::Indeterminate
    } else {
        HandResultDisposition::Failure
    }
}

fn map_hand_result(tool_id: &str, result: HandResult) -> Result<ToolOutput, ToolError> {
    let disposition = hand_result_disposition(
        matches!(&result, HandResult::Ok { .. }),
        matches!(
            &result,
            HandResult::Indeterminate | HandResult::Running { .. } | HandResult::Observed { .. }
        ),
    );

    match (disposition, result) {
        (HandResultDisposition::Success, HandResult::Ok { output }) => Ok(output),
        (HandResultDisposition::Failure, HandResult::Err { error })
            if error.kind == HandErrorKind::Execution
                && error.message == MISSING_DURABLE_OPERATION_TOKEN =>
        {
            // This is a local composition error, not evidence that replacing
            // the Hand or its channel can make the invocation succeed.
            Err(ToolError::DefinitiveFailure(format!(
                "tool executor configuration rejected dispatch: {}",
                error.message
            )))
        }
        (HandResultDisposition::Failure, HandResult::Err { error }) => match error.kind {
            // Preserve the in-process display so a remote unknown-tool reads
            // identically to a local one.
            HandErrorKind::UnknownTool => Err(ToolError::UnknownTool(tool_id.to_string())),
            _ => Err(ToolError::DefinitiveFailure(error.message)),
        },
        (
            HandResultDisposition::Indeterminate,
            HandResult::Indeterminate | HandResult::Running { .. } | HandResult::Observed { .. },
        ) => {
            // The same wire outcome covers lost replies and unresolved ledger
            // claims; do not invent a connection-loss cause or repeat the error
            // class in its detail. The Runtime already owns call coordinates.
            Err(ToolError::Indeterminate(tool_id.to_owned()))
        }
        _ => unreachable!("HandResult disposition must match its closed enum variant"),
    }
}

/// A `ToolExecutor` that runs each call on a remote hand over `channel`.
///
/// Calls in one run are sequential (the loop awaits each), so a single framed
/// channel guarded by a mutex is sufficient: send request, read the matching
/// reply. Correlation ids pair wire replies only; Runtime-owned operation tokens
/// key a durable hand's idempotency ledger across connection replacement.
pub struct RemoteToolExecutor<S> {
    channel: Mutex<HandChannel<S>>,
    next_id: AtomicU64,
    catalog_fingerprint: Option<String>,
    operation_scope: Option<String>,
    recovery_capability: ToolRecoveryCapability,
    pending_request: awaken_sync::LeafLock<Option<HandRequest>>,
}

struct HandChannel<S> {
    framed: Framed<S, LengthDelimitedCodec>,
}

impl<S> RemoteToolExecutor<S>
where
    S: AgentChannel,
{
    /// Wrap an established byte channel to a hand.
    pub fn new(channel: S) -> Self {
        Self {
            channel: Mutex::new(HandChannel {
                framed: Framed::new(channel, LengthDelimitedCodec::new()),
            }),
            next_id: AtomicU64::new(1),
            catalog_fingerprint: None,
            operation_scope: None,
            recovery_capability: ToolRecoveryCapability::NonRecoverable,
            pending_request: awaken_sync::LeafLock::new(None),
        }
    }

    /// Namespace stable operation ids by the owning run/session when one hand
    /// ledger serves more than one execution scope.
    #[must_use]
    pub fn with_operation_scope(mut self, scope: impl Into<String>) -> Self {
        self.operation_scope = Some(scope.into());
        self
    }

    /// Stamp every request with the run's catalog fingerprint so the hand can
    /// fail closed on a mismatch (mirrors the runtime's own fingerprint check).
    #[must_use]
    pub fn with_catalog_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.catalog_fingerprint = Some(fingerprint.into());
        self
    }

    /// Declare that this executor addresses one live resident Hand ledger by
    /// stable operation id. Attached or replaceable Hand processes must retain
    /// the fail-closed default.
    #[must_use]
    pub fn with_durable_request_recovery(mut self) -> Self {
        self.recovery_capability = ToolRecoveryCapability::DurableRequest;
        self
    }

    /// Run one call on the hand, returning its raw [`HandResult`]. A different
    /// operation rejected on an already poisoned stream returns the existing
    /// `UnavailableBeforeDispatch` error without writing any request bytes.
    ///
    /// Once writing the request has been attempted, every transport failure is
    /// surfaced as [`HandResult::Indeterminate`] (ADR-0044 D4). A stream write
    /// may fail after emitting part or all of a frame, so an error cannot prove
    /// that the effect stayed behind the dispatch boundary.
    pub async fn call_hand(&self, call: &ToolCall) -> Result<HandResult, ToolError> {
        let correlation_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let token = current_tool_operation_token();
        let durable_recovery = self.recovery_capability == ToolRecoveryCapability::DurableRequest;
        let operation_id = match (
            operation_identity_decision(token.is_some(), durable_recovery),
            token,
        ) {
            (OperationIdentityDecision::RuntimeToken, Some(token)) => {
                token.ledger_id(self.operation_scope.as_deref())
            }
            (OperationIdentityDecision::RejectMissingRuntimeToken, None) => {
                return Ok(HandResult::err(crate::wire::HandError::new(
                    HandErrorKind::Execution,
                    MISSING_DURABLE_OPERATION_TOKEN,
                )));
            }
            (OperationIdentityDecision::LegacyCallIdentity, None) => {
                self.operation_scope.as_ref().map_or_else(
                    || call.call_id.clone(),
                    |scope| format!("{scope}:{}", call.call_id),
                )
            }
            _ => unreachable!("operation identity decision preserves token presence"),
        };
        let request = HandRequest {
            action: crate::HandAction::Invoke,
            protocol_version: crate::wire::CURRENT_HAND_PROTOCOL_VERSION,
            correlation_id,
            operation_id,
            catalog_fingerprint: self.catalog_fingerprint.clone(),
            deadline_unix_ms: None,
            call: call.clone(),
        };
        self.exchange_request(request).await
    }

    /// Observe or cancel the exact unresolved request on an Environment-owned
    /// fresh channel. This never invokes a new operation and never clears the
    /// original stream's uncertainty or creates physical-stop evidence.
    pub async fn recover_pending<C: AgentChannel>(
        &self,
        channel: C,
        action: crate::HandAction,
    ) -> Result<Option<HandResult>, ToolError> {
        if action == crate::HandAction::Invoke {
            return Err(ToolError::InvalidArguments(
                "recovery requires Observe or Cancel".into(),
            ));
        }
        let Some(mut request) = self.pending_request.lock().clone() else {
            return Ok(None);
        };
        request.action = action;
        request.protocol_version = crate::wire::CURRENT_HAND_PROTOCOL_VERSION;
        request.correlation_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        RemoteToolExecutor::new(channel)
            .exchange_request(request)
            .await
            .map(Some)
    }

    async fn exchange_request(&self, request: HandRequest) -> Result<HandResult, ToolError> {
        let correlation_id = request.correlation_id;
        let bytes = match serde_json::to_vec(&request) {
            Ok(b) => b,
            // A serialization failure is a local, pre-dispatch fault: the call
            // never left, so it is a definite error, not indeterminate.
            Err(e) => {
                return Ok(HandResult::err(crate::wire::HandError::new(
                    HandErrorKind::Execution,
                    format!("failed to encode hand request: {e}"),
                )));
            }
        };

        let mut channel = self.channel.lock().await;
        if let Some(pending) = self.pending_request.lock().as_ref() {
            // The prior operation may have executed. Its retry remains unknown;
            // a distinct operation has sent no bytes and can use the existing
            // Environment-owned pre-dispatch replacement path. Correlation ids
            // and changed wire call ids must never turn the old operation new.
            return if pending.operation_id == request.operation_id {
                Ok(HandResult::Indeterminate)
            } else {
                Err(ToolError::UnavailableBeforeDispatch(
                    "Hand channel has an unresolved prior operation; this request was not sent"
                        .into(),
                ))
            };
        }
        // Mark the stream unusable before the first cancellable write. A dropped
        // caller cannot otherwise tell whether a partial/full frame or its reply
        // remains queued. Only the exact correlated reply restores readiness.
        *self.pending_request.lock() = Some(request.clone());
        if channel.framed.send(bytes.into()).await.is_err() {
            return Ok(HandResult::Indeterminate);
        }
        // Past this point the request is on the wire; any read failure is
        // indeterminate.
        Ok(match channel.framed.next().await {
            Some(Ok(frame)) => match serde_json::from_slice::<HandReply>(&frame) {
                Ok(reply)
                    if hand_channel_reply_admitted(
                        true,
                        reply.correlation_id == correlation_id,
                    ) =>
                {
                    *self.pending_request.lock() = None;
                    reply.result
                }
                Ok(_) => HandResult::Indeterminate,
                Err(_) => HandResult::Indeterminate,
            },
            _ => HandResult::Indeterminate,
        })
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation remote_tool.cancelled_channel_never_correlates_a_late_reply_to_another_call
    /// @proof-input-debt tautology hand_channel_reply_admitted
    #[kani::proof]
    fn hand_channel_only_exact_decoded_reply_restores_readiness() {
        let decoded = kani::any();
        let correlation_matches = kani::any();
        assert_eq!(
            hand_channel_reply_admitted(decoded, correlation_matches),
            decoded && correlation_matches
        );
    }

    /// @obligation remote_tool.indeterminate_never_false_success
    /// @proof-input-debt free_symbol hand_result_disposition explicit_success
    /// @proof-input-debt free_symbol hand_result_disposition indeterminate
    #[kani::proof]
    #[kani::unwind(16)]
    fn hand_result_disposition_allows_only_explicit_success() {
        // Cause graph: C1 the Hand returned Ok; C2 it returned a definite Err;
        // C3 its outcome is Indeterminate. Effects: E1 select Success; E2
        // select Failure; E3 select Indeterminate. Exactly one cause holds for
        // the closed HandResult enum; the production mapper owns each payload.
        //
        // | Rule | Hand result | Effect |
        // |---|---|---|
        // | R1 | Ok | E1 |
        // | R2 | Err | E2 |
        // | R3 | Indeterminate | E3 |
        let explicit_success: bool = kani::any();
        let indeterminate: bool = kani::any();
        kani::assume(!(explicit_success && indeterminate));

        let disposition = hand_result_disposition(explicit_success, indeterminate);
        assert_eq!(
            disposition == HandResultDisposition::Success,
            explicit_success
        );
        assert_eq!(
            disposition == HandResultDisposition::Indeterminate,
            indeterminate
        );
        // Verify the actual adapter too: selecting the right disposition is
        // insufficient if payload mapping erases Unknown into ordinary failure.
        // Prove the changed Unknown adapter arm with a fixed diagnostic id.
        // Success/error payload serialization remains executable-test evidence;
        // this does not prove transport delivery or provider termination.
        assert!(matches!(
            map_hand_result("tool", HandResult::Indeterminate),
            Err(ToolError::Indeterminate(_))
        ));
    }

    /// @obligation remote_tool.stable_operation_identity
    /// @proof-input-debt free_symbol operation_identity_decision has_runtime_token
    #[kani::proof]
    fn durable_hand_identity_requires_the_runtime_operation_token() {
        // Causes: C1 a Runtime-owned operation token is present; C2 durable
        // request recovery is enabled. Effects: E1 use the canonical token;
        // E2 allow the legacy call identity only for non-durable execution;
        // E3 reject before dispatch. The four Boolean rows are exhaustive.
        //
        // | Rule | C1 token | C2 durable | Effect |
        // |---|---|---|---|
        // | R1 | yes | no  | E1 |
        // | R2 | yes | yes | E1 |
        // | R3 | no  | no  | E2 |
        // | R4 | no  | yes | E3 |
        let has_runtime_token: bool = kani::any();
        let durable_recovery: bool = kani::any();
        let decision = operation_identity_decision(has_runtime_token, durable_recovery);

        assert_eq!(
            decision == OperationIdentityDecision::RuntimeToken,
            has_runtime_token
        );
        assert_eq!(
            decision == OperationIdentityDecision::LegacyCallIdentity,
            !has_runtime_token && !durable_recovery
        );
        assert_eq!(
            decision == OperationIdentityDecision::RejectMissingRuntimeToken,
            !has_runtime_token && durable_recovery
        );
    }
}

#[async_trait]
impl<S> ToolExecutor for RemoteToolExecutor<S>
where
    S: AgentChannel,
{
    fn recovery_capability(&self, _tool_id: &str) -> ToolRecoveryCapability {
        self.recovery_capability
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        map_hand_result(&call.tool_id, self.call_hand(call).await?)
    }
}
