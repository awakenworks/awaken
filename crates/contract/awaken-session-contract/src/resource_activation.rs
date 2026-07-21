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
    pub activations: Vec<SessionResourceActivation>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResourceActivationError {
    #[error("a Session resource activation is already pending")]
    Pending,
    #[error("no Session resource activation is pending")]
    NoPending,
    #[error("Session resource activation revision is exhausted")]
    RevisionExhausted,
    #[error("Session resource activation state is inconsistent: {0}")]
    Invalid(String),
}

impl SessionResourceState {
    /// Convert a pre-activation persisted manifest into the new durable state.
    /// The coordinator adopts activation records on first recovery.
    #[must_use]
    pub fn from_legacy(active: ResolvedSessionResources) -> Self {
        Self {
            revision: u64::from(!active.inputs.is_empty()),
            active,
            pending: None,
            activations: Vec::new(),
        }
    }

    /// Persist a new desired manifest before invoking any external realizer.
    pub fn prepare(
        &mut self,
        session_id: &str,
        desired: ResolvedSessionResources,
    ) -> Result<u64, ResourceActivationError> {
        if self.pending.is_some()
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
                .inputs
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

    /// Commit a successful whole-manifest realization.
    pub fn commit(&mut self) -> Result<(), ResourceActivationError> {
        let desired = self
            .pending
            .take()
            .ok_or(ResourceActivationError::NoPending)?;
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
        Ok(())
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
        Ok(())
    }

    /// Adopt a legacy active manifest after it has been re-realized once.
    pub fn adopt_legacy_active(&mut self, session_id: &str) {
        if !self.activations.is_empty() || self.active.inputs.is_empty() {
            return;
        }
        self.revision = self.revision.max(1);
        self.activations
            .extend(self.active.inputs.iter().map(|input| {
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
    }

    /// A terminal Session was torn down while a replacement was pending. The
    /// previous generation is released and the never-committed generation is
    /// failed; no manifest becomes newly active after termination.
    pub fn complete_terminal_release(&mut self, reason: impl Into<String>) {
        self.pending = None;
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
    }

    #[must_use]
    pub fn has_active(&self) -> bool {
        self.activations
            .iter()
            .any(|activation| activation.state == ActivationState::Active)
    }

    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        self.pending.is_some()
            || self.activations.iter().any(|activation| {
                matches!(
                    activation.state,
                    ActivationState::Prepared | ActivationState::Releasing
                )
            })
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
    use awaken_resource_contract::{BindingId, FileId};

    use super::*;

    fn manifest(id: &str) -> ResolvedSessionResources {
        ResolvedSessionResources {
            inputs: vec![ResolvedInput {
                binding_id: BindingId::from(format!("binding-{id}")),
                source: ResolvedInputSource::File {
                    file_id: FileId::from(format!("file-{id}")),
                },
                mount_path: format!("/inputs/{id}"),
                access: ResourceAccess::ReadOnly,
                instructions: None,
            }],
            skills: Some(Vec::new()),
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
        assert_eq!(state.active, manifest("a"));
        assert_eq!(state.activations[0].state, ActivationState::Releasing);
        state.commit().unwrap();
        assert_eq!(state.active, manifest("b"));
        assert_eq!(state.activations[0].state, ActivationState::Released);
        assert_eq!(state.activations[1].state, ActivationState::Active);
    }

    #[test]
    fn rollback_restores_old_authority_and_fails_only_new_generation() {
        let mut state = SessionResourceState::default();
        state.prepare("session-1", manifest("a")).unwrap();
        state.commit().unwrap();
        state.prepare("session-1", manifest("b")).unwrap();
        state.start_attempt().unwrap();
        state.rollback("clone failed").unwrap();
        assert_eq!(state.active, manifest("a"));
        assert!(state.pending.is_none());
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
}
