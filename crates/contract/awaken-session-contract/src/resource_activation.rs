//! Durable Session resource-activation state.
//!
//! This is application state for crash recovery, not an authorization decision
//! and not a new resource aggregate. Records contain logical resource identity
//! only: no principal, role, policy, API key, credential value, host path,
//! Project, or WorkUnit.

use awaken_resource_contract::{InputResourceId, ResourceAccess};
use serde::{Deserialize, Serialize};

use crate::{ResolvedInput, ResolvedInputSource, ResolvedSessionResources};

/// One Session-local realization of one resolved input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResourceActivation {
    pub activation_id: String,
    pub session_id: String,
    pub revision: u64,
    pub binding_id: awaken_resource_contract::BindingId,
    pub resource_id: InputResourceId,
    pub access: ResourceAccess,
    pub state: ActivationState,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Recoverable activation lifecycle. `Failed` and `Released` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationState {
    Prepared,
    Active,
    Releasing,
    Released,
    Failed,
}

/// The complete durable resource state of one Session.
///
/// `active` is the manifest visible to the Session. `pending` is written before
/// an external realization attempt and cleared only after that attempt either
/// commits or is successfully rolled back. Mutable Memory content versions and
/// Git revisions never enter either manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResourceState {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub active: ResolvedSessionResources,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<ResolvedSessionResources>,
    #[serde(default)]
    /// Unsettled physical-effect authority plus the latest diagnostic generation
    /// for each terminal outcome. Older Released/Failed generations are not an
    /// audit log: root cleanup receipts own terminal proof, so retaining every
    /// replacement here would duplicate history and make recovery cost unbounded.
    pub activations: Vec<SessionResourceActivation>,
    /// Exact removed Repository inputs whose Session-owned Registry/Vault
    /// participants still require retirement. The intent is created atomically
    /// with activation of the replacement generation and remains a retention
    /// edge until the application records successful idempotent cleanup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repository_retirements: Vec<ResolvedInput>,
}

/// Physical Resource pins retained across an active/pending replacement. This
/// is intentionally not an effective [`ResolvedSessionResources`] manifest:
/// both generations may contain the same binding identity with different
/// physical targets, and both must remain retained until commit or rollback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionResourceReferences {
    inputs: Vec<ResolvedInput>,
    skills: Vec<crate::ResolvedSkillBinding>,
}

impl SessionResourceReferences {
    #[must_use]
    pub fn inputs(&self) -> &[ResolvedInput] {
        &self.inputs
    }

    #[must_use]
    pub fn skills(&self) -> &[crate::ResolvedSkillBinding] {
        &self.skills
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResourceActivationError {
    #[error("a Session resource activation or Repository retirement is already pending")]
    Pending,
    #[error("no Session resource activation is pending")]
    NoPending,
    #[error("Session resource activation revision is exhausted")]
    RevisionExhausted,
    #[error("Session resource activation state is inconsistent: {0}")]
    Invalid(String),
}

impl SessionResourceState {
    /// Remove superseded terminal diagnostics without touching any activation
    /// that can still authorize or require a physical effect.
    ///
    /// The latest Released and latest Failed generations remain independently
    /// visible. Keeping them by state preserves the most recent success and
    /// failure diagnostics while bounding settled history to at most two
    /// manifest generations.
    pub fn compact_settled_activations(&mut self) -> bool {
        let latest_released = self
            .activations
            .iter()
            .filter(|activation| activation.state == ActivationState::Released)
            .map(|activation| activation.revision)
            .max();
        let latest_failed = self
            .activations
            .iter()
            .filter(|activation| activation.state == ActivationState::Failed)
            .map(|activation| activation.revision)
            .max();
        let before = self.activations.len();
        self.activations
            .retain(|activation| match activation.state {
                ActivationState::Prepared
                | ActivationState::Active
                | ActivationState::Releasing => true,
                ActivationState::Released => Some(activation.revision) == latest_released,
                ActivationState::Failed => Some(activation.revision) == latest_failed,
            });
        self.activations.len() != before
    }

    /// The manifest requested by the Session. While a replacement is pending,
    /// `active` remains the installed Runtime generation and `pending` is the
    /// sole desired generation accepted by subsequent pre-attempt commands.
    #[must_use]
    pub fn desired(&self) -> &ResolvedSessionResources {
        self.pending.as_ref().unwrap_or(&self.active)
    }

    /// Every Resource identity that must remain physically retained. During a
    /// replacement both the installed and desired manifests are live facts;
    /// dropping either side before the phase commits opens a reclamation race.
    #[must_use]
    pub fn resource_references(&self) -> SessionResourceReferences {
        let mut inputs = self.active.inputs().to_vec();
        let mut skills = self.active.skills().to_vec();
        if let Some(pending) = &self.pending {
            inputs.extend_from_slice(pending.inputs());
            skills.extend_from_slice(pending.skills());
        }
        inputs.extend_from_slice(&self.repository_retirements);
        SessionResourceReferences { inputs, skills }
    }

    /// Whether this aggregate owns any durable Resource-retention edge.
    #[must_use]
    pub fn has_references(&self) -> bool {
        let referenced = self.resource_references();
        !referenced.inputs().is_empty() || !referenced.skills().is_empty()
    }

    /// Initialize activation state from the currently installed manifest.
    #[must_use]
    pub fn from_active(active: ResolvedSessionResources) -> Self {
        Self {
            revision: u64::from(!active.inputs().is_empty() || !active.skills().is_empty()),
            active,
            pending: None,
            activations: Vec::new(),
            repository_retirements: Vec::new(),
        }
    }

    /// Persist a new desired manifest before invoking any external realizer.
    pub fn prepare(
        &mut self,
        session_id: &str,
        desired: ResolvedSessionResources,
    ) -> Result<u64, ResourceActivationError> {
        if self.has_repository_retirement_conflict(&desired)
            || self.pending.is_some()
            || self.activations.iter().any(|activation| {
                matches!(
                    activation.state,
                    ActivationState::Prepared | ActivationState::Releasing
                )
            })
        {
            return Err(ResourceActivationError::Pending);
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(ResourceActivationError::RevisionExhausted)?;
        for activation in &mut self.activations {
            if activation.state == ActivationState::Active {
                activation.state = ActivationState::Releasing;
            }
        }
        self.activations.extend(
            desired
                .inputs()
                .iter()
                .map(|input| prepared_activation(session_id, revision, input)),
        );
        self.revision = revision;
        self.pending = Some(desired);
        Ok(revision)
    }

    /// Fence one external attempt durably. A retry increments the same records;
    /// it never creates a second logical activation.
    pub fn start_attempt(&mut self) -> Result<(), ResourceActivationError> {
        if self.pending.is_none() {
            return Err(ResourceActivationError::NoPending);
        }
        for activation in self.activations.iter_mut().filter(|activation| {
            activation.revision == self.revision && activation.state == ActivationState::Prepared
        }) {
            activation.attempts = activation.attempts.saturating_add(1);
            activation.last_error = None;
        }
        Ok(())
    }

    /// Amend a pending generation before any external realization attempt has
    /// observed it. The revision remains stable because no effect or receipt can
    /// yet name this generation. Once an attempt starts, the ordinary pending
    /// fence remains absorbing until commit/rollback/recovery settles it.
    pub fn revise_unattempted_pending(
        &mut self,
        session_id: &str,
        desired: ResolvedSessionResources,
    ) -> Result<u64, ResourceActivationError> {
        if self.pending.is_none() {
            return Err(ResourceActivationError::NoPending);
        }
        if self.has_repository_retirement_conflict(&desired) {
            return Err(ResourceActivationError::Pending);
        }
        let mutable = self.activations.iter().all(|activation| {
            activation.revision != self.revision
                || (activation.state == ActivationState::Prepared
                    && activation.attempts == 0
                    && activation.lease_expires_at_unix_ms.is_none())
        });
        if !mutable {
            return Err(ResourceActivationError::Pending);
        }
        self.activations.retain(|activation| {
            activation.revision != self.revision || activation.state != ActivationState::Prepared
        });
        self.activations.extend(
            desired
                .inputs()
                .iter()
                .map(|input| prepared_activation(session_id, self.revision, input)),
        );
        self.pending = Some(desired);
        Ok(self.revision)
    }

    /// Commit a successful whole-manifest realization.
    pub fn commit(&mut self) -> Result<(), ResourceActivationError> {
        let desired = self
            .pending
            .take()
            .ok_or(ResourceActivationError::NoPending)?;
        let desired_repository_ids = desired
            .inputs()
            .iter()
            .filter_map(repository_id)
            .collect::<std::collections::BTreeSet<_>>();
        for input in self.active.inputs().iter().filter(|input| {
            repository_id(input).is_some_and(|id| !desired_repository_ids.contains(id))
        }) {
            upsert_repository_retirement(&mut self.repository_retirements, input.clone());
        }
        for activation in &mut self.activations {
            match activation.state {
                ActivationState::Prepared if activation.revision == self.revision => {
                    activation.state = ActivationState::Active;
                    activation.last_error = None;
                }
                ActivationState::Releasing => activation.state = ActivationState::Released,
                _ => {}
            }
        }
        self.active = desired;
        self.compact_settled_activations();
        Ok(())
    }

    /// Exact pending Repository retirement candidates. Ownership is deliberately
    /// validated by the Session application against the Resource Registry; this
    /// aggregate stores lifecycle intent and never duplicates catalog policy.
    #[must_use]
    pub fn repository_retirements(&self) -> &[ResolvedInput] {
        &self.repository_retirements
    }

    /// Project the one canonical terminal Repository retirement plan from all
    /// Resource generations still retained by this Session. The same
    /// repository identity appears once; when generations pin different
    /// credential revisions, the newest exact pin owns retirement.
    #[must_use]
    pub(crate) fn terminal_repository_retirement_plan(&self) -> Vec<ResolvedInput> {
        let mut plan = Vec::new();
        for input in std::iter::once(&self.active)
            .chain(self.pending.iter())
            .flat_map(ResolvedSessionResources::inputs)
            .chain(self.repository_retirements.iter())
        {
            upsert_repository_retirement(&mut plan, input.clone());
        }
        plan.sort_by(|left, right| repository_id(left).cmp(&repository_id(right)));
        plan
    }

    /// Persist the exact terminal plan through the existing Repository
    /// retirement intent list. The ordinary retirement reconciler remains the
    /// only participant-effect owner; terminal cleanup does not scan or delete
    /// Repository/Vault state through a second path.
    pub fn ensure_terminal_repository_retirements(&mut self) -> bool {
        let plan = self.terminal_repository_retirement_plan();
        if self.repository_retirements == plan {
            return false;
        }
        self.repository_retirements = plan;
        true
    }

    #[must_use]
    pub fn has_repository_retirements(&self) -> bool {
        !self.repository_retirements.is_empty()
    }

    fn has_repository_retirement_conflict(&self, desired: &ResolvedSessionResources) -> bool {
        desired.inputs().iter().filter_map(repository_id).any(|id| {
            self.repository_retirements
                .iter()
                .any(|retirement| repository_id(retirement) == Some(id))
        })
    }

    /// Record completion of one exact retirement without clearing a newer pin
    /// for the same Repository identity.
    pub fn complete_repository_retirement(&mut self, completed: &ResolvedInput) -> bool {
        let before = self.repository_retirements.len();
        self.repository_retirements
            .retain(|candidate| candidate != completed);
        self.repository_retirements.len() != before
    }

    /// Record a failed attempt that must be retried by the reconciler.
    pub fn note_retryable_failure(
        &mut self,
        error: impl Into<String>,
    ) -> Result<(), ResourceActivationError> {
        if self.pending.is_none() {
            return Err(ResourceActivationError::NoPending);
        }
        let error = error.into();
        for activation in self.activations.iter_mut().filter(|activation| {
            activation.revision == self.revision && activation.state == ActivationState::Prepared
        }) {
            activation.last_error = Some(error.clone());
        }
        Ok(())
    }

    /// Complete a successful rollback to the previously active manifest.
    pub fn rollback(&mut self, error: impl Into<String>) -> Result<(), ResourceActivationError> {
        self.pending
            .take()
            .ok_or(ResourceActivationError::NoPending)?;
        let error = error.into();
        for activation in &mut self.activations {
            match activation.state {
                ActivationState::Prepared if activation.revision == self.revision => {
                    activation.state = ActivationState::Failed;
                    activation.last_error = Some(error.clone());
                }
                ActivationState::Releasing => activation.state = ActivationState::Active,
                _ => {}
            }
        }
        self.compact_settled_activations();
        Ok(())
    }

    /// Adopt a legacy active manifest after it has been re-realized once.
    pub fn adopt_legacy_active(&mut self, session_id: &str) {
        if !self.activations.is_empty() || self.active.inputs().is_empty() {
            return;
        }
        self.revision = self.revision.max(1);
        self.activations
            .extend(self.active.inputs().iter().map(|input| {
                let mut activation = prepared_activation(session_id, self.revision, input);
                activation.state = ActivationState::Active;
                activation.attempts = 1;
                activation
            }));
    }

    /// Commit the intent to release all active realizations before teardown.
    pub fn begin_release(&mut self) -> Result<(), ResourceActivationError> {
        if self.pending.is_some() {
            return Err(ResourceActivationError::Pending);
        }
        for activation in &mut self.activations {
            if activation.state == ActivationState::Active {
                activation.state = ActivationState::Releasing;
            }
        }
        Ok(())
    }

    /// Record idempotent completion of all outstanding releases.
    pub fn complete_release(&mut self) {
        for activation in &mut self.activations {
            if activation.state == ActivationState::Releasing {
                activation.state = ActivationState::Released;
            }
        }
        self.compact_settled_activations();
    }

    /// A terminal Session was torn down while a replacement was pending. The
    /// previous generation is released and the never-committed generation is
    /// failed; no manifest becomes newly active after termination.
    pub fn complete_terminal_release(&mut self, reason: impl Into<String>) {
        self.pending = None;
        self.active = ResolvedSessionResources::default();
        self.repository_retirements.clear();
        let reason = reason.into();
        for activation in &mut self.activations {
            match activation.state {
                ActivationState::Prepared => {
                    activation.state = ActivationState::Failed;
                    activation.last_error = Some(reason.clone());
                }
                ActivationState::Active | ActivationState::Releasing => {
                    activation.state = ActivationState::Released;
                }
                ActivationState::Released | ActivationState::Failed => {}
            }
        }
        self.compact_settled_activations();
    }

    #[must_use]
    pub fn has_active(&self) -> bool {
        self.activations
            .iter()
            .any(|activation| activation.state == ActivationState::Active)
    }

    /// Generation of the manifest currently visible to the Runtime. During a
    /// replacement, `revision` already names the pending generation while the
    /// prior Active/Releasing activation still owns the installed manifest.
    #[must_use]
    pub fn active_revision(&self) -> u64 {
        self.activations
            .iter()
            .filter(|activation| {
                matches!(
                    activation.state,
                    ActivationState::Active | ActivationState::Releasing
                )
            })
            .map(|activation| activation.revision)
            .max()
            .unwrap_or_else(|| {
                self.revision
                    .saturating_sub(u64::from(self.pending.is_some()))
            })
    }

    /// Return the one installed Runtime generation as an inseparable revision
    /// and manifest pair. `revision` remains a monotonic attempted-generation
    /// watermark after rollback, so callers must not pair it with `active`
    /// independently.
    #[must_use]
    pub fn active_generation(&self) -> (u64, &ResolvedSessionResources) {
        (self.active_revision(), &self.active)
    }

    /// Return the generation that the Session currently asks realization to
    /// converge. A pending replacement owns the latest revision; otherwise the
    /// installed generation remains authoritative even after a failed newer
    /// attempt advanced the monotonic watermark.
    #[must_use]
    pub fn desired_generation(&self) -> (u64, &ResolvedSessionResources) {
        match &self.pending {
            Some(pending) => (self.revision, pending),
            None => self.active_generation(),
        }
    }

    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        self.pending.is_some()
            || self.has_repository_retirements()
            || self.activations.iter().any(|activation| {
                matches!(
                    activation.state,
                    ActivationState::Prepared | ActivationState::Releasing
                )
            })
    }
}

fn repository_id(input: &ResolvedInput) -> Option<&str> {
    match &input.source {
        ResolvedInputSource::Repository { repository_id, .. } => Some(repository_id.as_str()),
        ResolvedInputSource::File { .. } | ResolvedInputSource::MemoryStore { .. } => None,
    }
}

fn repository_credential_revision(input: &ResolvedInput) -> Option<u64> {
    match &input.source {
        ResolvedInputSource::Repository { credential, .. } => credential
            .as_deref()
            .map(|credential| credential.access.credential.revision),
        ResolvedInputSource::File { .. } | ResolvedInputSource::MemoryStore { .. } => None,
    }
}

fn upsert_repository_retirement(retirements: &mut Vec<ResolvedInput>, candidate: ResolvedInput) {
    let Some(candidate_id) = repository_id(&candidate) else {
        return;
    };
    if let Some(existing) = retirements
        .iter_mut()
        .find(|existing| repository_id(existing) == Some(candidate_id))
    {
        if repository_credential_revision(&candidate) > repository_credential_revision(existing) {
            *existing = candidate;
        }
    } else {
        retirements.push(candidate);
    }
}

fn prepared_activation(
    session_id: &str,
    revision: u64,
    input: &ResolvedInput,
) -> SessionResourceActivation {
    let resource_id = match &input.source {
        ResolvedInputSource::File { file_id } => InputResourceId::File(file_id.clone()),
        ResolvedInputSource::MemoryStore {
            memory_store_id, ..
        } => InputResourceId::MemoryStore(memory_store_id.clone()),
        ResolvedInputSource::Repository { repository_id, .. } => {
            InputResourceId::Repository(repository_id.clone())
        }
    };
    SessionResourceActivation {
        activation_id: format!("{session_id}:{revision}:{}", input.binding_id),
        session_id: session_id.to_string(),
        revision,
        binding_id: input.binding_id.clone(),
        resource_id,
        access: input.access,
        state: ActivationState::Prepared,
        attempts: 0,
        lease_expires_at_unix_ms: None,
        last_error: None,
    }
}

#[cfg(test)]
mod tests {
    use awaken_credential_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_resource_contract::{BindingId, ConfigVersion, FileId};

    use super::*;

    fn manifest(id: &str) -> ResolvedSessionResources {
        ResolvedSessionResources::try_new(
            vec![ResolvedInput {
                binding_id: BindingId::from(format!("binding-{id}")),
                source: ResolvedInputSource::File {
                    file_id: FileId::from(format!("file-{id}")),
                },
                mount_path: format!("/inputs/{id}"),
                access: ResourceAccess::ReadOnly,
                instructions: None,
            }],
            Vec::new(),
        )
        .unwrap()
    }

    fn repository_manifest(id: &str) -> ResolvedSessionResources {
        ResolvedSessionResources::try_new(vec![repository_input(id, None)], Vec::new()).unwrap()
    }

    fn repository_input(id: &str, credential_revision: Option<u64>) -> ResolvedInput {
        let repository_id = format!("managed:session-1:repository:{id}").into();
        let remote_url = format!("https://example.test/{id}.git");
        let credential_binding = credential_revision.map(|_| format!("credential-{id}"));
        let credential = credential_revision.map(|revision| {
            let holder = PlaintextHolder::new(
                PlaintextBoundary::Worker,
                "spiffe://example.test/session-resource-worker",
            );
            Box::new(crate::ResolvedRepositoryCredential {
                access: CredentialAccess::new(
                    CredentialRef {
                        id: format!("credential-{id}"),
                        revision,
                    },
                    CredentialMaterialSource::ControlPlaneReference,
                    crate::repository_transport_credential_usage(),
                    CredentialExecutionPolicy::exact(
                        holder.clone(),
                        ModelExposurePolicy::Forbidden,
                    ),
                )
                .with_target(crate::repository_transport_credential_target(&remote_url).unwrap()),
                selected_plaintext_holder: holder,
            })
        });
        ResolvedInput {
            binding_id: BindingId::from(format!("binding-{id}")),
            source: ResolvedInputSource::Repository {
                repository_id,
                config: awaken_resource_contract::RepositoryConfigVersion {
                    repository_id: format!("managed:session-1:repository:{id}").into(),
                    version: ConfigVersion::INITIAL,
                    remote_url,
                    credential_binding,
                    initial_branch: Some("main".into()),
                    initial_commit: None,
                    clone_policy: Default::default(),
                },
                credential,
            },
            mount_path: format!("/workspace/{id}"),
            access: ResourceAccess::ReadOnly,
            instructions: None,
        }
    }

    #[test]
    fn prepare_attempt_commit_is_exactly_one_active_generation() {
        let mut state = SessionResourceState::default();
        assert_eq!(state.prepare("session-1", manifest("a")).unwrap(), 1);
        state.start_attempt().unwrap();
        state.commit().unwrap();
        assert_eq!(state.active, manifest("a"));
        assert!(state.pending.is_none());
        assert_eq!(state.activations[0].state, ActivationState::Active);
        assert_eq!(state.activations[0].attempts, 1);
    }

    #[test]
    fn replacement_keeps_old_active_until_commit_and_records_release() {
        let mut state = SessionResourceState::default();
        state.prepare("session-1", manifest("a")).unwrap();
        state.commit().unwrap();
        state.prepare("session-1", manifest("b")).unwrap();
        assert_eq!(state.active_revision(), 1);
        assert_eq!(state.active, manifest("a"));
        assert_eq!(state.activations[0].state, ActivationState::Releasing);
        state.commit().unwrap();
        assert_eq!(state.active_revision(), 2);
        assert_eq!(state.active, manifest("b"));
        assert_eq!(state.activations[0].state, ActivationState::Released);
        assert_eq!(state.activations[1].state, ActivationState::Active);
    }

    /// Repository-retirement cause/effect graph: C1 an Active Repository is
    /// absent from the committed successor; C2 realization rolls back instead;
    /// C3 the same Repository is requested while its retirement is unsettled;
    /// C4 cleanup has durably cleared that intent. Effects: E1 commit atomically
    /// retains the exact removed input as cleanup intent; E2 rollback creates no
    /// intent; E3 same-identity reintroduction fails closed before a new pending
    /// generation exists; E4 an unsettled intent remains a physical Resource
    /// retention edge; E5 retry after durable cleanup may prepare normally.
    ///
    /// | Rule | Removed | Settlement | Reintroduced | Effect |
    /// |---|---|---|---|---|
    /// | R1 | yes | commit | no | E1 + E4 |
    /// | R2 | yes | rollback | no | E2 |
    /// | R3 | prior intent | prepare | yes | E3 + E4 |
    /// | R4 | intent cleared | prepare | yes | E5 |
    #[test]
    fn repository_retirement_intent_follows_the_manifest_commit_boundary() {
        let repository = repository_manifest("source");

        let mut committed = SessionResourceState::from_active(repository.clone());
        committed
            .prepare("session-1", ResolvedSessionResources::default())
            .unwrap();
        committed.commit().unwrap();
        assert_eq!(
            committed.repository_retirements(),
            repository.inputs(),
            "R1/E1"
        );
        assert!(committed.has_references(), "R1/E4");

        let mut rolled_back = SessionResourceState::from_active(repository.clone());
        rolled_back
            .prepare("session-1", ResolvedSessionResources::default())
            .unwrap();
        rolled_back.rollback("injected").unwrap();
        assert!(rolled_back.repository_retirements().is_empty(), "R2/E2");

        assert_eq!(
            committed.prepare("session-1", repository.clone()),
            Err(ResourceActivationError::Pending),
            "R3/E3"
        );
        assert!(committed.pending.is_none(), "R3/E3");
        assert!(committed.has_references(), "R3/E4");

        assert!(committed.complete_repository_retirement(&repository.inputs()[0]));
        committed.prepare("session-1", repository).unwrap();
        assert!(committed.pending.is_some(), "R4/E5");
    }

    /// Terminal Repository-plan cause/effect graph: C1 Repository inputs may
    /// exist in Active, Pending, and the retained retirement intent; C2 the
    /// same identity may occur in more than one generation; C3 those exact
    /// pins may carry different credential revisions; C4 the retained list is
    /// either not yet the projected plan or already equals it. Effects: E1 a
    /// state with no Repository input has an empty plan and needs no write; E2
    /// the plan is the sorted identity union and excludes other Resource kinds;
    /// E3 each duplicate selects the highest credential revision; E4 the first
    /// ensure stores the complete plan; E5 exact replay is inert and preserves
    /// that complete list as the later Disposing receipt's proof input.
    ///
    /// | Rule | Repository sources | Duplicate identity | Pin revisions | Retained list | Effect |
    /// |---|---|---|---|---|---|
    /// | T0 | none | no | n/a | empty | E1 |
    /// | T1 | Active + Pending + retirement | yes | different | partial | E2 + E3 + E4 |
    /// | T2 | Active + Pending + retirement | yes | different | exact plan | E5 |
    ///
    /// Constraint: Active and Pending manifests remain unchanged; the existing
    /// retirement list is the only durable terminal-plan owner.
    #[test]
    fn terminal_repository_plan_is_canonical_and_idempotently_retained() {
        let mut empty = SessionResourceState::default();
        assert!(
            empty.terminal_repository_retirement_plan().is_empty(),
            "T0/E1"
        );
        assert!(!empty.ensure_terminal_repository_retirements(), "T0/E1");

        let alpha_active = repository_input("alpha", Some(2));
        let beta_pending = repository_input("beta", Some(1));
        let delta_retained = repository_input("delta", Some(2));
        let zeta_active = repository_input("zeta", Some(1));
        let zeta_pending = repository_input("zeta", Some(4));
        let zeta_retained = repository_input("zeta", Some(3));
        let alpha_retained = repository_input("alpha", Some(1));
        let file = manifest("non-repository").inputs()[0].clone();
        let active = ResolvedSessionResources::try_new(
            vec![zeta_active, file, alpha_active.clone()],
            Vec::new(),
        )
        .unwrap();
        let pending = ResolvedSessionResources::try_new(
            vec![zeta_pending.clone(), beta_pending.clone()],
            Vec::new(),
        )
        .unwrap();
        let mut state = SessionResourceState {
            revision: 2,
            active: active.clone(),
            pending: Some(pending.clone()),
            activations: Vec::new(),
            repository_retirements: vec![zeta_retained, delta_retained.clone(), alpha_retained],
        };
        let expected = vec![alpha_active, beta_pending, delta_retained, zeta_pending];

        assert_eq!(
            state.terminal_repository_retirement_plan(),
            expected,
            "T1/E2 + T1/E3"
        );
        assert!(state.ensure_terminal_repository_retirements(), "T1/E4");
        assert_eq!(state.repository_retirements(), expected, "T1/E4");
        assert_eq!(state.active, active, "Active authority is unchanged");
        assert_eq!(
            state.pending.as_ref(),
            Some(&pending),
            "Pending authority is unchanged"
        );

        let retained = state.repository_retirements().to_vec();
        assert!(!state.ensure_terminal_repository_retirements(), "T2/E5");
        assert_eq!(state.repository_retirements(), retained, "T2/E5");
        assert_eq!(
            state.terminal_repository_retirement_plan(),
            retained,
            "T2/E5"
        );
    }

    /// Visible-generation cause/effect graph. C1=active mounted inputs; C2=active
    /// Skill pins; C3=pending replacement; C4=input activation record exists.
    /// E1 an empty active manifest stays at legacy generation zero; E2 either
    /// resource family preserves generation one; E3 reports the installed
    /// generation, never the merely desired generation. Constraints: input and
    /// Skill collections are independent, but either makes the active manifest
    /// non-empty; revision and manifest remain one selected generation pair.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | Effect |
    /// |---|---|---|---|---|---|
    /// | V0 | no | no | no | no | E1: revision 0 |
    /// | V1 | yes | no | no | no | E2: revision 1 |
    /// | V2 | no | yes | no | no | E2: revision 1 |
    /// | V3 | yes | no | yes | no | E3: revision 1 |
    /// | V4 | yes | no | yes | yes | E3: activation revision 1 |
    #[test]
    fn active_revision_tracks_the_visible_manifest_during_replacement() {
        assert_eq!(
            SessionResourceState::from_active(ResolvedSessionResources::default())
                .active_revision(),
            0,
            "V0/E1"
        );

        let mut legacy = SessionResourceState::from_active(manifest("legacy"));
        assert_eq!(legacy.active_revision(), 1, "V1/E2");

        let skill_only = ResolvedSessionResources::try_new(
            Vec::new(),
            vec![crate::ResolvedSkillBinding {
                kind: awaken_agent_contract::AgentSkillKind::Custom,
                skill_id: "skill-only".into(),
                version: 1,
                bundle_sha256: "sha256-skill-only-v1".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            SessionResourceState::from_active(skill_only).active_revision(),
            1,
            "V2/E2"
        );

        legacy.prepare("session-legacy", manifest("next")).unwrap();
        assert_eq!(legacy.active_revision(), 1, "V3/E3");

        let mut activated = SessionResourceState::default();
        activated.prepare("session-1", manifest("a")).unwrap();
        activated.commit().unwrap();
        activated.prepare("session-1", manifest("b")).unwrap();
        assert_eq!(activated.active_revision(), 1, "V4/E3");
    }

    /// Rollback generation cause/effect graph: C1 generation 1 is installed;
    /// C2 generation 2 is pending; C3 its attempt fails and rolls back. Effects:
    /// E1 pending pairs desired generation 2 while active remains generation 1;
    /// E2 rollback retains watermark 2 but pairs both views with generation 1;
    /// E3 only generation 2 becomes Failed.
    ///
    /// | Rule | C1 | C2 | C3 | Watermark | Active pair | Desired pair |
    /// |---|---|---|---|---|---|---|
    /// | R1 | yes | yes | no | 2 | (1, a) | (2, b) |
    /// | R2 | yes | cleared | rollback | 2 | (1, a) | (1, a) |
    /// Constraints/invariants: rollback cannot lower the generation watermark,
    /// mutate the prior active manifest, or fail a generation that was active.
    #[test]
    fn rollback_restores_old_authority_and_fails_only_new_generation() {
        let mut state = SessionResourceState::default();
        state.prepare("session-1", manifest("a")).unwrap();
        state.commit().unwrap();
        state.prepare("session-1", manifest("b")).unwrap();
        assert_eq!(state.active_generation(), (1, &manifest("a")), "R1/E1");
        assert_eq!(state.desired_generation(), (2, &manifest("b")), "R1/E1");
        state.start_attempt().unwrap();
        state.rollback("clone failed").unwrap();
        assert_eq!(state.revision, 2, "R2/E2");
        assert_eq!(state.active, manifest("a"));
        assert!(state.pending.is_none());
        assert_eq!(state.active_generation(), (1, &manifest("a")), "R2/E2");
        assert_eq!(state.desired_generation(), (1, &manifest("a")), "R2/E2");
        assert_eq!(state.activations[0].state, ActivationState::Active);
        assert_eq!(state.activations[1].state, ActivationState::Failed);
        assert_eq!(
            state.activations[1].last_error.as_deref(),
            Some("clone failed")
        );
    }

    #[test]
    fn pending_transition_cannot_be_overwritten() {
        let mut state = SessionResourceState::default();
        state.prepare("session-1", manifest("a")).unwrap();
        assert_eq!(
            state.prepare("session-1", manifest("b")),
            Err(ResourceActivationError::Pending)
        );
        assert_eq!(state.pending, Some(manifest("a")));
    }

    /// Pending-amendment cause/effect graph: C1 a desired generation exists;
    /// C2 no external attempt/lease has observed it; C3 an attempt has started.
    /// E1 atomically replaces the desired manifest in the same revision; E2
    /// rejects without mutation; E3 absence is not synthesized into a second
    /// preparation path. C2 and C3 are mutually exclusive.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | P1 | yes | yes | no | E1 |
    /// | P2 | yes | no | yes | E2 |
    /// | P3 | no | n/a | no | E3 |
    #[test]
    fn only_an_unattempted_pending_generation_can_be_revised() {
        let mut absent = SessionResourceState::default();
        assert_eq!(
            absent.revise_unattempted_pending("session-1", manifest("b")),
            Err(ResourceActivationError::NoPending),
            "P3"
        );

        let mut mutable = SessionResourceState::default();
        let revision = mutable.prepare("session-1", manifest("a")).unwrap();
        assert_eq!(
            mutable
                .revise_unattempted_pending("session-1", manifest("b"))
                .unwrap(),
            revision,
            "P1"
        );
        assert_eq!(mutable.desired(), &manifest("b"), "P1/E1");
        assert_eq!(mutable.activations.len(), 1, "P1 replaces, never appends");
        assert_eq!(mutable.activations[0].attempts, 0, "P1");

        mutable.start_attempt().unwrap();
        let before = mutable.clone();
        assert_eq!(
            mutable.revise_unattempted_pending("session-1", manifest("c")),
            Err(ResourceActivationError::Pending),
            "P2"
        );
        assert_eq!(mutable, before, "P2/E2");
    }

    /// Retention cause/effect graph: C1 an installed generation exists; C2 a
    /// replacement is pending; C3 the replacement commits or rolls back. E1
    /// retains the installed set, E2 retains the union so neither side can be
    /// reclaimed during the phase, and E3 shrinks to the terminal winner.
    ///
    /// | Rule | C1 | C2 | C3 | Referenced manifests |
    /// |---|---|---|---|---|
    /// | G1 | no | no | n/a | empty |
    /// | G2 | yes | no | n/a | installed |
    /// | G3 | yes | yes | unsettled | installed + desired |
    /// | G4 | yes | no | commit | desired only |
    /// | G5 | yes | no | rollback | installed only |
    #[test]
    fn resource_references_retain_both_sides_until_settlement() {
        let empty = SessionResourceState::default();
        assert!(!empty.has_references(), "G1");

        let mut commit = SessionResourceState::default();
        commit.prepare("session-1", manifest("a")).unwrap();
        commit.commit().unwrap();
        assert_eq!(
            commit.resource_references().inputs(),
            manifest("a").inputs(),
            "G2"
        );
        commit.prepare("session-1", manifest("b")).unwrap();
        let retained = commit.resource_references();
        assert_eq!(retained.inputs().len(), 2, "G3");
        assert!(commit.has_references(), "G3");
        commit.commit().unwrap();
        assert_eq!(
            commit.resource_references().inputs(),
            manifest("b").inputs(),
            "G4"
        );

        let mut rollback = SessionResourceState::default();
        rollback.prepare("session-2", manifest("a")).unwrap();
        rollback.commit().unwrap();
        rollback.prepare("session-2", manifest("b")).unwrap();
        rollback.rollback("injected").unwrap();
        assert_eq!(
            rollback.resource_references().inputs(),
            manifest("a").inputs(),
            "G5"
        );
    }

    #[test]
    fn release_is_durable_and_idempotently_completes() {
        let mut state = SessionResourceState::default();
        state.prepare("session-1", manifest("a")).unwrap();
        state.commit().unwrap();
        state.begin_release().unwrap();
        assert!(state.needs_reconciliation());
        state.complete_release();
        state.complete_release();
        assert_eq!(state.activations[0].state, ActivationState::Released);
        assert!(!state.needs_reconciliation());
    }

    /// Terminal-release cause/effect table. C1 an installed manifest owns a
    /// retention edge; C2 a replacement may also be Prepared; C3 every external
    /// teardown succeeded. Effects are E1 installed activation Released, E2
    /// uncommitted activation Failed, and E3 both desired/active manifests are
    /// cleared so the durable purge fence can observe zero Session references.
    ///
    /// | Rule | Active | Pending | Teardown | Effect |
    /// |---|---|---|---|---|
    /// | T1 | yes | no | success | E1 + E3 |
    /// | T2 | yes | yes | success | E1 + E2 + E3 |
    #[test]
    fn terminal_release_clears_the_reference_authority_after_teardown() {
        let mut state = SessionResourceState::default();
        state.prepare("session-1", manifest("a")).unwrap();
        state.commit().unwrap();
        state.prepare("session-1", manifest("b")).unwrap();
        state.complete_terminal_release("terminated");

        assert!(state.active.inputs().is_empty(), "T2/E3");
        assert!(state.pending.is_none(), "T2/E3");
        assert!(!state.has_references(), "T2/E3");
        assert_eq!(
            state.activations[0].state,
            ActivationState::Released,
            "T2/E1"
        );
        assert_eq!(state.activations[1].state, ActivationState::Failed, "T2/E2");
    }

    /// Settled-history cause/effect graph: C1 hundreds of successful manifest
    /// replacements create superseded Released generations; C2 a later rollback
    /// creates Failed diagnostics; C3 Active/Prepared/Releasing records still
    /// own effects. Effects: E1 all C3 records survive; E2 only the latest
    /// Released generation survives; E3 only the latest Failed generation
    /// survives; E4 revision, active manifest, and retention references are
    /// unchanged; E5 applying compaction again is an exact no-op.
    ///
    /// | Rule | Nonterminal | Released history | Failed history | Effect |
    /// |---|---|---|---|---|
    /// | H1 | active | 400 generations | none | E1 + E2 + E4 |
    /// | H2 | active | retained latest | new failure | E1 + E2 + E3 + E4 |
    /// | H3 | unchanged | compacted | compacted | E5 |
    #[test]
    fn settled_activation_history_is_bounded_without_weakening_authority() {
        let mut state = SessionResourceState::default();
        for revision in 0..400 {
            state
                .prepare("session-history", manifest(&format!("success-{revision}")))
                .unwrap();
            state.start_attempt().unwrap();
            state.commit().unwrap();
        }
        assert_eq!(state.revision, 400, "H1/E4");
        assert_eq!(state.activations.len(), 2, "H1/E1-E2");
        assert_eq!(
            state
                .activations
                .iter()
                .filter(|activation| activation.state == ActivationState::Released)
                .map(|activation| activation.revision)
                .collect::<Vec<_>>(),
            [399],
            "H1/E2"
        );
        assert_eq!(state.active, manifest("success-399"), "H1/E4");
        assert_eq!(
            state.resource_references().inputs(),
            manifest("success-399").inputs(),
            "H1/E4"
        );

        state
            .prepare("session-history", manifest("failed-400"))
            .unwrap();
        state.start_attempt().unwrap();
        state.rollback("injected failure").unwrap();
        assert_eq!(state.revision, 401, "H2/E4");
        assert_eq!(state.activations.len(), 3, "H2/E1-E3");
        assert!(
            state.activations.iter().any(|activation| {
                activation.state == ActivationState::Failed
                    && activation.revision == 401
                    && activation.last_error.as_deref() == Some("injected failure")
            }),
            "H2/E3"
        );
        let compacted = state.clone();
        assert!(!state.compact_settled_activations(), "H3/E5");
        assert_eq!(state, compacted, "H3/E5");
    }
}
