//! The brain side (ADR-0044 D1): a remote [`ToolExecutor`].
//!
//! [`RemoteToolExecutor`] frames one already-authorized [`ToolCall`] onto a
//! channel to a hand and awaits its [`HandReply`]. It is a drop-in `ToolExecutor`:
//! the kernel loop calls it exactly as it calls the in-process `LocalToolExecutor`,
//! and never learns where the tool ran.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_runtime_contract::tool::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::wire::{HandErrorKind, HandReply, HandRequest, HandResult};

/// A `ToolExecutor` that runs each call on a remote hand over `channel`.
///
/// Calls in one run are sequential (the loop awaits each), so a single framed
/// channel guarded by a mutex is sufficient: send request, read the matching
/// reply. Correlation ids are monotonic and also key the hand's idempotency
/// ledger, so a re-drive after a transport hiccup runs the effect at most once.
pub struct RemoteToolExecutor<S> {
    framed: Mutex<Framed<S, LengthDelimitedCodec>>,
    next_id: AtomicU64,
    catalog_fingerprint: Option<String>,
}

impl<S> RemoteToolExecutor<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Wrap an established byte channel to a hand.
    pub fn new(channel: S) -> Self {
        Self {
            framed: Mutex::new(Framed::new(channel, LengthDelimitedCodec::new())),
            next_id: AtomicU64::new(1),
            catalog_fingerprint: None,
        }
    }

    /// Stamp every request with the run's catalog fingerprint so the hand can
    /// fail closed on a mismatch (mirrors the runtime's own fingerprint check).
    #[must_use]
    pub fn with_catalog_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.catalog_fingerprint = Some(fingerprint.into());
        self
    }

    /// Run one call on the hand, returning the raw [`HandResult`].
    ///
    /// A transport failure *after* the request was written is surfaced as
    /// [`HandResult::Indeterminate`] (ADR-0044 D4): the effect may have run, so
    /// the caller must resolve it by an idempotent re-drive, never by assuming
    /// success or failure.
    pub async fn call_hand(&self, call: &ToolCall) -> HandResult {
        let correlation_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = HandRequest {
            correlation_id,
            catalog_fingerprint: self.catalog_fingerprint.clone(),
            deadline_unix_ms: None,
            call: call.clone(),
        };
        let bytes = match serde_json::to_vec(&request) {
            Ok(b) => b,
            // A serialization failure is a local, pre-dispatch fault: the call
            // never left, so it is a definite error, not indeterminate.
            Err(e) => return HandResult::err(crate::wire::HandError::new(
                HandErrorKind::Execution,
                format!("failed to encode hand request: {e}"),
            )),
        };

        let mut framed = self.framed.lock().await;
        if framed.send(bytes.into()).await.is_err() {
            // Could not even write the request → it never ran → definite failure.
            return HandResult::err(crate::wire::HandError::new(
                HandErrorKind::Execution,
                "hand channel closed before dispatch",
            ));
        }
        // Past this point the request is on the wire; any read failure is
        // indeterminate.
        match framed.next().await {
            Some(Ok(frame)) => match serde_json::from_slice::<HandReply>(&frame) {
                Ok(reply) if reply.correlation_id == correlation_id => reply.result,
                Ok(_) => HandResult::Indeterminate,
                Err(_) => HandResult::Indeterminate,
            },
            _ => HandResult::Indeterminate,
        }
    }
}

#[async_trait]
impl<S> ToolExecutor for RemoteToolExecutor<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        match self.call_hand(call).await {
            HandResult::Ok { output } => Ok(output),
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
