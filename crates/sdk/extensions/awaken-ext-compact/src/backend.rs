//! Asynchronous compaction labor port.
//!
//! The extension owns window policy; a host implements this port with its
//! ordinary Agent Run substrate and background scheduler. Cached artifacts are
//! accelerators only: a hard request may always call `summarize` and recover the
//! stable committed auxiliary Run.

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;

/// One immutable prefix handed to the compactor.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactRequest {
    /// Ordinary auxiliary Agent frozen by the parent Agent's compact config.
    pub agent_id: String,
    /// Cache/recovery namespace: parent Thread plus compaction policy identity.
    pub scope: String,
    /// Content-derived stable identity for this exact prefix and prompt.
    pub key: String,
    /// Number of parent transcript messages covered by the requested summary.
    pub covered_messages: usize,
    /// Folded prefix followed by the compaction prompt.
    pub seed: Vec<Message>,
}

/// A completed summary and the exact parent prefix it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactArtifact {
    pub scope: String,
    pub key: String,
    pub covered_messages: usize,
    pub summary: String,
}

/// Host port for non-blocking soft prefetch and hard, recoverable resolution.
#[async_trait]
pub trait CompactBackend: Send + Sync {
    /// Schedule the request and return after durable/stable work has been handed
    /// to the host's background executor. Duplicate keys are idempotent.
    async fn prefetch(&self, request: CompactRequest);

    /// Newest ready artifact in `scope` whose covered prefix does not exceed the
    /// current hard fold point. Pending work is not awaited.
    async fn latest_ready(&self, scope: &str, at_most_messages: usize) -> Option<CompactArtifact>;

    /// Resolve this exact request, joining an identical in-flight prefetch or
    /// running/recovering its stable auxiliary Run.
    async fn summarize(&self, request: CompactRequest) -> Option<CompactArtifact>;
}
