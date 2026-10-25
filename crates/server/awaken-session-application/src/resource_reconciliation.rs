// @formal entrypoint session.cleanup_completion_requires_exact_identity_and_canonical_artifact_evidence
// @formal entrypoint session.cleanup_operation_advances_exactly_and_completed_is_absorbing
// @formal entrypoint session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
// @formal entrypoint session_resources.external_io_requires_durable_prepared_state
// @formal entrypoint session_resources.failed_replacement_preserves_active_manifest
// @formal entrypoint session_resources.repository_rotation_requires_original_live_generation
// @formal entrypoint session_resources.restart_retries_same_pending_revision
// @formal entrypoint session_resources.terminal_session_never_reactivates
//! Durable Session Resource recovery and terminal reclamation.

use crate::credentials::preflight_repository_credential_rotation;
use crate::mutation::ROOT_CAS_ATTEMPTS;
use crate::repository_participants::RepositoryRetirementCause;
use awaken_credential_contract::{
    CredentialMaterialInput, CredentialMaterialRetirementCommand, CredentialMaterialRotationCommand,
};
use awaken_session_configuration_contract::resource::{
    ResolvedInput, ResolvedInputSource, ResolvedSessionResources,
};
use awaken_session_state_contract::{
    ActivationState, PersistedSession, SessionAggregate, SessionExecutionState,
};
use awaken_session_types::RunError;
use awaken_time::WallClockExt as _;

use std::sync::Arc;

use super::{
    SessionMutationError, SessionPreparationError, SessionReconciliation,
    SessionReconciliationFailure, SessionRecoveryCandidates,
    credentials::{internal, mutation_failure, repository_preparation},
};

mod ordinary_reconciliation_gate;
mod purge_guard;

use ordinary_reconciliation_gate::ordinary_resource_reconciliation_allowed;
pub use purge_guard::SessionResourcePurgeGuard;
pub(crate) use purge_guard::resource_targets;

#[derive(Clone)]
enum ResourceSettlement {
    Commit,
    Rollback(String),
    RetryableFailure(String),
}

/// Root-CAS retries may advance only this command's credential pin/observation,
/// never rebind a completed Vault effect to another Resource identity/config.
fn repository_rotation_target_matches(before: &ResolvedInput, after: &ResolvedInput) -> bool {
    let mut candidate = after.clone();
    let (
        ResolvedInputSource::Repository {
            credential: original,
            ..
        },
        ResolvedInputSource::Repository {
            credential: current,
            ..
        },
    ) = (&before.source, &mut candidate.source)
    else {
        return false;
    };
    *current = original.clone();
    candidate.updated_at_unix_ms = before.updated_at_unix_ms;
    candidate == *before
}

const fn repository_rotation_generation_is_current(
    captured: u64,
    current: u64,
    execution: SessionExecutionState,
    disposition: &awaken_session_state_contract::SessionDisposition,
) -> bool {
    captured == current
        && !awaken_session_state_contract::session_is_terminal(execution, disposition)
}

#[cfg(kani)]
/// @obligation session_resources.repository_rotation_requires_original_live_generation
#[kani::proof]
fn repository_rotation_requires_original_live_resource_generation() {
    // Full u64 generations and every actual durable execution-state variant.
    // Root-only CAS changes can rebase; a different desired generation or
    // terminal root refuses. Codec/source honesty, pin equality, Vault/SQL
    // atomicity and async races are outside this scalar proof.
    let captured: u64 = kani::any();
    let current: u64 = kani::any();
    let execution = match kani::any::<u8>() {
        0 => SessionExecutionState::Preparing,
        1 => SessionExecutionState::Activating,
        2 => SessionExecutionState::ActivationFailed,
        3 => SessionExecutionState::Running,
        4 => SessionExecutionState::Rescheduling,
        5 => SessionExecutionState::Idle,
        _ => SessionExecutionState::Terminated,
    };
    let disposition = match kani::any::<u8>() {
        0 => awaken_session_state_contract::SessionDisposition::Active,
        1 => awaken_session_state_contract::SessionDisposition::Archived {
            archived_at: String::new(),
            archived_at_unix_ms: None,
        },
        2 => awaken_session_state_contract::SessionDisposition::Deleting,
        _ => awaken_session_state_contract::SessionDisposition::Deleted,
    };
    let terminal = matches!(
        execution,
        SessionExecutionState::ActivationFailed | SessionExecutionState::Terminated
    ) || !matches!(
        disposition,
        awaken_session_state_contract::SessionDisposition::Active
    );
    let admitted =
        repository_rotation_generation_is_current(captured, current, execution, &disposition);
    assert_eq!(admitted, captured == current && !terminal);
    if captured != current || terminal {
        assert!(!admitted);
    }
    if captured == current && !terminal {
        assert!(admitted);
    }
}

/// Durable Session Resource reconciliation: drive a committed desired Resource
/// generation to active (or roll it back), retire detached Repository
/// participants, and hand a terminal Session's Resources to terminal cleanup.
/// It holds the CAS kernel, the clock, the credential pins, the Resource
/// commit bracket, the Repository participants, terminal cleanup and the
/// realization-effects role that prepares Resources in the Environment.
pub struct SessionResourceReconciliation {
    mutations: Arc<crate::SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
    credentials: Arc<crate::SessionCredentials>,
    resource_references: Arc<crate::SessionResourceReferences>,
    repository_participants: Arc<crate::SessionRepositoryParticipants>,
    terminal_cleanups: Arc<crate::SessionTerminalCleanups>,
    placement: awaken_session_execution_contract::SessionPlacementRoles,
}

impl SessionResourceReconciliation {
    pub(crate) fn new(
        mutations: Arc<crate::SessionMutations>,
        clock: Arc<dyn awaken_time::WallClock>,
        credentials: Arc<crate::SessionCredentials>,
        resource_references: Arc<crate::SessionResourceReferences>,
        repository_participants: Arc<crate::SessionRepositoryParticipants>,
        terminal_cleanups: Arc<crate::SessionTerminalCleanups>,
        runtime: &crate::SessionExecutor,
    ) -> Self {
        Self {
            mutations,
            clock,
            credentials,
            resource_references,
            repository_participants,
            terminal_cleanups,
            placement: runtime.placement.clone(),
        }
    }

    /// Settle the one canonical reconciliation attempt against its exact
    /// Resource generation. Root-only concurrent mutations are rebased; a
    /// competing Resource generation is never merged.
    async fn settle_resource_reconciliation(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        resource_revision: u64,
        desired: &ResolvedSessionResources,
        settlement: ResourceSettlement,
    ) -> Result<SessionAggregate, SessionPreparationError> {
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let mut current = self
                .mutations
                .session_aggregate(session_id)
                .await
                .map_err(repository_preparation)?;
            if current.resources.revision != resource_revision
                || current.resources.pending.as_ref() != Some(desired)
            {
                return Err(SessionPreparationError::Conflict);
            }
            let operation = match &settlement {
                ResourceSettlement::Commit => {
                    current.resources().commit().map_err(internal)?;
                    "resource-reconcile-active"
                }
                ResourceSettlement::Rollback(error) => {
                    current
                        .resources()
                        .rollback(error.clone())
                        .map_err(internal)?;
                    "resource-reconcile-rollback"
                }
                ResourceSettlement::RetryableFailure(error) => {
                    current
                        .resources()
                        .note_retryable_failure(error.clone())
                        .map_err(internal)?;
                    "resource-reconcile-failed"
                }
            };
            match self
                .resource_references
                .commit(owner_scope, current, operation, Vec::new())
                .await
            {
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                result => return result.map_err(mutation_failure),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    async fn start_resource_reconciliation(
        &self,
        owner_scope: &str,
        mut session: SessionAggregate,
        desired: &ResolvedSessionResources,
    ) -> Result<SessionAggregate, SessionPreparationError> {
        let resource_revision = session.resources.revision;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            if session.resources.revision != resource_revision
                || session.resources.pending.as_ref() != Some(desired)
            {
                return Err(SessionPreparationError::Conflict);
            }
            let mut candidate = session.clone();
            candidate.resources().start_attempt().map_err(internal)?;
            match self
                .resource_references
                .commit(
                    owner_scope,
                    candidate,
                    "resource-reconcile-attempt",
                    Vec::new(),
                )
                .await
            {
                Ok(committed) => return Ok(committed),
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    session = self
                        .mutations
                        .session_aggregate(&session.session_id)
                        .await
                        .map_err(repository_preparation)?;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    /// Reconcile every local durable Resource projection requiring convergence.
    pub async fn reconcile_resource_activations(&self) -> SessionReconciliation {
        let candidates = match super::recovery_scan::recovery_candidates(
            self.mutations.repository().as_ref(),
            self.clock.as_ref(),
            true,
        )
        .await
        {
            Ok(candidates) => candidates,
            Err(error) => {
                let mut report = SessionReconciliation::default();
                report.failures.push(SessionReconciliationFailure {
                    session_id: "<repository>".to_string(),
                    message: error.to_string(),
                });
                return report;
            }
        };
        self.reconcile_resource_activations_from(&candidates).await
    }

    pub(crate) async fn reconcile_resource_activations_from(
        &self,
        candidates: &SessionRecoveryCandidates,
    ) -> SessionReconciliation {
        let mut report = SessionReconciliation {
            pending: candidates.sessions.len(),
            quarantined: candidates.quarantined.clone(),
            ..Default::default()
        };
        // Different Sessions already reconcile concurrently across replicas.
        // Reuse the same bounded, owned sweep as Event recovery while retaining
        // every reference/Resource authority check and each root's effect order.
        let resource_candidates = SessionRecoveryCandidates {
            sessions: super::recovery_sweep::select_resource_candidates(candidates),
            quarantined: candidates.quarantined.clone(),
        };
        let completed = super::recovery_sweep::map_candidates(
            &resource_candidates,
            super::recovery_sweep::RESOURCE_RECOVERY_CONCURRENCY,
            |candidate| async move {
                let mut result = SessionReconciliation::default();
                let owner_scope = &candidate.workspace_id;
                let session = match self
                    .mutations
                    .session_aggregate(&candidate.session_id)
                    .await
                {
                    Ok(session) => session,
                    Err(awaken_session_state_contract::SessionRepositoryError::NotFound) => {
                        return result;
                    }
                    Err(error) => {
                        result.failures.push(SessionReconciliationFailure {
                            session_id: candidate.session_id.to_string(),
                            message: error.to_string(),
                        });
                        return result;
                    }
                };
                let session_id = session.session_id.clone();
                if let Err(error) = self
                    .resource_references
                    .synchronize(owner_scope, &session)
                    .await
                {
                    result.failures.push(SessionReconciliationFailure {
                        session_id: session_id.into_string(),
                        message: error.to_string(),
                    });
                    return result;
                }
                if (!session.is_terminal()
                    && session.requires_external_environment_owner()
                    && !session.resources.has_repository_retirements())
                    || !session.needs_resource_reconciliation()
                {
                    return result;
                }
                match self
                    .reconcile_persisted_resources(owner_scope, session.into_snapshot())
                    .await
                {
                    Ok(session) => result.settled.push(session),
                    Err(SessionPreparationError::NotFound) => {
                        // An eager terminal-cleanup actor may commit the delete
                        // tombstone after this scan captured its candidate. The
                        // missing aggregate is then the authoritative successful
                        // outcome, not a retryable reconciliation failure. Repair
                        // the disposable reference projection from that truth.
                        self.resource_references
                            .repair(owner_scope, &session_id)
                            .await;
                    }
                    Err(error) => result.failures.push(SessionReconciliationFailure {
                        session_id: session_id.into_string(),
                        message: error.to_string(),
                    }),
                }
                result
            },
        )
        .await;
        for result in completed {
            report.settled.extend(result.settled);
            report.failures.extend(result.failures);
        }
        report
    }

    /// Reconcile one persisted Session's Resource projection and reclamation.
    pub async fn reconcile_persisted_resources(
        &self,
        owner_scope: &str,
        session: PersistedSession,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let session = self
            .credentials
            .ensure_repository_credentials_pinned(owner_scope, session)
            .await?;
        if !session.is_terminal() && !session.requires_external_environment_owner() {
            if session.execution != SessionExecutionState::Idle {
                // Resource changes remain deferred while a Run owns execution.
                // Select that existing rule before constructing the complete
                // activation/retirement future on the supervisor poll stack.
                return Ok(session);
            }
            if !session.resources.needs_reconciliation()
                && session.resources.active.inputs().is_empty()
                && session.resources.activations.is_empty()
            {
                // This is the exact no-op branch below, selected before constructing
                // the full activation/retirement future. `needs_reconciliation` is
                // the canonical pending/activation/retirement-work predicate.
                return Ok(session);
            }
        }
        self.reconcile_prepared_resources(
            owner_scope,
            crate::mutation::aggregate_from_snapshot(session).map_err(repository_preparation)?,
        )
        .await
        .map(SessionAggregate::into_snapshot)
    }

    /// Run the Repository retirements a detached Resource generation left
    /// behind; a root without retirements is returned unchanged.
    async fn retire_detached_generation(
        &self,
        owner_scope: &str,
        session: SessionAggregate,
    ) -> Result<SessionAggregate, SessionPreparationError> {
        if !session.resources.has_repository_retirements() {
            return Ok(session);
        }
        self.repository_participants
            .reconcile_retirements(
                owner_scope,
                session,
                RepositoryRetirementCause::DetachedGeneration,
            )
            .await
    }

    /// Apply the Resource state machine only after credential migration has
    /// reached a stable Session root. Keeping these phases as separate futures
    /// also bounds synchronous poll-stack growth on the HTTP admission path.
    async fn reconcile_prepared_resources(
        &self,
        owner_scope: &str,
        mut session: SessionAggregate,
    ) -> Result<SessionAggregate, SessionPreparationError> {
        if !session.is_terminal() && session.requires_external_environment_owner() {
            return if session.resources.pending.is_none() {
                self.retire_detached_generation(owner_scope, session).await
            } else {
                Ok(session)
            };
        }
        let effects_runtime = self
            .placement
            .realization_effects()
            .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))?;
        let session_id = session.session_id.clone();
        if ordinary_resource_reconciliation_allowed(
            session.is_terminal(),
            session.execution == SessionExecutionState::Idle,
        ) {
            if let Some(desired) = session.resources.pending.clone() {
                let previous = session.resources.active.clone();
                let previous_revision = session.resources.active_revision();
                let transition = crate::session_resource::SessionResourceTransition::new(
                    crate::session_resource::SessionResourceManifest::at_revision(
                        owner_scope,
                        previous_revision,
                        previous.clone(),
                    ),
                    crate::session_resource::SessionResourceManifest::at_revision(
                        owner_scope,
                        session.resources.revision,
                        desired.clone(),
                    ),
                )
                .map_err(internal)?;
                session = self
                    .start_resource_reconciliation(owner_scope, session, &desired)
                    .await?;
                if let Err(error) = effects_runtime
                    .apply_session_inputs(session_id.as_str(), &transition)
                    .await
                {
                    let settlement = match effects_runtime
                        .apply_session_inputs(session_id.as_str(), &transition.reversed())
                        .await
                    {
                        Ok(()) => ResourceSettlement::Rollback(error.to_string()),
                        Err(rollback_error) => ResourceSettlement::RetryableFailure(format!(
                            "activation failed: {error}; rollback failed: {rollback_error}"
                        )),
                    };
                    self.settle_resource_reconciliation(
                        owner_scope,
                        &session_id,
                        session.resources.revision,
                        &desired,
                        settlement,
                    )
                    .await?;
                    return Err(SessionPreparationError::Rejected(error));
                }
                let committed = self
                    .settle_resource_reconciliation(
                        owner_scope,
                        &session_id,
                        session.resources.revision,
                        &desired,
                        ResourceSettlement::Commit,
                    )
                    .await?;
                return self
                    .retire_detached_generation(owner_scope, committed)
                    .await;
            }

            if session
                .resources
                .activations
                .iter()
                .any(|activation| activation.state == ActivationState::Releasing)
            {
                return Err(internal(
                    "resource activation has Releasing records without a pending manifest",
                ));
            }
            if session.resources.active.inputs().is_empty()
                && session.resources.activations.is_empty()
            {
                return self.retire_detached_generation(owner_scope, session).await;
            }
            let (active_revision, active_resources) = session.resources.active_generation();
            let active = crate::session_resource::SessionResourceManifest::at_revision(
                owner_scope,
                active_revision,
                active_resources.clone(),
            );
            let transition =
                crate::session_resource::SessionResourceTransition::new(active.clone(), active)
                    .map_err(internal)?;
            effects_runtime
                .apply_session_inputs(session_id.as_str(), &transition)
                .await
                .map_err(SessionPreparationError::Rejected)?;
            if session.resources.activations.is_empty() {
                session.resources().adopt_legacy_active();
                session = self
                    .resource_references
                    .commit(owner_scope, session, "resource-adopt-legacy", Vec::new())
                    .await
                    .map_err(mutation_failure)?;
            }
            return self.retire_detached_generation(owner_scope, session).await;
        }

        self.reconcile_terminal_resource_candidate(owner_scope, session)
            .await
    }

    async fn reconcile_terminal_resource_candidate(
        &self,
        owner_scope: &str,
        session: SessionAggregate,
    ) -> Result<SessionAggregate, SessionPreparationError> {
        match self
            .terminal_cleanups
            .release_terminal_resources(owner_scope, session.session_id.as_str())
            .await?
        {
            Some(committed) => {
                crate::mutation::aggregate_from_snapshot(committed).map_err(repository_preparation)
            }
            None => Ok(session),
        }
    }
}

impl crate::SessionCredentials {
    /// Rotate and repin one Session-scoped Repository credential through the
    /// durable root CAS. A conflict reloads and recompiles the exact pin.
    pub async fn rotate_repository_credential(
        &self,
        session_id: &awaken_session_types::SessionId,
        owner_scope: &str,
        binding_id: &awaken_resource_contract::BindingId,
        material: CredentialMaterialInput,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let mutations = self.mutations();
        let mut persisted = mutations
            .session_aggregate(session_id)
            .await
            .map_err(repository_preparation)?;
        if persisted.is_terminal() {
            return Err(SessionPreparationError::Conflict);
        }
        if let Some(baseline) = persisted.frozen_baseline()
            && !baseline
                .mutation_policy
                .admits_repository_credential_mutation()
        {
            return Err(SessionPreparationError::Rejected(RunError::bad_request(
                "profiled Session Repository credentials are immutable",
            )));
        }
        let ingress = self.credential_material_ingress().ok_or_else(|| {
            SessionPreparationError::Rejected(RunError::bad_request(
                "repository authorization requires a configured credential Vault",
            ))
        })?;
        let resource_generation = persisted.resources.desired_generation().0;
        let original_input = persisted
            .resources
            .desired()
            .inputs()
            .iter()
            .find(|input| input.binding_id == *binding_id)
            .cloned()
            .ok_or(SessionPreparationError::NotFound)?;
        let (credential_source, credential_revision, remote_url) = match &original_input.source {
            ResolvedInputSource::Repository {
                config, credential, ..
            } => config
                .credential_binding
                .clone()
                .zip(credential.as_ref())
                .map(|(binding, credential)| {
                    (
                        binding,
                        credential.access.credential.revision,
                        config.remote_url.clone(),
                    )
                }),
            _ => None,
        }
        .ok_or_else(|| {
            SessionPreparationError::Rejected(RunError::bad_request(
                "repository credential update requires an authenticated Repository resource",
            ))
        })?;
        // Run the existing exact pending gate BEFORE the Vault effect. The
        // clone is a preflight only; the durable root remains the sole owner.
        preflight_repository_credential_rotation(&persisted, original_input.clone())?;
        self.resource_plaintext_holder(&persisted)?;
        let credential_target =
            crate::session_resource::repository_transport_credential_target(&remote_url).map_err(
                |error| SessionPreparationError::Rejected(RunError::bad_request(error.to_string())),
            )?;
        let completed_credential_revision = ingress
            .rotate_material(CredentialMaterialRotationCommand {
                source_id: awaken_credential_contract::CredentialSourceId(credential_source),
                expected_revision: credential_revision,
                workspace_id: owner_scope.to_owned(),
                target: credential_target,
                usage: crate::session_resource::repository_transport_credential_usage(),
                material,
            })
            .await
            .map_err(|error| {
                SessionPreparationError::Rejected(RunError::bad_request(format!(
                    "repository authorization could not be rotated: {error}"
                )))
            })?;

        for attempt in 0..ROOT_CAS_ATTEMPTS {
            if !repository_rotation_generation_is_current(
                resource_generation,
                persisted.resources.desired_generation().0,
                persisted.execution,
                &persisted.disposition,
            ) {
                return Err(SessionPreparationError::Conflict);
            }
            let holder = self.resource_plaintext_holder(&persisted)?;
            let mut input = persisted
                .resources
                .desired()
                .inputs()
                .iter()
                .find(|input| input.binding_id == *binding_id)
                .cloned()
                .ok_or(SessionPreparationError::Conflict)?;
            if !repository_rotation_target_matches(&original_input, &input) {
                return Err(SessionPreparationError::Conflict);
            }
            let ResolvedInputSource::Repository { credential, .. } = &mut input.source else {
                return Err(SessionPreparationError::NotFound);
            };
            *credential = None;
            self.pin_repository_credential(owner_scope, &holder, &mut input)
                .await?;
            let ResolvedInputSource::Repository {
                credential: Some(credential),
                ..
            } = &input.source
            else {
                return Err(SessionPreparationError::Rejected(RunError::bad_request(
                    "repository credential rotation did not produce an exact execution pin",
                )));
            };
            if credential.access.credential.revision != completed_credential_revision {
                return Err(SessionPreparationError::Rejected(RunError::bad_request(
                    "repository credential revision changed before Session repin",
                )));
            }
            input.updated_at_unix_ms = Some(
                self.clock()
                    .now_ms()
                    .max(input.updated_at_unix_ms.unwrap_or(0)),
            );
            persisted
                .resources()
                .replace_desired_repository_credential(input)
                .map_err(|_| SessionPreparationError::Conflict)?;
            match mutations
                .commit_session_snapshot_with_outcome(
                    owner_scope,
                    persisted,
                    "repository-credential-update",
                    Vec::new(),
                )
                .await
            {
                Ok(commit) => return Ok(commit.into_session()),
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    persisted = mutations
                        .session_aggregate(session_id)
                        .await
                        .map_err(repository_preparation)?;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    pub(crate) async fn retire_owned_repository_credential(
        &self,
        owner_scope: &str,
        credential: &awaken_credential_contract::CredentialRef,
    ) -> bool {
        let Some(ingress) = self.credential_material_ingress() else {
            return false;
        };
        ingress
            .retire_material(CredentialMaterialRetirementCommand {
                credential: credential.clone(),
                workspace_id: owner_scope.to_owned(),
            })
            .await
            .is_ok()
    }
}
