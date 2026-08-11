//! Control-to-Coordinator executable Agent registration boundary.
//!
//! The values in this crate are transport commands configured from existing
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

/// Coordinator-owned inventory view used by projections such as `/v1/models`.
/// It enumerates the same current registrations as exact Session resolution and
/// exposes no Control catalog or credential repository.
#[async_trait]
pub trait ExecutableAgentInventorySource: Send + Sync {
    async fn current_registrations(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ExecutableAgentRegistration>, ExecutableAgentRegistrationError>;
}

/// Model references reachable from the current executable Agent publications in
/// one Workspace. Primary and fallback candidates share this one normalization
/// rule for Dream readiness and every public model projection.
pub async fn current_model_references(
    registrations: &dyn ExecutableAgentInventorySource,
    workspace_id: &str,
) -> Result<Vec<String>, ExecutableAgentRegistrationError> {
    let mut model_references = registrations
        .current_registrations(workspace_id)
        .await?
        .into_iter()
        .flat_map(|registration| {
            let spec = registration.snapshot.resolved_spec;
            std::iter::once(spec.model_binding)
                .chain(spec.model_candidates)
                .map(|candidate| candidate.binding.model_ref)
        })
        .filter(|model_reference| !model_reference.trim().is_empty())
        .collect::<Vec<_>>();
    model_references.sort();
    model_references.dedup();
    Ok(model_references)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::snapshot::AgentId;
    use awaken_runtime_contract::{
        AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
        AgentSnapshotMetadata, ExecutableAgentSnapshot,
    };

    struct Inventory(Vec<ExecutableAgentRegistration>);

    #[async_trait]
    impl ExecutableAgentInventorySource for Inventory {
        async fn current_registrations(
            &self,
            workspace_id: &str,
        ) -> Result<Vec<ExecutableAgentRegistration>, ExecutableAgentRegistrationError> {
            Ok(self
                .0
                .iter()
                .filter(|registration| registration.workspace_id == workspace_id)
                .cloned()
                .collect())
        }
    }

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
        }
    }

    #[tokio::test]
    async fn executable_model_reference_decision_table() {
        // FMECA: a missing Workspace match could leak another tenant's models;
        // repeated primary/fallback references could expose inconsistent model
        // choices; a blank reference could create an unusable route. Causes are
        // C1 current registration in scope, C2 primary/fallback reference, C3
        // duplicate, C4 blank, C5 another scope. Effects are E1 include, E2
        // sort/deduplicate, E3 omit. Cause graph: (C1 && C2 && !C4) -> E1;
        // C3 -> E2; (C4 || C5) -> E3.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
        // |---|---|---|---|---|---|---|
        // | M1 | yes | primary+fallback | no | no | no | E1 sorted refs |
        // | M2 | yes | either | yes | no | no | E2 one ref |
        // | M3 | yes | either | any | yes | no | E3 omitted |
        // | M4 | no | any | any | any | yes | E3 omitted |
        let registration = |workspace: &str, agent: &str, primary: &str, fallbacks: &[&str]| {
            ExecutableAgentRegistration {
                workspace_id: workspace.into(),
                agent_id: agent.into(),
                source_revision: 1,
                snapshot: ExecutableAgentSnapshot::builder(agent)
                    .model(awaken_runtime_contract::resolved::ModelBinding::new(
                        "provider", primary, "native",
                    ))
                    .model_candidates(fallbacks.iter().map(|model| {
                        awaken_runtime_contract::resolved::ModelBinding::new(
                            "provider", *model, "native",
                        )
                    }))
                    .build(),
                session_profile: ExecutableAgentSessionProfile::default(),
            }
        };
        let inventory = Inventory(vec![
            registration("workspace-a", "agent-a", "model-b", &["model-a", ""]),
            registration("workspace-a", "agent-b", "model-a", &["model-c"]),
            registration("workspace-b", "agent-c", "model-secret", &[]),
        ]);

        assert_eq!(
            current_model_references(&inventory, "workspace-a")
                .await
                .unwrap(),
            vec!["model-a", "model-b", "model-c"]
        );
    }

    #[test]
    fn registration_identity_and_fingerprints_fail_closed() {
        // Cause/effect decision table:
        // V1 exact non-empty identity + matching source/fingerprints -> valid;
        // V2 boundary Agent mismatch, V3 source revision mismatch and V4 any
        // fingerprint mismatch -> Invalid. Placement is intentionally absent.
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
    }

    #[test]
    fn registration_wire_round_trip_preserves_existing_values() {
        // One boundary command owns the wire shape. A serde round trip must keep
        // the exact snapshot, Session view and revision; no
        // parallel DTO is reconstructed by an adapter.
        let value = registration();
        let encoded = serde_json::to_vec(&value).unwrap();
        let decoded: ExecutableAgentRegistration = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, value);
    }
}
