//! Control-to-Coordinator executable Agent registration boundary.
//!
//! The values in this crate are transport commands composed from existing
//! publication and Session projection values. They are not a second Agent model,
//! catalog implementation, persistence API, or RPC framework.

use async_trait::async_trait;
pub use awaken_runtime_contract::ExecutableAgentSnapshot;
use serde::{Deserialize, Serialize};

mod session_profile;

pub use session_profile::{
    ExecutableAgentEnvironment, ExecutableAgentMcpServer, ExecutableAgentProfileSource,
    ExecutableAgentSessionProfile,
};

/// One immutable Control publication made available for future Coordinator
/// Session resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableAgentRegistration {
    pub workspace_id: String,
    pub agent_id: String,
    pub source_revision: u64,
    pub snapshot: ExecutableAgentSnapshot,
    /// Session-facing defaults frozen from the same publication.
    pub session_profile: ExecutableAgentSessionProfile,
    /// Optional logical Hand placement intent. Placement remains outside the
    /// executable snapshot and is joined to deployment-owned executors later.
    pub declared_hand: Option<String>,
}

impl ExecutableAgentRegistration {
    /// Reject a command whose boundary identity does not match the immutable
    /// publication identity it carries.
    pub fn validate(&self) -> Result<(), ExecutableAgentRegistrationError> {
        let invalid = |message: &str| {
            Err(ExecutableAgentRegistrationError::Invalid(
                message.to_owned(),
            ))
        };
        if self.workspace_id.trim().is_empty() {
            return invalid("workspace_id must not be empty");
        }
        if self.agent_id.trim().is_empty() {
            return invalid("agent_id must not be empty");
        }
        if self.source_revision == 0 {
            return invalid("source_revision must be non-zero");
        }
        if self.snapshot.root_agent_id.0 != self.agent_id {
            return invalid("snapshot root Agent does not match agent_id");
        }
        if self.snapshot.metadata.is_legacy_default() {
            return invalid("registered publication must carry source metadata");
        }
        if self.snapshot.metadata.source.agent_id.0 != self.agent_id
            || self.snapshot.metadata.source.revision != self.source_revision
        {
            return invalid("snapshot source identity does not match registration identity");
        }
        let fingerprint = self.snapshot.fingerprint.0.trim();
        if fingerprint.is_empty()
            || self.snapshot.resolved_spec.catalog_fingerprint.0 != fingerprint
            || self.snapshot.metadata.fingerprint.0 != fingerprint
        {
            return invalid("snapshot fingerprints must be non-empty and identical");
        }
        if self
            .declared_hand
            .as_ref()
            .is_some_and(|hand| hand.trim().is_empty())
        {
            return invalid("declared_hand must be absent or non-empty");
        }
        Ok(())
    }
}

/// A monotonic lifecycle tombstone. Exact historical snapshots remain readable;
/// only current selection is withdrawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableAgentWithdrawal {
    pub workspace_id: String,
    pub agent_id: String,
    pub lifecycle_revision: u64,
}

impl ExecutableAgentWithdrawal {
    pub fn validate(&self) -> Result<(), ExecutableAgentRegistrationError> {
        if self.workspace_id.trim().is_empty() {
            return Err(ExecutableAgentRegistrationError::Invalid(
                "workspace_id must not be empty".into(),
            ));
        }
        if self.agent_id.trim().is_empty() {
            return Err(ExecutableAgentRegistrationError::Invalid(
                "agent_id must not be empty".into(),
            ));
        }
        if self.lifecycle_revision == 0 {
            return Err(ExecutableAgentRegistrationError::Invalid(
                "lifecycle_revision must be non-zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutableAgentRegistrationOutcome {
    RegisteredCurrent,
    RegisteredHistorical,
    AlreadyRegistered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutableAgentWithdrawalOutcome {
    WithdrawnCurrent,
    AlreadyWithdrawn,
    HistoricalNoop,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum ExecutableAgentRegistrationError {
    #[error("invalid executable Agent registration: {0}")]
    Invalid(String),
    #[error("conflicting executable Agent registration: {0}")]
    Conflict(String),
    #[error("executable Agent registration unavailable: {0}")]
    Unavailable(String),
    #[error("executable Agent registration storage failure: {0}")]
    Storage(String),
}

/// The sole application port from Control publication/lifecycle to Coordinator
/// executable availability.
#[async_trait]
pub trait ExecutableAgentRegistrar: Send + Sync {
    async fn register(
        &self,
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError>;

    async fn withdraw(
        &self,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError>;
}

/// Coordinator-owned read projection of Control's immutable registrations.
///
/// Deployment resolution needs only current or exact publication availability;
/// it must not receive Control's mutable Agent authoring repository merely to
/// freeze one executable revision.
#[async_trait]
pub trait ExecutableAgentRegistrationSource: Send + Sync {
    async fn current_registration(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<Option<ExecutableAgentRegistration>, ExecutableAgentRegistrationError>;

    async fn registration_at_revision(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Result<Option<ExecutableAgentRegistration>, ExecutableAgentRegistrationError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::snapshot::AgentId;
    use awaken_runtime_contract::{
        AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
        AgentSnapshotMetadata, ExecutableAgentSnapshot,
    };

    fn registration() -> ExecutableAgentRegistration {
        let mut snapshot = ExecutableAgentSnapshot::builder("agent-a")
            .fingerprint("fp-a")
            .build();
        snapshot.metadata = AgentSnapshotMetadata {
            source: AgentConfigRevisionRef {
                agent_id: AgentId("agent-a".into()),
                revision: 7,
            },
            publication_version: AgentPublicationVersion("v7".into()),
            resolution: Default::default(),
            fingerprint: AgentSnapshotFingerprint("fp-a".into()),
        };
        ExecutableAgentRegistration {
            workspace_id: "workspace-a".into(),
            agent_id: "agent-a".into(),
            source_revision: 7,
            snapshot,
            session_profile: ExecutableAgentSessionProfile::default(),
            declared_hand: Some("hand-a".into()),
        }
    }

    #[test]
    fn registration_identity_and_fingerprints_fail_closed() {
        // Cause/effect decision table:
        // V1 exact non-empty identity + matching source/fingerprints -> valid;
        // V2 boundary Agent mismatch, V3 source revision mismatch, V4 any
        // fingerprint mismatch, V5 empty optional Hand -> Invalid.
        let valid = registration();
        assert_eq!(valid.validate(), Ok(()), "V1");

        let mut mismatch = valid.clone();
        mismatch.agent_id = "other".into();
        assert!(
            matches!(
                mismatch.validate(),
                Err(ExecutableAgentRegistrationError::Invalid(_))
            ),
            "V2"
        );

        let mut mismatch = valid.clone();
        mismatch.source_revision = 8;
        assert!(mismatch.validate().is_err(), "V3");

        let mut mismatch = valid.clone();
        mismatch.snapshot.metadata.fingerprint.0 = "other".into();
        assert!(mismatch.validate().is_err(), "V4");

        let mut mismatch = valid;
        mismatch.declared_hand = Some(" ".into());
        assert!(mismatch.validate().is_err(), "V5");
    }

    #[test]
    fn registration_wire_round_trip_preserves_existing_values() {
        // One boundary command owns the wire shape. A serde round trip must keep
        // the exact snapshot, Session view, revision and placement intent; no
        // parallel DTO is reconstructed by an adapter.
        let value = registration();
        let encoded = serde_json::to_vec(&value).unwrap();
        let decoded: ExecutableAgentRegistration = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, value);
    }
}
