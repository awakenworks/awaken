// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Serves the Worker terminal-cleanup control roles by forwarding each step to its
// @formal reason: root-derived claim, authorization or receipt owner in this component's modules.
//! Terminal cleanup of a terminal Session: the one durable cleanup operation
//! the Session aggregate owns, scheduled from this application.
//!
//! One use case: release everything a terminal Session still holds. The
//! co-located drive runs the canonical terminal cleanup driver for a local
//! Session; a Worker-owned Session is claimed by an external Worker through
//! the control roles implemented here, and every preparation, disposal and
//! Repository publication outcome is recorded through the Session root CAS.
//! The aggregate decides each step; this component orders reads, decisions,
//! effects and commits and holds no cleanup state of its own.

use std::sync::Arc;

use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_realization_contract::{
    SessionRealizationControlFailure, SessionTerminalCleanupAssignment,
};
use awaken_session_execution_contract::{
    CoordinationRuntime, RealizationEffects, RealizationRuntime, SessionCoordinationReads,
    TerminalCleanupRuntime,
};
use awaken_time::WallClockExt as _;

use crate::realization::{unavailable, validate_realization_target};

mod claims;
mod local_drive;
mod records;

pub struct SessionTerminalCleanups {
    mutations: Arc<crate::SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
    queries: Arc<crate::SessionQueries>,
    lifecycle: Arc<crate::SessionLifecycle>,
    resource_references: Arc<crate::SessionResourceReferences>,
    repository_participants: Arc<crate::SessionRepositoryParticipants>,
    work_assignments: Arc<crate::SessionWorkAssignments>,
    realizations: Arc<crate::SessionRealizations>,
    executable_projections: crate::ExecutableProjections,
    execution_admission: Arc<crate::SessionExecutionAdmission>,
    realization: Arc<dyn RealizationRuntime>,
    realization_effects: Arc<dyn RealizationEffects>,
    cleanup: Arc<dyn TerminalCleanupRuntime>,
    coordination: Arc<dyn CoordinationRuntime>,
    coordination_reads: Arc<dyn SessionCoordinationReads>,
}

/// The collaborators one [`SessionTerminalCleanups`] is built from.
pub(crate) struct SessionTerminalCleanupCollaborators {
    pub(crate) mutations: Arc<crate::SessionMutations>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) queries: Arc<crate::SessionQueries>,
    pub(crate) lifecycle: Arc<crate::SessionLifecycle>,
    pub(crate) resource_references: Arc<crate::SessionResourceReferences>,
    pub(crate) repository_participants: Arc<crate::SessionRepositoryParticipants>,
    pub(crate) work_assignments: Arc<crate::SessionWorkAssignments>,
    pub(crate) realizations: Arc<crate::SessionRealizations>,
    pub(crate) executable_projections: crate::ExecutableProjections,
    pub(crate) execution_admission: Arc<crate::SessionExecutionAdmission>,
    pub(crate) runtime: crate::SessionExecutor,
}

impl SessionTerminalCleanups {
    pub(crate) fn new(collaborators: SessionTerminalCleanupCollaborators) -> Self {
        let SessionTerminalCleanupCollaborators {
            mutations,
            clock,
            queries,
            lifecycle,
            resource_references,
            repository_participants,
            work_assignments,
            realizations,
            executable_projections,
            execution_admission,
            runtime,
        } = collaborators;
        Self {
            mutations,
            clock,
            queries,
            lifecycle,
            resource_references,
            repository_participants,
            work_assignments,
            realizations,
            executable_projections,
            execution_admission,
            realization: runtime.realization.clone(),
            realization_effects: runtime.realization_effects.clone(),
            cleanup: runtime.terminal_cleanup.clone(),
            coordination: runtime.coordination.clone(),
            coordination_reads: runtime.coordination_reads.clone(),
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalCleanupQueue for SessionTerminalCleanups {
    async fn claim_next_terminal_cleanup(
        &self,
        target: awaken_session_realization_contract::SessionRealizationTarget,
        after_session_id: Option<&awaken_session_types::SessionId>,
    ) -> Result<Option<SessionTerminalCleanupAssignment>, SessionRealizationControlFailure> {
        validate_realization_target(&target, self.clock.now_ms())?;
        if target.reassign_existing_lease {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal cleanup recovery claims cannot reassign a live logical owner".into(),
            ));
        }
        let admission = self
            .executable_projections
            .refresh()
            .await
            .map_err(unavailable)?;
        admission
            .enter(|| self.claim_terminal_cleanup_after_refresh(target, after_session_id))
            .await
    }

    async fn terminal_cleanup_work(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionTerminalCleanupWork>,
        SessionRealizationControlFailure,
    > {
        self.terminal_cleanup_work_for_lease(session_id, lease)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalPreparationControl
    for SessionTerminalCleanups
{
    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
        SessionRealizationControlFailure,
    > {
        self.authorize_terminal_cleanup_effect_from_root(effect)
            .await
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        lease: &SessionRealizationLease,
        preparation: awaken_session_state_contract::SessionCleanupPreparation,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_cleanup_preparation_from_root(lease, preparation)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalDisposalControl
    for SessionTerminalCleanups
{
    async fn authorize_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, SessionRealizationControlFailure> {
        self.authorize_terminal_cleanup_disposal_from_root(effect)
            .await
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        lease: &SessionRealizationLease,
        receipt: awaken_session_state_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_cleanup_disposal_from_root(lease, receipt)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalRepositoryPublicationControl
    for SessionTerminalCleanups
{
    async fn terminal_repository_publication_command(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionRepositoryPublicationProjection>,
        SessionRealizationControlFailure,
    > {
        self.terminal_repository_publication_command_for_lease(session_id, lease)
            .await
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &SessionRealizationLease,
        receipt: awaken_session_state_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_repository_publication_receipt_from_root(session_id, lease, receipt)
            .await
    }

    async fn record_terminal_repository_publication_rejection(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &SessionRealizationLease,
        rejection: awaken_session_state_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_repository_publication_rejection_from_root(
            session_id, lease, rejection,
        )
        .await
    }
}
