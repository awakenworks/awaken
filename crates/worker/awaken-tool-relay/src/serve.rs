//! The hand side (ADR-0044 D3): a value-returning tool server.
//!
//! [`HandSession`] is the pure request handler — a tool registry plus an
//! idempotency ledger — with no model client, no commit coordinator, and no
//! store (enforced by G33 at the crate-dependency level). [`serve_hand`] drives a
//! session over a framed byte channel. The hand writes no durable runtime truth;
//! it returns serializable data only. The brain commits the result.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_agent_channel::AgentChannel;
use awaken_runtime_contract::tool::{RawTool, RawToolRegistry, ToolError, ToolExecutor};
use futures_util::{SinkExt, StreamExt};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::wire::{HandError, HandErrorKind, HandReply, HandRequest, HandResult};
use crate::{HandOperationLedger, LedgerAdmission};

/// The hand's tool catalog plus its idempotency ledger.
///
/// A registry entry is an ordinary [`RawTool`]; the hand does not know the
/// runtime, only how to invoke a tool by id and return its serializable output.
pub struct HandSession {
    registry: RawToolRegistry,
    catalog_fingerprint: Option<String>,
    ledger: Arc<dyn HandOperationLedger>,
    in_flight_wait_timeout: Duration,
}

const DEFAULT_IN_FLIGHT_WAIT_TIMEOUT: Duration = Duration::from_secs(300);

impl HandSession {
    /// A session over `tools`, keyed by each tool's id.
    pub fn new(
        tools: impl IntoIterator<Item = Arc<dyn RawTool>>,
        ledger: Arc<dyn HandOperationLedger>,
    ) -> Self {
        let registry = RawToolRegistry::new(tools);
        Self {
            registry,
            catalog_fingerprint: None,
            ledger,
            in_flight_wait_timeout: DEFAULT_IN_FLIGHT_WAIT_TIMEOUT,
        }
    }

    /// Process-local constructor available only to tests and explicit test-support
    /// consumers. Production composition must select a durable ledger.
    #[cfg(any(test, feature = "test-support"))]
    pub fn in_memory(tools: impl IntoIterator<Item = Arc<dyn RawTool>>) -> Self {
        Self::new(tools, Arc::new(crate::InMemoryOperationLedger::default()))
    }

    /// Fail closed on any request whose fingerprint does not match `fingerprint`.
    #[must_use]
    pub fn with_catalog_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.catalog_fingerprint = Some(fingerprint.into());
        self
    }

    /// Bound a reconnect's wait for a live owner. Timing out never cancels or
    /// replays the original effect; it only returns an indeterminate result to
    /// this waiter.
    #[must_use]
    pub fn with_in_flight_wait_timeout(mut self, timeout: Duration) -> Self {
        self.in_flight_wait_timeout = timeout;
        self
    }

    /// Handle one request. An operation already completed by this Hand process
    /// returns its process-local result without re-running the effect. A claim
    /// recovered from a prior process is indeterminate (ADR-0044 D4).
    pub async fn handle(&mut self, request: HandRequest) -> HandReply {
        // Envelope validation is side-effect free and must precede ledger
        // admission. A rejected request must not claim or cache the stable
        // operation identity that a later authoritative request may use.
        if let Some(result) = self.validate_envelope(&request) {
            return HandReply {
                correlation_id: request.correlation_id,
                result,
            };
        }
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
            Ok(LedgerAdmission::InFlight) => {
                let wait = request
                    .deadline_unix_ms
                    .map(|deadline| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();
                        let remaining = u128::from(deadline).saturating_sub(now);
                        Duration::from_millis(u64::try_from(remaining).unwrap_or(u64::MAX))
                            .min(self.in_flight_wait_timeout)
                    })
                    .unwrap_or(self.in_flight_wait_timeout);
                match tokio::time::timeout(wait, self.ledger.wait(operation_id)).await {
                    Ok(Ok(Some(result))) => result,
                    Ok(Ok(None)) | Err(_) => HandResult::Indeterminate,
                    Ok(Err(error)) => HandResult::err(HandError::new(
                        HandErrorKind::Execution,
                        format!("hand operation join unavailable: {error}"),
                    )),
                }
            }
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
                            format!("hand operation completion fence was not durable: {error}"),
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

    fn validate_envelope(&self, request: &HandRequest) -> Option<HandResult> {
        // Keep the established rolling-production boundary: an unstamped legacy
        // peer remains accepted, but two present fingerprints must agree.
        if let (Some(expected), Some(got)) =
            (&self.catalog_fingerprint, &request.catalog_fingerprint)
            && expected != got
        {
            return Some(HandResult::err(HandError::new(
                HandErrorKind::FingerprintMismatch,
                format!("catalog fingerprint mismatch: hand={expected} run={got}"),
            )));
        }
        if let Some(deadline) = request.deadline_unix_ms {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            if now >= u128::from(deadline) {
                return Some(HandResult::err(HandError::new(
                    HandErrorKind::DeadlineExceeded,
                    format!("hand request deadline {deadline} has expired"),
                )));
            }
        }
        None
    }

    async fn dispatch(&self, request: &HandRequest) -> HandResult {
        match self.registry.invoke(&request.call).await {
            Ok(output) => HandResult::ok(output),
            Err(ToolError::Unknown(tool_id)) => HandResult::err(HandError::unknown_tool(&tool_id)),
            Err(error) => {
                HandResult::err(HandError::new(HandErrorKind::Execution, error.to_string()))
            }
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
