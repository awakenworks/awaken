//! The brain side (ADR-0044 D1): a remote [`ToolExecutor`].
//!
//! [`RemoteToolExecutor`] frames one already-authorized [`ToolCall`] onto a
//! channel to a hand and awaits its [`HandReply`]. It is a drop-in `ToolExecutor`:
//! the kernel loop calls it exactly as it calls the in-process `LocalToolExecutor`,
//! and never learns where the tool ran.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_runtime_contract::tool::{
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
}

struct HandChannel<S> {
    framed: Framed<S, LengthDelimitedCodec>,
    usable: bool,
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
                usable: true,
            }),
            next_id: AtomicU64::new(1),
            catalog_fingerprint: None,
            operation_scope: None,
            recovery_capability: ToolRecoveryCapability::NonRecoverable,
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

    /// Run one call on the hand, returning the raw [`HandResult`].
    ///
    /// Once writing the request has been attempted, every transport failure is
    /// surfaced as [`HandResult::Indeterminate`] (ADR-0044 D4). A stream write
    /// may fail after emitting part or all of a frame, so an error cannot prove
    /// that the effect stayed behind the dispatch boundary.
    pub async fn call_hand(&self, call: &ToolCall) -> HandResult {
        let correlation_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let operation_id = match current_tool_operation_token() {
            Some(token) => token.ledger_id(self.operation_scope.as_deref()),
            None if self.recovery_capability == ToolRecoveryCapability::DurableRequest => {
                return HandResult::err(crate::wire::HandError::new(
                    HandErrorKind::Execution,
                    MISSING_DURABLE_OPERATION_TOKEN,
                ));
            }
            None => self.operation_scope.as_ref().map_or_else(
                || call.call_id.clone(),
                |scope| format!("{scope}:{}", call.call_id),
            ),
        };
        let request = HandRequest {
            protocol_version: crate::wire::CURRENT_HAND_PROTOCOL_VERSION,
            correlation_id,
            operation_id,
            catalog_fingerprint: self.catalog_fingerprint.clone(),
            deadline_unix_ms: None,
            call: call.clone(),
        };
        let bytes = match serde_json::to_vec(&request) {
            Ok(b) => b,
            // A serialization failure is a local, pre-dispatch fault: the call
            // never left, so it is a definite error, not indeterminate.
            Err(e) => {
                return HandResult::err(crate::wire::HandError::new(
                    HandErrorKind::Execution,
                    format!("failed to encode hand request: {e}"),
                ));
            }
        };

        let mut channel = self.channel.lock().await;
        if !channel.usable {
            return HandResult::Indeterminate;
        }
        // Mark the stream unusable before the first cancellable write. A dropped
        // caller cannot otherwise tell whether a partial/full frame or its reply
        // remains queued. Only the exact correlated reply restores readiness.
        channel.usable = false;
        if channel.framed.send(bytes.into()).await.is_err() {
            return HandResult::Indeterminate;
        }
        // Past this point the request is on the wire; any read failure is
        // indeterminate.
        match channel.framed.next().await {
            Some(Ok(frame)) => match serde_json::from_slice::<HandReply>(&frame) {
                Ok(reply)
                    if hand_channel_reply_admitted(
                        true,
                        reply.correlation_id == correlation_id,
                    ) =>
                {
                    channel.usable = true;
                    reply.result
                }
                Ok(_) => HandResult::Indeterminate,
                Err(_) => HandResult::Indeterminate,
            },
            _ => HandResult::Indeterminate,
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn hand_channel_only_exact_decoded_reply_restores_readiness() {
        let decoded = kani::any();
        let correlation_matches = kani::any();
        assert_eq!(
            hand_channel_reply_admitted(decoded, correlation_matches),
            decoded && correlation_matches
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
        match self.call_hand(call).await {
            HandResult::Ok { output } => Ok(output),
            HandResult::Err { error }
                if error.kind == HandErrorKind::Execution
                    && error.message == MISSING_DURABLE_OPERATION_TOKEN =>
            {
                // This is a local composition error, not evidence that replacing
                // the Hand or its channel can make the invocation succeed.
                Err(ToolError::Execution(format!(
                    "tool executor configuration rejected dispatch: {}",
                    error.message
                )))
            }
            HandResult::Err { error } => match error.kind {
                // Preserve the in-process display so a remote unknown-tool reads
                // identically to a local one.
                HandErrorKind::UnknownTool => Err(ToolError::Unknown(call.tool_id.clone())),
                _ => Err(ToolError::Execution(error.message)),
            },
            HandResult::Indeterminate => Err(ToolError::Execution(format!(
                "indeterminate: hand connection lost during `{}`",
                call.tool_id
            ))),
        }
    }
}
