//! The hand side (ADR-0044 D3): a value-returning tool server.
//!
//! [`HandSession`] is the pure request handler — a tool registry plus an
//! idempotency ledger — with no model client, no commit coordinator, and no
//! store (enforced by G33 at the crate-dependency level). [`serve_hand`] drives a
//! session over a framed byte channel. The hand writes no durable runtime truth;
//! it returns serializable data only. The brain commits the result.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_agent_channel::AgentChannel;
use awaken_runtime_contract::tool::RawTool;
use futures_util::{SinkExt, StreamExt};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::wire::{HandError, HandErrorKind, HandReply, HandRequest, HandResult};
use crate::{HandOperationLedger, InMemoryOperationLedger, LedgerAdmission};

/// The hand's tool catalog plus its idempotency ledger.
///
/// A registry entry is an ordinary [`RawTool`]; the hand does not know the
/// runtime, only how to invoke a tool by id and return its serializable output.
pub struct HandSession {
    registry: HashMap<String, Arc<dyn RawTool>>,
    catalog_fingerprint: Option<String>,
    ledger: Arc<dyn HandOperationLedger>,
}

impl HandSession {
    /// A session over `tools`, keyed by each tool's id.
    pub fn new(tools: impl IntoIterator<Item = Arc<dyn RawTool>>) -> Self {
        let registry = tools.into_iter().map(|t| (t.id().to_string(), t)).collect();
        Self {
            registry,
            catalog_fingerprint: None,
            ledger: Arc::new(InMemoryOperationLedger::default()),
        }
    }

    /// Use a process-external operation ledger when effects must remain fenced
    /// across hand restarts.
    #[must_use]
    pub fn with_operation_ledger(mut self, ledger: Arc<dyn HandOperationLedger>) -> Self {
        self.ledger = ledger;
        self
    }

    /// Fail closed on any request whose fingerprint does not match `fingerprint`.
    #[must_use]
    pub fn with_catalog_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.catalog_fingerprint = Some(fingerprint.into());
        self
    }

    /// Handle one request. A `correlation_id` already in the ledger returns the
    /// recorded result without re-running the effect (ADR-0044 D4).
    pub async fn handle(&mut self, request: HandRequest) -> HandReply {
        // Old peers omitted `operation_id`; treating the already-stable tool call
        // id as the operation identity preserves compatibility without falling
        // back to the per-request correlation id.
        let operation_id = if request.operation_id.is_empty() {
            request.call.call_id.as_str()
        } else {
            request.operation_id.as_str()
        };
        let result = match self.ledger.begin(operation_id).await {
            Ok(LedgerAdmission::Cached(result)) => result,
            Ok(LedgerAdmission::Indeterminate) => HandResult::Indeterminate,
            Err(error) => HandResult::err(HandError::new(
                HandErrorKind::Execution,
                format!("hand operation ledger unavailable: {error}"),
            )),
            Ok(LedgerAdmission::Execute) => {
                let result = self.dispatch(&request).await;
                if let Err(error) = self.ledger.complete(operation_id, &result).await {
                    return HandReply {
                        correlation_id: request.correlation_id,
                        result: HandResult::err(HandError::new(
                            HandErrorKind::Execution,
                            format!("hand operation result was not durable: {error}"),
                        )),
                    };
                }
                result
            }
        };
        HandReply {
            correlation_id: request.correlation_id,
            result,
        }
    }

    async fn dispatch(&self, request: &HandRequest) -> HandResult {
        if let (Some(expected), Some(got)) =
            (&self.catalog_fingerprint, &request.catalog_fingerprint)
            && expected != got
        {
            return HandResult::err(HandError::new(
                HandErrorKind::FingerprintMismatch,
                format!("catalog fingerprint mismatch: hand={expected} run={got}"),
            ));
        }
        match self.registry.get(&request.call.tool_id) {
            None => HandResult::err(HandError::unknown_tool(&request.call.tool_id)),
            Some(tool) => match tool.invoke(request.call.clone()).await {
                Ok(output) => HandResult::ok(output),
                Err(err) => {
                    HandResult::err(HandError::new(HandErrorKind::Execution, err.to_string()))
                }
            },
        }
    }
}

/// Errors serving a hand over a channel.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("hand channel I/O failed: {0}")]
    Io(String),
    #[error("hand received a frame that is not a HandRequest: {0}")]
    Decode(String),
}

/// Drive a [`HandSession`] over a framed byte channel until the peer hangs up.
///
/// Reads length-delimited JSON `HandRequest` frames, invokes the session, and
/// writes `HandReply` frames. Returns `Ok(())` on a clean peer disconnect.
pub async fn serve_hand<S>(channel: S, mut session: HandSession) -> Result<(), ServeError>
where
    S: AgentChannel,
{
    let mut framed = Framed::new(channel, LengthDelimitedCodec::new());
    while let Some(frame) = framed.next().await {
        let frame = frame.map_err(|e| ServeError::Io(e.to_string()))?;
        let request: HandRequest =
            serde_json::from_slice(&frame).map_err(|e| ServeError::Decode(e.to_string()))?;
        let reply = session.handle(request).await;
        let bytes = serde_json::to_vec(&reply).map_err(|e| ServeError::Decode(e.to_string()))?;
        framed
            .send(bytes.into())
            .await
            .map_err(|e| ServeError::Io(e.to_string()))?;
    }
    Ok(())
}
