//! Durable Session Resource recovery and terminal reclamation.

use awaken_resource_contract::{ResourceReference, ResourceReferenceKind, ResourceReferenceRecord};
use awaken_session_contract::{
    ActivationState, IdempotencyRecord, ManagedLifecycleFact, PersistedSession,
    ResolvedInputSource, ResolvedSessionResources, RunError, SessionExecutionState,
    SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionRealizationControlFailure, SessionRealizationLease, SessionRevision, SessionTombstone,
    stable_fingerprint,
};

use super::{
    ConfiguredSessionRepository, CredentialMaterialInput, CredentialMaterialRetirementCommand,
    CredentialMaterialRotationCommand, SessionApplication, SessionMutationError,
    SessionParticipantProvenance, SessionPreparationError, SessionReconciliation,
    SessionReconciliationFailure, SessionRecoveryCandidates, SessionRepositoryOwner,
    mutation::repository_failure,
    realization::{mutation_control, repository_control},
};

mod purge_guard;
mod repository_reference;
mod terminal_cleanup;

pub use purge_guard::SessionResourcePurgeGuard;
use purge_guard::resource_targets;
use repository_reference::{
    session_resources_reference_repository, session_resources_reference_repository_generation,
};

fn terminal_restore_target_for_thread(
    workspace_id: &str,
    session: &PersistedSession,
    thread_id: &str,
) -> Option<awaken_session_contract::SandboxRestoreRequest> {
    (thread_id == session.session_id)
        .then(|| {
            session
                .environment
                .restoring_request(workspace_id, &session.session_id)
        })
        .flatten()
}

fn bind_terminal_restore_target(
    workspace_id: &str,
    session: &PersistedSession,
    action: awaken_session_contract::SessionTerminalCleanupAction,
) -> Result<awaken_session_contract::SessionTerminalCleanupAction, SessionRealizationControlFailure>
{
    match action {
        awaken_session_contract::SessionTerminalCleanupAction::Prepare { mut commands } => {
            if let Some(root) = commands
                .iter_mut()
                .find(|command| command.thread_id == session.session_id)
                && let Some(request) =
                    terminal_restore_target_for_thread(workspace_id, session, &root.thread_id)
            {
                *root = root.clone().with_restore_target(request).map_err(|error| {
                    SessionRealizationControlFailure::Invalid(error.to_string())
                })?;
            }
            Ok(awaken_session_contract::SessionTerminalCleanupAction::Prepare { commands })
        }
        awaken_session_contract::SessionTerminalCleanupAction::Dispose { command } => {
            let command = match terminal_restore_target_for_thread(
                workspace_id,
                session,
                &session.session_id,
            ) {
                Some(request) => command.with_restore_target(request).map_err(|error| {
                    SessionRealizationControlFailure::Invalid(error.to_string())
                })?,
                None => command,
            };
            Ok(awaken_session_contract::SessionTerminalCleanupAction::Dispose { command })
        }
        awaken_session_contract::SessionTerminalCleanupAction::Waiting => {
            Ok(awaken_session_contract::SessionTerminalCleanupAction::Waiting)
        }
    }
}

#[derive(Clone)]
enum ResourceSettlement {
    Commit,
    Rollback(String),
    RetryableFailure(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnedRepositoryRetirement {
    Retired,
    NotOwned,
    Pending,
}

impl OwnedRepositoryRetirement {
    const fn is_complete(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// One cause projection for the existing Repository retirement reconciler.
/// Ordinary manifest replacement cancels a retirement if the generation is
/// still referenced. Terminal preparation deliberately retains that frozen
/// Resource projection until physical disposal, so its already-persisted plan
/// must instead run to completion through the same participant authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepositoryRetirementCause {
    DetachedGeneration,
    TerminalPreparation,
}

/// Protocol-neutral whole-manifest command. The interface owns request
/// canonicalization; the Session application owns desired-state persistence,
/// idempotency, and realization ordering.
#[derive(Clone, Debug)]
pub struct ReplaceSessionResourceManifest {
    pub resources: ResolvedSessionResources,
    /// Optimistic fence for the Resource sub-aggregate, not the Session root.
    /// Runtime realization lease renewals advance the root revision without
    /// changing this generation and therefore must not starve Resource writes.
    pub expected_resource_revision: Option<u64>,
    pub idempotency_key: Option<String>,
    pub request_fingerprint: String,
}

/// Durable command receipt plus the latest aggregate projection. The command
/// revision is stable across replay even if realization advances the root CAS.
#[derive(Clone, Debug)]
pub struct SessionResourceManifestOutcome {
    pub session: PersistedSession,
    pub command_revision: SessionRevision,
    pub command_applied: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionResourceManifestError {
    #[error("Session was not found")]
    NotFound,
    #[error("Session is terminal")]
    Terminal,
    #[error("Session revision conflict")]
    Conflict,
    #[error("Session idempotency key was reused with another manifest")]
    IdempotencyMismatch,
    #[error("Session resource manifest was rejected: {0}")]
    Rejected(#[source] RunError),
    /// Desired truth already committed; a retry/reconciler may repair the
    /// disposable Runtime projection without accepting another generation.
    #[error("Session resource realization failed after commit: {source}")]
    ProjectionAfterCommit {
        outcome: Box<SessionResourceManifestOutcome>,
        #[source]
        source: SessionPreparationError,
    },
    #[error("Session resource persistence is unavailable: {0}")]
    Unavailable(String),
}

impl SessionResourceManifestError {
    fn mutation(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => Self::NotFound,
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::IdempotencyMismatch => Self::IdempotencyMismatch,
            SessionMutationError::Unavailable(message) => Self::Unavailable(message),
        }
    }
}

pub(crate) fn mutation_failure(error: SessionMutationError) -> SessionPreparationError {
    match error {
        SessionMutationError::NotFound => SessionPreparationError::NotFound,
        SessionMutationError::Conflict => SessionPreparationError::Conflict,
        SessionMutationError::IdempotencyMismatch => SessionPreparationError::Unavailable(
            "Session idempotency key was reused with another payload".into(),
        ),
        SessionMutationError::Unavailable(message) => SessionPreparationError::Unavailable(message),
    }
}

fn repository_preparation(
    error: awaken_session_contract::SessionRepositoryError,
) -> SessionPreparationError {
    mutation_failure(repository_failure(error))
}

pub(crate) fn internal(error: impl std::fmt::Display) -> SessionPreparationError {
    SessionPreparationError::Rejected(RunError::internal(error.to_string()))
}

fn deletion_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
        .to_string()
}

impl SessionApplication {
    fn terminal_cleanup_lease_matches(
        session: &PersistedSession,
        asserted: &SessionRealizationLease,
    ) -> bool {
        session.realization.as_ref().is_some_and(|current| {
            awaken_session_contract::realization_lease_generation_authorizes(current, asserted)
        })
    }

    /// Read one terminal-work projection from one Session-root snapshot.
    ///
    /// Cause/effect table: C1 the asserted generation matches the root's current
    /// lease; C2 the root has no terminal fence, is fenced/requested, or is
    /// completed; C3 the frozen Environment/Resource projection succeeds or
    /// fails. Effects: R1 C1+no fence/completed/legacy-complete => `None`; R2
    /// C1+fenced or publication barrier => `Waiting`; R3 C1+requested+C3
    /// succeeds => one `Prepare` or `Dispose` action from this same snapshot;
    /// R4 stale ownership or projection failure => typed failure before a
    /// Worker effect. The work value owns no queue or receipt.
    pub(crate) async fn terminal_cleanup_work_for_lease(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupWork>,
        SessionRealizationControlFailure,
    > {
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_control)?;
        if !Self::terminal_cleanup_lease_matches(&session, lease) {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        session
            .verified_terminal_cleanup()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        let action = session
            .terminal_cleanup_work_action()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        if action.is_none() {
            return Ok(None);
        }
        // Refresh only after the cheap root preflight identifies terminal work,
        // then re-read the root. The second snapshot, not the preflight value,
        // supplies every assignment and command fact below.
        self.refresh_executable_projections()
            .await
            .map_err(SessionRealizationControlFailure::Unavailable)?;
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_control)?;
        if !Self::terminal_cleanup_lease_matches(&session, lease) {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        session
            .verified_terminal_cleanup()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        let Some(action) = session
            .terminal_cleanup_work_action()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?
        else {
            return Ok(None);
        };
        let owner_scope = self
            .session_repository()
            .owner(session_id)
            .await
            .map_err(repository_control)?;
        let action = bind_terminal_restore_target(&owner_scope, &session, action)?;
        let assignment = self
            .terminal_cleanup_assignment_from_snapshot(&owner_scope, &session)
            .await
            .map_err(|error| SessionRealizationControlFailure::Unavailable(error.to_string()))?;
        Ok(Some(awaken_session_contract::SessionTerminalCleanupWork {
            assignment,
            action,
        }))
    }

    /// A terminal generation may carry an assertion whose original timestamp
    /// elapsed, but starting any new Resource/provider effect still requires
    /// the one current Session-root lease to be live. The aggregate's domain
    /// authorization immediately after this gate proves same-generation
    /// identity; this helper owns only the shared current-clock decision.
    fn require_current_live_terminal_generation(
        session: &PersistedSession,
    ) -> Result<(), SessionRealizationControlFailure> {
        if session.realization.as_ref().is_some_and(|current| {
            awaken_session_contract::realization_lease_is_live_at(
                current.expires_at_unix_ms,
                crate::activity::now_unix_ms(),
            )
        }) {
            Ok(())
        } else {
            Err(SessionRealizationControlFailure::StaleOwnership)
        }
    }

    pub(crate) async fn authorize_terminal_cleanup_effect_from_root(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
        SessionRealizationControlFailure,
    > {
        let session = self
            .session_repository()
            .get(&effect.command.session_id)
            .await
            .map_err(repository_control)?;
        // Terminal generation lifetime lets an already-admitted effect finish
        // after its asserted expiry. Starting a new physical effect is narrower:
        // the one current Session-root lease must still be live, while the
        // domain check below proves the old assertion is that same generation.
        // Worker Registry authentication remains the independent transport
        // identity edge; no second lease record or timer authority is created.
        Self::require_current_live_terminal_generation(&session)?;
        let workspace_id = self
            .session_repository()
            .owner(&effect.command.session_id)
            .await
            .map_err(repository_control)?;
        if effect.command.restore_target
            != terminal_restore_target_for_thread(
                &workspace_id,
                &session,
                &effect.command.thread_id,
            )
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal cleanup restore target differs from durable Environment authority".into(),
            ));
        }
        let inherited_provider_disposal = session
            .authorize_terminal_cleanup_effect(effect)
            .map_err(|error| match error {
                awaken_session_contract::SessionCleanupError::RealizationMismatch => {
                    SessionRealizationControlFailure::StaleOwnership
                }
                error => SessionRealizationControlFailure::Invalid(error.to_string()),
            })?;
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            effect.clone(),
            workspace_id,
            inherited_provider_disposal,
        )
    }

    pub(crate) async fn authorize_terminal_cleanup_disposal_from_root(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, SessionRealizationControlFailure> {
        let session = self
            .session_repository()
            .get(&effect.command.session_id)
            .await
            .map_err(repository_control)?;
        Self::require_current_live_terminal_generation(&session)?;
        let owner_scope = self
            .session_repository()
            .owner(&effect.command.session_id)
            .await
            .map_err(repository_control)?;
        if effect.command.restore_target
            != terminal_restore_target_for_thread(
                &owner_scope,
                &session,
                &effect.command.session_id,
            )
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal disposal restore target differs from durable Environment authority"
                    .into(),
            ));
        }
        session
            .authorize_terminal_cleanup_disposal_effect(&owner_scope, effect)
            .map_err(|error| match error {
                awaken_session_contract::SessionCleanupError::RealizationMismatch => {
                    SessionRealizationControlFailure::StaleOwnership
                }
                error => SessionRealizationControlFailure::Invalid(error.to_string()),
            })?;
        Ok(owner_scope)
    }

    pub(crate) async fn authorize_terminal_memory_intent_from_root(
        &self,
        intent: &awaken_session_contract::SessionTerminalMemoryIntent,
    ) -> Result<
        awaken_session_contract::SessionTerminalMemoryTarget,
        SessionRealizationControlFailure,
    > {
        let session_id = &intent.effect().command.session_id;
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_control)?;
        Self::require_current_live_terminal_generation(&session)?;
        session
            .authorize_terminal_memory_intent(intent)
            .map_err(|error| match error {
                awaken_session_contract::SessionMemoryReconciliationError::Cleanup(
                    awaken_session_contract::SessionCleanupError::RealizationMismatch,
                ) => SessionRealizationControlFailure::StaleOwnership,
                error => SessionRealizationControlFailure::Invalid(error.to_string()),
            })?;
        let workspace_id = self
            .session_repository()
            .owner(session_id)
            .await
            .map_err(repository_control)?;
        awaken_session_contract::SessionTerminalMemoryTarget::from_authorized_root(
            workspace_id,
            intent.clone(),
        )
        .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))
    }

    /// Project the root-only Repository publication effect under the exact
    /// realization lease that already owns terminal cleanup. The command and
    /// immutable Workspace owner are read from the Session authority on every
    /// poll; no application queue or receipt registry exists beside
    /// `SessionCleanupOperation`.
    pub(crate) async fn terminal_repository_publication_command_for_lease(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        SessionRealizationControlFailure,
    > {
        let session =
            self.session_repository()
                .get(session_id)
                .await
                .map_err(|error| match error {
                    awaken_session_contract::SessionRepositoryError::NotFound => {
                        SessionRealizationControlFailure::NotFound
                    }
                    error => SessionRealizationControlFailure::Unavailable(error.to_string()),
                })?;
        // The asserted publication generation may predate a same-generation
        // renewal. Starting a new credential/source effect still requires the
        // current root lease to be live; its exact readback travels with the
        // immutable command so transport adapters never reconstruct it.
        Self::require_current_live_terminal_generation(&session)?;
        if !Self::terminal_cleanup_lease_matches(&session, lease) {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        let terminal_cleanup = session
            .verified_terminal_cleanup()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        if terminal_cleanup.is_completed() || !terminal_cleanup.is_requested() {
            return Ok(None);
        }
        let command = terminal_cleanup
            .publication_command(session_id)
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        let Some(command) = command else {
            return Ok(None);
        };
        let workspace_id = self.owner(session_id).await.map_err(|error| match error {
            SessionMutationError::NotFound => SessionRealizationControlFailure::NotFound,
            error => SessionRealizationControlFailure::Unavailable(error.to_string()),
        })?;
        let current_lease = session.realization.clone().ok_or_else(|| {
            SessionRealizationControlFailure::Invalid(
                "terminal Repository publication has no current realization".into(),
            )
        })?;
        awaken_session_contract::SessionRepositoryPublicationProjection::try_new(
            workspace_id,
            command,
            current_lease,
        )
        .map(Some)
    }

    /// Verify and persist one exact Repository publication receipt in the same
    /// root CAS for local composition and remote Workers. Physical disposal
    /// remains a later action, so response loss can replay this receipt without
    /// losing the working tree that produced it. The current root lease, not
    /// topology, is the sole admission authority.
    pub(crate) async fn record_terminal_repository_publication_receipt_from_root(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_repository_publication_effect_from_root(
            session_id,
            lease,
            awaken_session_contract::SessionRepositoryPublicationEffect::Published(receipt),
        )
        .await
    }

    pub(crate) async fn record_terminal_repository_publication_rejection_from_root(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
        rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_repository_publication_effect_from_root(
            session_id,
            lease,
            awaken_session_contract::SessionRepositoryPublicationEffect::Rejected(rejection),
        )
        .await
    }

    async fn record_terminal_repository_publication_effect_from_root(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
        effect: awaken_session_contract::SessionRepositoryPublicationEffect,
    ) -> Result<(), SessionRealizationControlFailure> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(session_id).await.map_err(|error| {
                SessionRealizationControlFailure::Unavailable(error.to_string())
            })?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(|error| match error {
                    awaken_session_contract::SessionRepositoryError::NotFound => {
                        SessionRealizationControlFailure::NotFound
                    }
                    error => SessionRealizationControlFailure::Unavailable(error.to_string()),
                })?;
            if !session.is_terminal() {
                return Err(SessionRealizationControlFailure::Invalid(
                    "Session is not terminal".into(),
                ));
            }
            if !Self::terminal_cleanup_lease_matches(&session, lease) {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            let changed = match effect.clone() {
                awaken_session_contract::SessionRepositoryPublicationEffect::Published(receipt) => {
                    session
                        .terminal_cleanup
                        .record_repository_publication_receipt(session_id, receipt)
                }
                awaken_session_contract::SessionRepositoryPublicationEffect::Rejected(
                    rejection,
                ) => session
                    .terminal_cleanup
                    .record_repository_publication_rejection(session_id, rejection),
            }
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
            if changed {
                match self
                    .commit_resource_snapshot(
                        &owner_scope,
                        session,
                        "terminal-repository-publication-worker-outcome",
                        Vec::new(),
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(SessionMutationError::Conflict)
                        if attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                    {
                        continue;
                    }
                    Err(SessionMutationError::Conflict) => {
                        return Err(SessionRealizationControlFailure::Conflict);
                    }
                    Err(error) => {
                        return Err(SessionRealizationControlFailure::Unavailable(
                            error.to_string(),
                        ));
                    }
                }
            }
            self.wake_lifecycle_supervisor();
            return Ok(());
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    /// Persist one source-dependent preparation through the Session root. The
    /// last root preparation may enter `Disposing` only after every
    /// Session-owned Repository participant has been retired while the
    /// aggregate still retains its canonical Resource projection.
    pub(crate) async fn record_terminal_cleanup_preparation_from_root(
        &self,
        lease: &SessionRealizationLease,
        preparation: awaken_session_contract::SessionCleanupPreparation,
    ) -> Result<(), SessionRealizationControlFailure> {
        let session_id = preparation.effect.command.session_id.clone();
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(&session_id).await.map_err(mutation_control)?;
            let mut session = self
                .session_repository()
                .get(&session_id)
                .await
                .map_err(repository_control)?;
            if !session.is_terminal() {
                return Err(SessionRealizationControlFailure::Invalid(
                    "Session is not terminal".into(),
                ));
            }
            let repository_preparation =
                if let Some(durable) = session.terminal_cleanup.repository_preparation().cloned() {
                    // A delayed child or root retry may arrive after the last
                    // preparation CAS already entered Disposing. Every exact replay
                    // reuses the same aggregate-owned Repository proof.
                    Some(durable)
                } else if preparation.effect.command.thread_id == session_id {
                    // Root preparation is projected only after every child and
                    // optional publication barrier. Run the canonical
                    // Repository/Vault participant owner before admitting this
                    // last preparation; its receipt and the Runtime receipt
                    // enter Disposing in the same root CAS.
                    let _inherited_provider_disposal = session
                        .authorize_terminal_cleanup_effect(&preparation.effect)
                        .map_err(|error| match error {
                            awaken_session_contract::SessionCleanupError::RealizationMismatch => {
                                SessionRealizationControlFailure::StaleOwnership
                            }
                            error => SessionRealizationControlFailure::Invalid(error.to_string()),
                        })?;
                    let prepared = self
                        .prepare_terminal_repository_participants(&owner_scope, session)
                        .await;
                    let (next_session, receipt) = match prepared {
                        Ok(prepared) => prepared,
                        Err(SessionPreparationError::Conflict)
                            if attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                        {
                            continue;
                        }
                        Err(SessionPreparationError::Conflict) => {
                            return Err(SessionRealizationControlFailure::Conflict);
                        }
                        Err(SessionPreparationError::NotFound) => {
                            return Err(SessionRealizationControlFailure::NotFound);
                        }
                        Err(error) => {
                            return Err(SessionRealizationControlFailure::Unavailable(
                                error.to_string(),
                            ));
                        }
                    };
                    session = next_session;
                    Some(receipt)
                } else {
                    None
                };
            let changed = session
                .record_terminal_cleanup_preparation(
                    &owner_scope,
                    lease,
                    preparation.clone(),
                    repository_preparation,
                )
                .map_err(|error| match error {
                    awaken_session_contract::SessionCleanupError::RealizationMismatch => {
                        SessionRealizationControlFailure::StaleOwnership
                    }
                    error => SessionRealizationControlFailure::Invalid(error.to_string()),
                })?;
            if !changed {
                self.wake_lifecycle_supervisor();
                return Ok(());
            }
            match self
                .commit_resource_snapshot(
                    &owner_scope,
                    session,
                    "terminal-cleanup-preparation",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => {
                    self.wake_lifecycle_supervisor();
                    return Ok(());
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {}
                Err(SessionMutationError::Conflict) => {
                    return Err(SessionRealizationControlFailure::Conflict);
                }
                Err(error) => return Err(mutation_control(error)),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    /// Persist the one physical-disposal receipt and atomically retire the
    /// aggregate's Environment/Resource projection. Exact response-loss replay
    /// is absorbed by the existing Completed fingerprint.
    pub(crate) async fn record_terminal_cleanup_disposal_from_root(
        &self,
        lease: &SessionRealizationLease,
        receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        let session_id = receipt.session_id.clone();
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(&session_id).await.map_err(mutation_control)?;
            let mut session = self
                .session_repository()
                .get(&session_id)
                .await
                .map_err(repository_control)?;
            let changed = session
                .record_terminal_cleanup_disposal(
                    &owner_scope,
                    lease,
                    receipt.clone(),
                    "Session terminated before activation completed",
                )
                .map_err(|error| match error {
                    awaken_session_contract::SessionCleanupError::RealizationMismatch => {
                        SessionRealizationControlFailure::StaleOwnership
                    }
                    error => SessionRealizationControlFailure::Invalid(error.to_string()),
                })?;
            if !changed {
                self.wake_lifecycle_supervisor();
                return Ok(());
            }
            match self
                .commit_resource_snapshot(
                    &owner_scope,
                    session,
                    "terminal-cleanup-disposal",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => {
                    self.wake_lifecycle_supervisor();
                    return Ok(());
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {}
                Err(SessionMutationError::Conflict) => {
                    return Err(SessionRealizationControlFailure::Conflict);
                }
                Err(error) => return Err(mutation_control(error)),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    async fn synchronize_resource_references(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), SessionPreparationError> {
        let (Some(references), Some(files)) = (&self.resource_references, &self.resource_files)
        else {
            return Ok(());
        };
        let resource_pins = session.resources.resource_references();
        let targets = resource_targets(files.as_ref(), owner_scope, &resource_pins)
            .await
            .map_err(internal)?;
        let records = targets
            .into_iter()
            .map(|target| ResourceReferenceRecord {
                target,
                reference: ResourceReference {
                    kind: ResourceReferenceKind::SessionBinding,
                    reference_id: session.session_id.clone(),
                },
            })
            .collect();
        references
            .replace_references(
                ResourceReferenceKind::SessionBinding,
                &session.session_id,
                records,
            )
            .await
            .map_err(internal)
    }

    /// Publish the candidate's complete retention set before its root CAS. A
    /// CAS failure repairs the projection from repository truth. This ordering
    /// makes additions fail closed against reclamation; removals remain safe
    /// because [`SessionResourcePurgeGuard`] still observes the pre-CAS aggregate.
    pub(crate) async fn commit_resource_snapshot(
        &self,
        owner_scope: &str,
        candidate: PersistedSession,
        operation: &str,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<PersistedSession, SessionMutationError> {
        let session_id = candidate.session_id.clone();
        self.synchronize_resource_references(owner_scope, &candidate)
            .await
            .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?;
        match self
            .commit_session_snapshot(owner_scope, candidate, operation, lifecycle_facts)
            .await
        {
            Ok(committed) => Ok(committed),
            Err(error) => {
                self.repair_resource_references(owner_scope, &session_id)
                    .await;
                Err(error)
            }
        }
    }

    async fn commit_resource_snapshot_with_record(
        &self,
        owner_scope: &str,
        candidate: PersistedSession,
        idempotency: IdempotencyRecord,
    ) -> Result<(PersistedSession, bool), SessionMutationError> {
        let session_id = candidate.session_id.clone();
        self.synchronize_resource_references(owner_scope, &candidate)
            .await
            .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?;
        match self
            .commit_session_snapshot_with_record(owner_scope, candidate, idempotency, Vec::new())
            .await
        {
            Ok(committed) => Ok(committed),
            Err(error) => {
                self.repair_resource_references(owner_scope, &session_id)
                    .await;
                Err(error)
            }
        }
    }

    pub(crate) async fn repair_resource_references(&self, owner_scope: &str, session_id: &str) {
        let authoritative = self.session_repository().get(session_id).await;
        let repair = match authoritative {
            Ok(session) => {
                self.synchronize_resource_references(owner_scope, &session)
                    .await
            }
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                match &self.resource_references {
                    Some(references) => references
                        .replace_references(
                            ResourceReferenceKind::SessionBinding,
                            session_id,
                            Vec::new(),
                        )
                        .await
                        .map_err(internal),
                    None => Ok(()),
                }
            }
            Err(error) => Err(repository_preparation(error)),
        };
        if let Err(error) = repair {
            tracing::warn!(
                session_id,
                workspace_id = owner_scope,
                error = %error,
                "Session Resource reference repair remains pending"
            );
        }
    }

    #[must_use]
    pub fn resource_manifest_operation_id(session_id: &str, idempotency_key: &str) -> String {
        stable_fingerprint(&(session_id, idempotency_key))
    }

    fn resource_manifest_idempotency_record(
        session_id: &str,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> IdempotencyRecord {
        IdempotencyRecord {
            key: format!(
                "managed:resource-manifest-command:{session_id}:{}",
                Self::resource_manifest_operation_id(session_id, idempotency_key)
            ),
            payload_hash: request_fingerprint.to_string(),
        }
    }

    /// Resolve an exact command replay before an interface performs lowering
    /// side effects such as Repository/Vault configuration. The main command
    /// repeats this check to close the race with a concurrent first writer.
    pub async fn replay_session_resource_manifest(
        &self,
        session_id: &str,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> Result<Option<SessionResourceManifestOutcome>, SessionResourceManifestError> {
        let record = Self::resource_manifest_idempotency_record(
            session_id,
            idempotency_key,
            request_fingerprint,
        );
        let Some(receipt) = self
            .session_repository()
            .idempotency_receipt(session_id, &record.key)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?
        else {
            return Ok(None);
        };
        if receipt.payload_hash != record.payload_hash {
            return Err(SessionResourceManifestError::IdempotencyMismatch);
        }
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?;
        self.converge_resource_manifest(SessionResourceManifestOutcome {
            session,
            command_revision: receipt.committed_revision,
            command_applied: false,
        })
        .await
        .map(Some)
    }

    /// Persist one complete desired Resource manifest through the Session root
    /// CAS, then ask the existing local reconciler or WorkQueue projection to
    /// realize that exact generation. No external effect runs before this
    /// method has a durable, queryable desired manifest.
    pub async fn replace_session_resource_manifest(
        &self,
        session_id: &str,
        command: ReplaceSessionResourceManifest,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        let command_record = command.idempotency_key.as_ref().map(|key| {
            Self::resource_manifest_idempotency_record(
                session_id,
                key,
                &command.request_fingerprint,
            )
        });

        if let Some(key) = &command.idempotency_key
            && let Some(outcome) = self
                .replay_session_resource_manifest(session_id, key, &command.request_fingerprint)
                .await?
        {
            return Ok(outcome);
        }

        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            match self
                .replace_session_resource_manifest_once(
                    session_id,
                    &command,
                    command_record.clone(),
                )
                .await
            {
                Err(SessionResourceManifestError::Conflict)
                    if attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                {
                    continue;
                }
                Ok(outcome) => return self.converge_resource_manifest(outcome).await,
                result => return result,
            }
        }
        Err(SessionResourceManifestError::Conflict)
    }

    async fn replace_session_resource_manifest_once(
        &self,
        session_id: &str,
        command: &ReplaceSessionResourceManifest,
        command_record: Option<IdempotencyRecord>,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        let owner_scope = self
            .owner(session_id)
            .await
            .map_err(SessionResourceManifestError::mutation)?;
        let mut session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?;
        if session.is_terminal() {
            return Err(SessionResourceManifestError::Terminal);
        }
        if command
            .expected_resource_revision
            .is_some_and(|expected| expected != session.resources.revision)
        {
            return Err(SessionResourceManifestError::Conflict);
        }
        if let Some(baseline) = session.frozen_baseline()
            && !baseline
                .mutation_policy
                .admits_resource_replacement(session.resources.desired(), &command.resources)
        {
            return Err(SessionResourceManifestError::Rejected(
                RunError::bad_request(
                    "profiled Session resource mutation is outside its immutable policy",
                ),
            ));
        }

        let unchanged = session.resources.desired() == &command.resources;
        if !unchanged {
            let result = if session.resources.pending.is_some() {
                session
                    .resources
                    .revise_unattempted_pending(&session.session_id, command.resources.clone())
            } else {
                session
                    .resources
                    .prepare(&session.session_id, command.resources.clone())
            };
            result.map_err(|error| match error {
                awaken_session_contract::ResourceActivationError::Pending => {
                    SessionResourceManifestError::Conflict
                }
                error => {
                    SessionResourceManifestError::Rejected(RunError::bad_request(error.to_string()))
                }
            })?;
        }

        let command_receipt_key = command_record.as_ref().map(|record| record.key.clone());
        let (session, command_applied) = match command_record {
            Some(record) => self
                .commit_resource_snapshot_with_record(&owner_scope, session, record)
                .await
                .map_err(SessionResourceManifestError::mutation)?,
            None if unchanged => {
                let revision = session.revision;
                return Ok(SessionResourceManifestOutcome {
                    session,
                    command_revision: revision,
                    command_applied: false,
                });
            }
            None => (
                self.commit_resource_snapshot(
                    &owner_scope,
                    session,
                    "resource-manifest-intent",
                    Vec::new(),
                )
                .await
                .map_err(SessionResourceManifestError::mutation)?,
                true,
            ),
        };
        let command_revision = if command_applied {
            session.revision
        } else {
            let key = command_receipt_key.ok_or_else(|| {
                SessionResourceManifestError::Unavailable(
                    "replayed Resource manifest has no idempotency key".into(),
                )
            })?;
            self.session_repository()
                .idempotency_receipt(session_id, &key)
                .await
                .map_err(repository_failure)
                .map_err(SessionResourceManifestError::mutation)?
                .ok_or_else(|| {
                    SessionResourceManifestError::Unavailable(
                        "replayed Resource manifest has no idempotency receipt".into(),
                    )
                })?
                .committed_revision
        };
        Ok(SessionResourceManifestOutcome {
            session,
            command_revision,
            command_applied,
        })
    }

    async fn converge_resource_manifest(
        &self,
        mut outcome: SessionResourceManifestOutcome,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        if outcome.session.resources.pending.is_none()
            && !outcome.session.resources.has_repository_retirements()
        {
            return Ok(outcome);
        }
        let owner_scope = self
            .owner(&outcome.session.session_id)
            .await
            .map_err(SessionResourceManifestError::mutation)?;
        let convergence = if outcome.session.resources.pending.is_none() {
            self.reconcile_repository_retirements(
                &owner_scope,
                outcome.session.clone(),
                RepositoryRetirementCause::DetachedGeneration,
            )
            .await
        } else if self.requires_external_realization(&outcome.session) {
            self.dispatch_session_work(&outcome.session)
                .await
                .map(|_| outcome.session.clone())
                .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))
        } else if outcome.session.execution == SessionExecutionState::Idle {
            self.reconcile_persisted_resources(&owner_scope, outcome.session.clone())
                .await
        } else {
            Ok(outcome.session.clone())
        };
        match convergence {
            Ok(session) => {
                outcome.session = session;
                Ok(outcome)
            }
            Err(source) => Err(SessionResourceManifestError::ProjectionAfterCommit {
                outcome: Box::new(outcome),
                source,
            }),
        }
    }

    /// Rotate and repin one Session-scoped Repository credential through the
    /// durable root CAS. A conflict reloads and recompiles the exact pin.
    pub async fn rotate_repository_credential(
        &self,
        session_id: &str,
        owner_scope: &str,
        binding_id: &awaken_resource_contract::BindingId,
        material: CredentialMaterialInput,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let mut persisted = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_preparation)?;
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
        let (credential_source, credential_revision, remote_url) = persisted
            .resources
            .active
            .inputs()
            .iter()
            .find(|input| input.binding_id == *binding_id)
            .and_then(|input| match &input.source {
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
            })
            .ok_or_else(|| {
                SessionPreparationError::Rejected(RunError::bad_request(
                    "repository credential update requires an authenticated Repository resource",
                ))
            })?;
        let credential_target =
            awaken_session_contract::repository_transport_credential_target(&remote_url).map_err(
                |error| SessionPreparationError::Rejected(RunError::bad_request(error.to_string())),
            )?;
        let completed_credential_revision = ingress
            .rotate_material(CredentialMaterialRotationCommand {
                source_id: awaken_credential_contract::CredentialSourceId(credential_source),
                expected_revision: credential_revision,
                workspace_id: owner_scope.to_owned(),
                target: credential_target,
                usage: awaken_session_contract::repository_transport_credential_usage(),
                material,
            })
            .await
            .map_err(|error| {
                SessionPreparationError::Rejected(RunError::bad_request(format!(
                    "repository authorization could not be rotated: {error}"
                )))
            })?;

        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let holder = self.resource_plaintext_holder(&persisted)?;
            let mut input = persisted
                .resources
                .active
                .inputs()
                .iter()
                .find(|input| input.binding_id == *binding_id)
                .cloned()
                .ok_or(SessionPreparationError::NotFound)?;
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
            persisted.resources.active = persisted
                .resources
                .active
                .replace(input)
                .map_err(internal)?;
            match self
                .commit_session_snapshot(
                    owner_scope,
                    persisted,
                    "repository-credential-update",
                    Vec::new(),
                )
                .await
            {
                Ok(committed) => return Ok(committed),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    persisted = self
                        .session_repository()
                        .get(session_id)
                        .await
                        .map_err(repository_preparation)?;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    /// Settle the one canonical reconciliation attempt against its exact
    /// Resource generation. Root-only concurrent mutations are rebased; a
    /// competing Resource generation is never merged.
    async fn settle_resource_reconciliation(
        &self,
        owner_scope: &str,
        session_id: &str,
        resource_revision: u64,
        desired: &ResolvedSessionResources,
        settlement: ResourceSettlement,
    ) -> Result<PersistedSession, SessionPreparationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let mut current = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_preparation)?;
            if current.resources.revision != resource_revision
                || current.resources.pending.as_ref() != Some(desired)
            {
                return Err(SessionPreparationError::Conflict);
            }
            let operation = match &settlement {
                ResourceSettlement::Commit => {
                    current.resources.commit().map_err(internal)?;
                    "resource-reconcile-active"
                }
                ResourceSettlement::Rollback(error) => {
                    current
                        .resources
                        .rollback(error.clone())
                        .map_err(internal)?;
                    "resource-reconcile-rollback"
                }
                ResourceSettlement::RetryableFailure(error) => {
                    current
                        .resources
                        .note_retryable_failure(error.clone())
                        .map_err(internal)?;
                    "resource-reconcile-failed"
                }
            };
            match self
                .commit_resource_snapshot(owner_scope, current, operation, Vec::new())
                .await
            {
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
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
        mut session: PersistedSession,
        desired: &ResolvedSessionResources,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let resource_revision = session.resources.revision;
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            if session.resources.revision != resource_revision
                || session.resources.pending.as_ref() != Some(desired)
            {
                return Err(SessionPreparationError::Conflict);
            }
            let mut candidate = session.clone();
            candidate.resources.start_attempt().map_err(internal)?;
            match self
                .commit_resource_snapshot(
                    owner_scope,
                    candidate,
                    "resource-reconcile-attempt",
                    Vec::new(),
                )
                .await
            {
                Ok(committed) => return Ok(committed),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    session = self
                        .session_repository()
                        .get(&session.session_id)
                        .await
                        .map_err(repository_preparation)?;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    /// Commit the terminal delete tombstone through the canonical Session CAS.
    /// The `session.deleted` lifecycle fact belongs to the earlier atomic Delete
    /// fence; emitting it again here could duplicate delivery after an outbox
    /// consumer acknowledged the first fact.
    async fn commit_delete_tombstone(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), SessionMutationError> {
        let deleted_revision =
            SessionRevision(session.revision.0.checked_add(1).ok_or_else(|| {
                SessionMutationError::Unavailable("Session revision exhausted".into())
            })?);
        let payload = SessionMutationPayload::Delete(SessionTombstone {
            session_id: session.session_id.clone(),
            deleted_revision,
            deleted_at: deletion_timestamp(),
        });
        let payload_hash = payload.stable_hash();
        let mutation = SessionMutation {
            expected_revision: session.revision,
            idempotency: awaken_session_contract::IdempotencyRecord {
                key: format!(
                    "session:delete:{}:{}:{payload_hash}",
                    session.session_id, session.revision.0
                ),
                payload_hash,
            },
            payload,
            lifecycle_facts: Vec::new(),
        };
        match self.commit_mutation(owner_scope, mutation).await? {
            SessionMutationResult::Applied { .. } | SessionMutationResult::Replayed { .. } => {
                Ok(())
            }
            SessionMutationResult::Conflict { .. } => Err(SessionMutationError::Conflict),
            SessionMutationResult::IdempotencyMismatch => {
                Err(SessionMutationError::IdempotencyMismatch)
            }
        }
    }

    /// Reconcile every local durable Resource projection requiring convergence.
    pub async fn reconcile_resource_activations(&self) -> SessionReconciliation {
        let candidates =
            match super::scan_all_reconcilable_sessions(self.session_repository()).await {
                Ok(scan) => SessionRecoveryCandidates::from(scan),
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

    pub(super) async fn reconcile_resource_activations_from(
        &self,
        candidates: &SessionRecoveryCandidates,
    ) -> SessionReconciliation {
        let mut report = SessionReconciliation {
            pending: candidates.sessions.len(),
            quarantined: candidates.quarantined.clone(),
            ..Default::default()
        };
        for candidate in &candidates.sessions {
            let owner_scope = &candidate.workspace_id;
            let session = match self.session_repository().get(&candidate.session_id).await {
                Ok(session) => session,
                Err(awaken_session_contract::SessionRepositoryError::NotFound) => continue,
                Err(error) => {
                    report.failures.push(SessionReconciliationFailure {
                        session_id: candidate.session_id.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            let session_id = session.session_id.clone();
            if let Err(error) = self
                .synchronize_resource_references(owner_scope, &session)
                .await
            {
                report.failures.push(SessionReconciliationFailure {
                    session_id,
                    message: error.to_string(),
                });
                continue;
            }
            if (!session.is_terminal()
                && self.requires_external_realization(&session)
                && !session.resources.has_repository_retirements())
                || !session.needs_resource_reconciliation()
            {
                continue;
            }
            match self
                .reconcile_persisted_resources(owner_scope, session)
                .await
            {
                Ok(session) => report.settled.push(session),
                Err(SessionPreparationError::NotFound) => {
                    // An eager terminal-cleanup actor may commit the delete
                    // tombstone after this scan captured its candidate. The
                    // missing aggregate is then the authoritative successful
                    // outcome, not a retryable reconciliation failure. Repair
                    // the disposable reference projection from that truth.
                    self.repair_resource_references(owner_scope, &session_id)
                        .await;
                }
                Err(error) => report.failures.push(SessionReconciliationFailure {
                    session_id,
                    message: error.to_string(),
                }),
            }
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
            .ensure_repository_credentials_pinned(owner_scope, session)
            .await?;
        if !session.is_terminal() && !self.requires_external_realization(&session) {
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
        self.reconcile_prepared_resources(owner_scope, session)
            .await
    }

    /// Apply the Resource state machine only after credential migration has
    /// reached a stable Session root. Keeping these phases as separate futures
    /// also bounds synchronous poll-stack growth on the HTTP admission path.
    async fn reconcile_prepared_resources(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
    ) -> Result<PersistedSession, SessionPreparationError> {
        if !session.is_terminal() && self.requires_external_realization(&session) {
            return if session.resources.pending.is_none()
                && session.resources.has_repository_retirements()
            {
                self.reconcile_repository_retirements(
                    owner_scope,
                    session,
                    RepositoryRetirementCause::DetachedGeneration,
                )
                .await
            } else {
                Ok(session)
            };
        }
        let session_id = session.session_id.clone();
        if session.execution == SessionExecutionState::Idle && !session.is_terminal() {
            if let Some(desired) = session.resources.pending.clone() {
                let previous = session.resources.active.clone();
                let previous_revision = session.resources.active_revision();
                let transition = awaken_session_contract::SessionResourceTransition::new(
                    awaken_session_contract::SessionResourceManifest::at_revision(
                        owner_scope,
                        previous_revision,
                        previous.clone(),
                    ),
                    awaken_session_contract::SessionResourceManifest::at_revision(
                        owner_scope,
                        session.resources.revision,
                        desired.clone(),
                    ),
                )
                .map_err(internal)?;
                session = self
                    .start_resource_reconciliation(owner_scope, session, &desired)
                    .await?;
                if let Err(error) = self
                    .runtime()
                    .apply_session_inputs(&session_id, &transition)
                    .await
                {
                    let settlement = match self
                        .runtime()
                        .apply_session_inputs(&session_id, &transition.reversed())
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
                return if committed.resources.has_repository_retirements() {
                    self.reconcile_repository_retirements(
                        owner_scope,
                        committed,
                        RepositoryRetirementCause::DetachedGeneration,
                    )
                    .await
                } else {
                    Ok(committed)
                };
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
                return if session.resources.has_repository_retirements() {
                    self.reconcile_repository_retirements(
                        owner_scope,
                        session,
                        RepositoryRetirementCause::DetachedGeneration,
                    )
                    .await
                } else {
                    Ok(session)
                };
            }
            let (active_revision, active_resources) = session.resources.active_generation();
            let active = awaken_session_contract::SessionResourceManifest::at_revision(
                owner_scope,
                active_revision,
                active_resources.clone(),
            );
            let transition =
                awaken_session_contract::SessionResourceTransition::new(active.clone(), active)
                    .map_err(internal)?;
            self.runtime()
                .apply_session_inputs(&session_id, &transition)
                .await
                .map_err(SessionPreparationError::Rejected)?;
            if session.resources.activations.is_empty() {
                session.resources.adopt_legacy_active(&session_id);
                session = self
                    .commit_resource_snapshot(
                        owner_scope,
                        session,
                        "resource-adopt-legacy",
                        Vec::new(),
                    )
                    .await
                    .map_err(mutation_failure)?;
            }
            return if session.resources.has_repository_retirements() {
                self.reconcile_repository_retirements(
                    owner_scope,
                    session,
                    RepositoryRetirementCause::DetachedGeneration,
                )
                .await
            } else {
                Ok(session)
            };
        }

        match self
            .release_terminal_resources(owner_scope, &session_id)
            .await?
        {
            Some(session) => Ok(session),
            None => Ok(session),
        }
    }

    async fn reconcile_repository_retirements(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        cause: RepositoryRetirementCause,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let terminal_plan = (cause == RepositoryRetirementCause::TerminalPreparation)
            .then(|| session.resources.repository_retirements().to_vec());
        let mut terminal_index = 0;
        loop {
            let retirement = terminal_plan
                .as_ref()
                .and_then(|plan| plan.get(terminal_index))
                .cloned()
                .or_else(|| {
                    (terminal_plan.is_none())
                        .then(|| session.resources.repository_retirements().first().cloned())
                        .flatten()
                });
            let Some(retirement) = retirement else {
                return Ok(session);
            };
            let ResolvedInputSource::Repository { repository_id, .. } = &retirement.source else {
                return Err(internal(
                    "Session Repository retirement intent contains another Resource kind",
                ));
            };
            if cause == RepositoryRetirementCause::DetachedGeneration
                && session_resources_reference_repository_generation(
                    &session.resources.active,
                    repository_id.as_str(),
                )
            {
                let mut candidate = session.clone();
                candidate
                    .resources
                    .complete_repository_retirement(&retirement);
                session = self
                    .commit_resource_snapshot(
                        owner_scope,
                        candidate,
                        "repository-retirement-cancelled",
                        Vec::new(),
                    )
                    .await
                    .map_err(mutation_failure)?;
                continue;
            }
            if cause == RepositoryRetirementCause::DetachedGeneration
                && session.resources.pending.as_ref().is_some_and(|pending| {
                    session_resources_reference_repository_generation(
                        pending,
                        repository_id.as_str(),
                    )
                })
            {
                return Ok(session);
            }
            if !self
                .retire_session_repository_input(
                    owner_scope,
                    &session.session_id,
                    &retirement.source,
                )
                .await
            {
                return Err(internal(
                    "Session-scoped Repository cleanup remains pending",
                ));
            }
            if terminal_plan.is_some() {
                // Keep the full frozen plan until the preparation receipt and
                // root Runtime receipt enter Disposing in one CAS. A crash or
                // response loss replays all participant effects through their
                // existing idempotency authorities; removing entries here
                // would erase the plan identity the receipt must bind.
                terminal_index += 1;
                continue;
            }
            let mut candidate = session.clone();
            if !candidate
                .resources
                .complete_repository_retirement(&retirement)
            {
                return Err(internal(
                    "Session Repository retirement intent disappeared before completion",
                ));
            }
            match self
                .commit_resource_snapshot(
                    owner_scope,
                    candidate,
                    "repository-retirement-complete",
                    Vec::new(),
                )
                .await
            {
                Ok(committed) => session = committed,
                Err(SessionMutationError::Conflict) => {
                    let latest = self
                        .session_repository()
                        .get(&session.session_id)
                        .await
                        .map_err(repository_preparation)?;
                    if latest.is_terminal()
                        != (cause == RepositoryRetirementCause::TerminalPreparation)
                    {
                        // A lifecycle transition won the root CAS. Never carry
                        // an ordinary replacement cause into a newly terminal
                        // plan (or vice versa); the owning lifecycle driver
                        // must re-enter with the cause derived from that root.
                        return Err(SessionPreparationError::Conflict);
                    }
                    session = latest;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
    }

    /// Prepare every Session-owned Repository participant through the one
    /// ordinary retirement reconciler, while retaining the frozen Resource
    /// projection required by later physical-disposal authorization. The
    /// returned receipt is committed with the root Runtime preparation; it is
    /// never a second effect log or retirement cursor.
    async fn prepare_terminal_repository_participants(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
    ) -> Result<
        (
            PersistedSession,
            awaken_session_contract::SessionCleanupRepositoryPreparation,
        ),
        SessionPreparationError,
    > {
        if session.resources.ensure_terminal_repository_retirements() {
            session = self
                .commit_resource_snapshot(
                    owner_scope,
                    session,
                    "terminal-repository-retirement-plan",
                    Vec::new(),
                )
                .await
                .map_err(mutation_failure)?;
        }
        session = self
            .reconcile_repository_retirements(
                owner_scope,
                session,
                RepositoryRetirementCause::TerminalPreparation,
            )
            .await?;
        // Terminal mode intentionally retains the complete canonical plan in
        // the Session root. The receipt below binds that plan; participant
        // effects are replay-safe through their existing Vault/Registry
        // authorities until the same root accepts physical disposal.
        let receipt = awaken_session_contract::SessionCleanupRepositoryPreparation::new(
            &session.session_id,
            owner_scope,
            &session.resources,
        )
        .map_err(internal)?;
        Ok((session, receipt))
    }

    /// Compensate only participants created by a command that failed before a
    /// durable Session root adopted them. Exact replays belong to an earlier
    /// command and are never inferred to be disposable.
    pub async fn abort_unadopted_session_repositories(
        &self,
        configured: &[ConfiguredSessionRepository],
    ) -> bool {
        let mut retired = true;
        for configured in configured.iter().rev() {
            let adopted = match self
                .session_repository()
                .get(configured.owner.session_id())
                .await
            {
                Ok(session) => session_resources_reference_repository(
                    &session.resources,
                    configured.repository_id.as_str(),
                ),
                Err(awaken_session_contract::SessionRepositoryError::NotFound) => false,
                Err(_) => {
                    // An unavailable/corrupt root read cannot prove that a
                    // concurrent exact command did not adopt this participant.
                    retired = false;
                    continue;
                }
            };
            if adopted {
                continue;
            }
            if configured.registry_provenance == SessionParticipantProvenance::Applied {
                retired &= self
                    .retire_owned_repository(
                        &configured.workspace_id,
                        &configured.owner,
                        configured.repository_id.as_str(),
                    )
                    .await;
            }
            if let Some(credential) = &configured.credential
                && credential.provenance == SessionParticipantProvenance::Applied
            {
                retired &= self
                    .retire_owned_repository_credential(
                        &configured.workspace_id,
                        &credential.credential,
                    )
                    .await;
            }
        }
        retired
    }

    /// Retire one detached Repository input only when both its canonical
    /// Session namespace and durable owner marker agree. Platform/shared
    /// Repository inputs are intentionally a no-op.
    async fn retire_session_repository_input(
        &self,
        owner_scope: &str,
        session_id: &str,
        source: &ResolvedInputSource,
    ) -> bool {
        let ResolvedInputSource::Repository {
            repository_id,
            config,
            credential,
        } = source
        else {
            return true;
        };
        let Some(owner) = SessionRepositoryOwner::from_repository_id(repository_id.as_str())
            .filter(|owner| owner.session_id() == session_id)
        else {
            return true;
        };
        let owned_credential_binding = format!("{}:credential", repository_id.as_str());
        let owned_credential = credential
            .as_deref()
            .filter(|_| config.credential_binding.as_deref() == Some(&owned_credential_binding))
            .map(|credential| &credential.access.credential);
        self.retire_owned_repository_participants(
            owner_scope,
            &owner,
            repository_id.as_str(),
            owned_credential,
        )
        .await
        .is_complete()
    }

    pub(crate) async fn retire_owned_repository(
        &self,
        owner_scope: &str,
        owner: &SessionRepositoryOwner,
        repository_id: &str,
    ) -> bool {
        self.retire_owned_repository_participants(owner_scope, owner, repository_id, None)
            .await
            .is_complete()
    }

    async fn retire_owned_repository_participants(
        &self,
        owner_scope: &str,
        owner: &SessionRepositoryOwner,
        repository_id: &str,
        credential: Option<&awaken_credential_contract::CredentialRef>,
    ) -> OwnedRepositoryRetirement {
        let Some(catalog) = self.resource_registry() else {
            return OwnedRepositoryRetirement::Pending;
        };
        let definition = match catalog.find_repository(owner_scope, repository_id) {
            Ok(Some(definition)) => definition,
            // The prior idempotent attempt may already have retired and purged
            // the Registry participant before its Session-root completion CAS.
            // Absence therefore completes without inferring credential authority.
            Ok(None) => return OwnedRepositoryRetirement::NotOwned,
            Err(_) => return OwnedRepositoryRetirement::Pending,
        };
        if !owner.matches_definition(&definition) {
            // Namespace resemblance alone never grants delete authority. This
            // is a successful no-op for platform/shared definitions so one
            // terminal Session cannot be held open by a resource it does not
            // own.
            return OwnedRepositoryRetirement::NotOwned;
        }
        if let Some(credential) = credential
            && !self
                .retire_owned_repository_credential(owner_scope, credential)
                .await
        {
            return OwnedRepositoryRetirement::Pending;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();
        if let Some(scheduler) = self.resource_purge_scheduler()
            && scheduler
                .schedule_purge(
                    awaken_resource_contract::ResourceTarget::new(
                        owner_scope,
                        awaken_resource_contract::ResourceKind::Repository,
                        repository_id,
                    ),
                    Some(definition.current_config_version.0),
                    now,
                    now,
                )
                .await
                .is_err()
        {
            return OwnedRepositoryRetirement::Pending;
        }
        if catalog
            .change_repository_state(awaken_resource_contract::ChangeRepositoryState {
                workspace_id: owner_scope.into(),
                id: repository_id.into(),
                state: awaken_resource_contract::ResourceState::Deleted,
            })
            .is_ok()
        {
            OwnedRepositoryRetirement::Retired
        } else {
            OwnedRepositoryRetirement::Pending
        }
    }

    async fn retire_owned_repository_credential(
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
