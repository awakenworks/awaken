// @formal entrypoint executable_snapshot.pin_is_complete_exact_and_non_mixing
//! Control-to-Coordinator executable Agent registration boundary.
//!
//! The values in this crate are transport commands configured from existing
//! publication and Session projection values. They are not a second Agent model,
//! catalog implementation, persistence API, or RPC framework.

use async_trait::async_trait;
pub use awaken_runtime_contract::ExecutableAgentSnapshot;
use serde::{Deserialize, Serialize};

mod session_profile;
mod snapshot_pin;

/// The Control preview namespace. A preview registration has no durable
/// publication time and must not appear as a published model.
pub const PREVIEW_AGENT_PREFIX: &str = "preview-";

fn publication_order_is_unverified(verified: &bool) -> bool {
    !verified
}

pub use session_profile::{
    ExecutableAgentDelegate, ExecutableAgentEnvironment, ExecutableAgentMcpServer,
    ExecutableAgentProfileSource, ExecutableAgentSessionProfile,
    requested_profile_revision_matches,
};
pub use snapshot_pin::{
    ExecutableSnapshotIdentity, PublishedSnapshotPin, SnapshotPinError,
    SnapshotRegistrationIdentity, published_snapshot_pin,
};

/// One immutable Control publication made available for future Coordinator
/// Session resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableAgentRegistration {
    pub workspace_id: String,
    pub agent_id: String,
    pub source_revision: u64,
    /// Persisted timestamp sampled for this exact publication row, later committed.
    /// Historical commands may
    /// lack the observation; absence must never become a read-time guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at_unix_ms: Option<u64>,
    /// Publication row insertion coordinate paired with the timestamp above.
    /// Historical commands omit both until exact Control row evidence arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_sequence: Option<u64>,
    /// The Control store fenced the insert and commit of this exact row in
    /// the execution Workspace. Old PostgreSQL commands default to false.
    #[serde(default, skip_serializing_if = "publication_order_is_unverified")]
    pub publication_order_verified: bool,
    pub snapshot: ExecutableAgentSnapshot,
    /// Session-facing defaults frozen from the same publication.
    pub session_profile: ExecutableAgentSessionProfile,
}

impl ExecutableAgentRegistration {
    /// Reject a command whose boundary identity does not match the immutable
    /// publication identity it carries.
    pub fn validate(&self) -> Result<(), ExecutableAgentRegistrationError> {
        fn nonblank(value: &str) -> Option<&str> {
            (!value.trim().is_empty()).then_some(value)
        }
        let time_is_paired = match (self.publication_sequence, self.published_at_unix_ms) {
            (None, None) => true,
            (Some(sequence), Some(_)) => sequence != 0,
            _ => false,
        };
        if !time_is_paired
            || (self.publication_order_verified && self.publication_sequence.is_none())
        {
            return Err(ExecutableAgentRegistrationError::Invalid(
                "publication sequence and timestamp must be an exact nonzero pair".into(),
            ));
        }
        published_snapshot_pin(
            SnapshotRegistrationIdentity {
                workspace: nonblank(&self.workspace_id),
                agent: nonblank(&self.agent_id),
                source_revision: self.source_revision,
            },
            ExecutableSnapshotIdentity {
                root_agent: self.snapshot.root_agent_id.0.as_str(),
                published_metadata: !self.snapshot.metadata.is_legacy_default(),
                source_agent: self.snapshot.metadata.source.agent_id.0.as_str(),
                source_revision: self.snapshot.metadata.source.revision,
                envelope_fingerprint: nonblank(&self.snapshot.fingerprint.0),
                resolved_fingerprint: nonblank(&self.snapshot.resolved_spec.catalog_fingerprint.0),
                metadata_fingerprint: nonblank(&self.snapshot.metadata.fingerprint.0),
            },
        )
        .map(|_| ())
        .map_err(|error| ExecutableAgentRegistrationError::Invalid(error.to_string()))
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

    /// Current model references paired with their first durable local
    /// publication time, sorted and deduplicated by reference. The default
    /// supports ephemeral fixtures; the durable catalog overrides it to
    /// include withdrawn historical registrations.
    async fn current_model_publications(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<PublishedModelReference>, ExecutableAgentRegistrationError> {
        let current = self.current_registrations(workspace_id).await?;
        Ok(project_current_model_publications(
            current.iter(),
            current.iter(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedModelReference {
    pub model_reference: String,
    pub first_published_at_unix_ms: Option<u64>,
}

/// The row observations behind one model. A pre-fence PostgreSQL row has a
/// factual timestamp but lacks proof that its sequence equals commit order;
/// one such row is unambiguous, two or more are not.
#[derive(Default)]
struct ModelPublicationHistory {
    rows: std::collections::BTreeMap<u64, (u64, bool)>,
    incomplete: bool,
}

impl ModelPublicationHistory {
    fn observe(&mut self, registration: &ExecutableAgentRegistration) {
        let (Some(sequence), Some(time)) = (
            registration.publication_sequence,
            registration.published_at_unix_ms,
        ) else {
            self.incomplete = true;
            return;
        };
        if sequence == 0 {
            self.incomplete = true;
            return;
        }
        match self.rows.entry(sequence) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((time, registration.publication_order_verified));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let (prior_time, verified) = entry.get_mut();
                if *prior_time != time {
                    self.incomplete = true;
                }
                *verified &= registration.publication_order_verified;
            }
        }
    }

    fn first_time(&self) -> Option<u64> {
        if self.incomplete {
            return None;
        }
        let (&first_sequence, &(time, first_verified)) = self.rows.first_key_value()?;
        let old = self
            .rows
            .iter()
            .filter(|(_, (_, verified))| !*verified)
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>();
        if old.len() > 1 || (old.len() == 1 && (old[0] != first_sequence || first_verified)) {
            return None;
        }
        Some(time)
    }
}

/// Project current reachability and the first time from one immutable
/// registration history. Callers read both collections from the same catalog
/// lock so a concurrent registration cannot split the view.
pub fn project_current_model_publications<'a>(
    current: impl IntoIterator<Item = &'a ExecutableAgentRegistration>,
    history: impl IntoIterator<Item = &'a ExecutableAgentRegistration>,
) -> Vec<PublishedModelReference> {
    fn is_unpublished_preview(registration: &ExecutableAgentRegistration) -> bool {
        registration.agent_id.starts_with(PREVIEW_AGENT_PREFIX)
            && registration.published_at_unix_ms.is_none()
            && registration.publication_sequence.is_none()
    }
    let mut history_by_model: std::collections::BTreeMap<String, ModelPublicationHistory> =
        std::collections::BTreeMap::new();
    for registration in history {
        // Control preview ids are explicitly `preview-*` and have no durable
        // publication row. They can execute a trial Session, but they are not
        // a publication of a model. A real published Agent with that prefix
        // becomes eligible as soon as its row time is recovered.
        if is_unpublished_preview(registration) {
            continue;
        }
        for model in registration_model_references(registration) {
            history_by_model
                .entry(model)
                .or_default()
                .observe(registration);
        }
    }
    let mut reachable = std::collections::BTreeSet::new();
    for registration in current {
        if is_unpublished_preview(registration) {
            continue;
        }
        reachable.extend(registration_model_references(registration));
    }
    reachable
        .into_iter()
        .map(|model_reference| PublishedModelReference {
            first_published_at_unix_ms: history_by_model
                .get(&model_reference)
                .and_then(ModelPublicationHistory::first_time),
            model_reference,
        })
        .collect()
}

/// The sole extraction rule for primary and fallback models in a publication.
pub fn registration_model_references(registration: &ExecutableAgentRegistration) -> Vec<String> {
    let spec = &registration.snapshot.resolved_spec;
    std::iter::once(&spec.model_binding)
        .chain(spec.model_candidates.iter())
        .map(|candidate| candidate.binding().identity().1.to_owned())
        .filter(|model_reference| !model_reference.trim().is_empty())
        .collect()
}

/// Model references reachable from the current executable Agent publications in
/// one Workspace. Primary and fallback candidates share this one normalization
/// rule for Dream readiness and every public model projection.
pub async fn current_model_references(
    registrations: &dyn ExecutableAgentInventorySource,
    workspace_id: &str,
) -> Result<Vec<String>, ExecutableAgentRegistrationError> {
    let mut model_references = registrations
        .current_model_publications(workspace_id)
        .await?
        .into_iter()
        .map(|publication| publication.model_reference)
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
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "test", "model", "native",
            ))
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
            published_at_unix_ms: None,
            publication_sequence: None,
            publication_order_verified: false,
            snapshot,
            session_profile: ExecutableAgentSessionProfile::default(),
        }
    }

    #[test]
    fn publication_commit_coordinate_is_paired_and_nonzero() {
        // Causes: C1 legacy command omits both; C2 only time or only sequence
        // arrives; C3 sequence zero; C4 nonzero sequence with a valid epoch-0
        // time; C5 verified order without its sequence. Effects: E1 C1/C4
        // admit; E2 C2/C3/C5 reject before catalog effect.
        // Decision rules C1=>E1, C2|C3|C5=>E2, C4=>E1.
        let mut command = registration();
        assert!(command.validate().is_ok(), "C1/E1");
        command.published_at_unix_ms = Some(0);
        assert!(command.validate().is_err(), "C2/E2");
        command.published_at_unix_ms = None;
        command.publication_sequence = Some(1);
        assert!(command.validate().is_err(), "C2/E2");
        command.published_at_unix_ms = Some(0);
        command.publication_sequence = Some(0);
        assert!(command.validate().is_err(), "C3/E2");
        command.publication_sequence = Some(1);
        assert!(command.validate().is_ok(), "C4/E1");
        command.publication_order_verified = true;
        assert!(command.validate().is_ok(), "C4/E1 verified");
        command.publication_sequence = None;
        assert!(command.validate().is_err(), "C5/E2");
    }

    #[tokio::test]
    async fn preview_registration_does_not_publish_a_model_time() {
        // Causes: P1 an undated Control preview in the reserved namespace,
        // P2 a real publication of the same model, P3 a real publication whose
        // Agent id happens to share that namespace. Effects: E1 preview alone
        // is absent; E2 preview cannot poison the real first time; E3 a dated
        // real publication remains visible. Rules P1=>E1, P1+P2=>E2, P3=>E3.
        let mut preview = registration();
        preview.agent_id = "preview-trial".into();
        assert!(
            project_current_model_publications([&preview], [&preview]).is_empty(),
            "E1"
        );
        assert!(
            current_model_references(&Inventory(vec![preview.clone()]), "workspace-a")
                .await
                .unwrap()
                .is_empty(),
            "E1 Dream readiness uses the same rule"
        );
        let mut published = registration();
        published.published_at_unix_ms = Some(1_000);
        published.publication_sequence = Some(2);
        published.publication_order_verified = true;
        assert_eq!(
            project_current_model_publications([&preview, &published], [&preview, &published])[0]
                .first_published_at_unix_ms,
            Some(1_000),
            "E2"
        );
        preview.published_at_unix_ms = Some(500);
        preview.publication_sequence = Some(1);
        preview.publication_order_verified = true;
        assert_eq!(
            project_current_model_publications([&preview, &published], [&preview, &published])[0]
                .first_published_at_unix_ms,
            Some(500),
            "E3"
        );
    }

    #[test]
    fn pre_fence_postgres_history_reports_only_unambiguous_first_time() {
        // Causes: H1 one historical row has a real time but no commit-order
        // proof; H2 a second such row might have committed first; H3 newer
        // fenced row has a backwards clock; H4 all rows are fenced; H5 two
        // old rows precede a newer fenced row; H6 two rows claim one sequence
        // with different times.
        // Effects: E1 H1 returns its time; E2 H1+H2 is unknown; E3 H1+H3
        // keeps the sole old row's time; E4 H4 chooses first sequence, not
        // minimum time; E5 H5 remains unknown; E6 H6 rejects conflicting
        // sequence evidence. These are the v1
        // model-directory availability rules.
        let mut old = registration();
        old.publication_sequence = Some(1);
        old.published_at_unix_ms = Some(2_000);
        let mut later = registration();
        later.source_revision = 8;
        later.snapshot.metadata.source.revision = 8;
        later.publication_sequence = Some(2);
        later.published_at_unix_ms = Some(1_000);
        assert_eq!(
            project_current_model_publications([&old], [&old])[0].first_published_at_unix_ms,
            Some(2_000),
            "E1"
        );
        assert_eq!(
            project_current_model_publications([&later], [&old, &later])[0]
                .first_published_at_unix_ms,
            None,
            "E2"
        );
        later.publication_order_verified = true;
        assert_eq!(
            project_current_model_publications([&later], [&later, &old])[0]
                .first_published_at_unix_ms,
            Some(2_000),
            "E3 recovery order independent"
        );
        let mut conflicting = old.clone();
        conflicting.published_at_unix_ms = Some(3_000);
        assert_eq!(
            project_current_model_publications([&later], [&old, &conflicting, &later])[0]
                .first_published_at_unix_ms,
            None,
            "E6"
        );
        let mut newest = registration();
        newest.publication_sequence = Some(3);
        newest.published_at_unix_ms = Some(500);
        newest.publication_order_verified = true;
        assert_eq!(
            project_current_model_publications([&newest], [&newest, &old, &later])[0]
                .first_published_at_unix_ms,
            Some(2_000),
            "E3 one old plus two fenced rows"
        );
        let mut old_second = registration();
        old_second.publication_sequence = Some(2);
        old_second.published_at_unix_ms = Some(1_500);
        assert_eq!(
            project_current_model_publications([&newest], [&old, &old_second, &newest],)[0]
                .first_published_at_unix_ms,
            None,
            "E5 two old rows remain ambiguous after a new fenced row"
        );
        old.publication_order_verified = true;
        assert_eq!(
            project_current_model_publications([&later], [&later, &old])[0]
                .first_published_at_unix_ms,
            Some(2_000),
            "E4"
        );
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
                published_at_unix_ms: None,
                publication_sequence: None,
                publication_order_verified: false,
                snapshot: ExecutableAgentSnapshot::builder(agent)
                    .model(awaken_runtime_contract::resolved::ModelBinding::new(
                        "provider", primary, "native",
                    ))
                    .model_candidates(fallbacks.iter().map(|model| {
                        // C4: a blank model reference is representable only
                        // on an external executor that keeps its default
                        // model; the binding admission refuses a native
                        // binding without one.
                        if model.is_empty() {
                            awaken_runtime_contract::resolved::ModelBinding::new(
                                "provider",
                                "",
                                "acp:codex",
                            )
                        } else {
                            awaken_runtime_contract::resolved::ModelBinding::new(
                                "provider", *model, "native",
                            )
                        }
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
