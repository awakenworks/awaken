//! Configuration publication values shared by the service and HTTP edge.

use awaken_config_store::AgentConfig;
use awaken_runtime_contract::{InferenceAccess, ResolutionManifest};

/// A publish failure, split so the edge can map it to an HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("no config stored for agent `{0}`")]
    NotStored(String),
    #[error("cannot resolve an auto model binding: {0}")]
    Unresolvable(String),
    #[error("config changed while it was being published (current revision: {0:?})")]
    StaleRevision(Option<u64>),
    #[error("{0}")]
    Compile(String),
    #[error("{0}")]
    Store(String),
}

/// A compile problem projected onto the authored config field that caused it.
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

/// Transient, secret-free result of reading every configuration source once.
#[derive(Debug)]
pub(crate) struct ResolvedAgentConfig {
    pub(crate) source: awaken_runtime_contract::AgentConfigRevisionRef,
    pub(crate) config: AgentConfig,
    pub(crate) manifest: ResolutionManifest,
    pub(crate) inference_access: Option<InferenceAccess>,
}

pub(crate) fn snapshot_metadata(
    resolved: &ResolvedAgentConfig,
) -> awaken_runtime_contract::AgentSnapshotMetadata {
    awaken_runtime_contract::AgentSnapshotMetadata {
        source: resolved.source.clone(),
        publication_version: awaken_runtime_contract::AgentPublicationVersion(String::new()),
        resolution: resolved.manifest.clone(),
        fingerprint: awaken_runtime_contract::AgentSnapshotFingerprint(String::new()),
        inference_access: resolved.inference_access.clone(),
    }
}
