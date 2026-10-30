// @formal entrypoint agent_profile.requested_revision_never_follows_mutable_current
// @formal entrypoint executable_projection.failed_refresh_never_enters_handler
// @formal entrypoint session_creation.network_intersection_is_canonical_and_non_widening
// @formal entrypoint session_creation.selected_environment_is_frozen_in_baseline
// @formal entrypoint session_event_batch.run_reservation_precedes_distinct_activity_receipt
// @formal entrypoint session_execution.activity_receipts_are_unique_durable_and_exactly_settled
// @formal entrypoint session_run.reservation_precedes_activity
//! Session-owned admission in front of the neutral Run application port.
//!
//! Runtime Host executes an already-admitted Run. Session creation/recovery is
//! application policy, so it is configured here once for every public protocol.

use crate::{SessionProjectionReads as _, SessionRootReads as _};

#[cfg(test)]
mod error_classification_tests;
mod events;
use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_session_application_contract::{
    OwnerScopedRunApplication, Pending, RunApplication, RunApplicationError, RunResume, StepOutcome,
};
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, SessionRunActivityReceiptPlan, SessionRunDelivery,
    session_run_activity_operation_id, session_run_activity_receipt_plan,
};
use awaken_session_state_contract::SessionRepositoryError;
use awaken_session_types::RunError;
use awaken_session_types::SessionRevision;

use crate::{
    CreateProfiledSessionCommand, SessionCreationError, SessionMutationError,
    SessionPreparationError, session_repository_error,
};

pub struct RecoveredSessionProjection {
    pub owner_scope: String,
    pub session: awaken_session_state_contract::PersistedSession,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionProjectionRecoveryError {
    #[error("Session was not found")]
    NotFound,
    #[error(transparent)]
    Rejected(#[from] RunError),
    #[error("Session recovery is unavailable: {0}")]
    Unavailable(String),
}

pub(crate) fn mutation_error(error: SessionMutationError) -> RunError {
    match error {
        SessionMutationError::Conflict => {
            RunError::unavailable("Session changed while it was being admitted")
        }
        error => error.into(),
    }
}

pub(crate) fn preparation_error(error: SessionPreparationError) -> RunError {
    match error {
        SessionPreparationError::Rejected(error) => error,
        SessionPreparationError::Unavailable(message) => RunError::unavailable(message),
        SessionPreparationError::NotFound => RunError::bad_request("Session was not found"),
        SessionPreparationError::Conflict => {
            RunError::unavailable("Session changed while it was being admitted")
        }
    }
}

fn creation_error(error: SessionCreationError) -> RunError {
    match error {
        SessionCreationError::Rejected(error) => error,
        SessionCreationError::Conflict => RunError::unavailable("Session creation conflicted"),
        SessionCreationError::Tombstoned => {
            RunError::bad_request("Session identity is terminally occupied")
        }
        SessionCreationError::IdempotencyMismatch => {
            RunError::bad_request("Session creation idempotency mismatch")
        }
        SessionCreationError::Unavailable(message) => RunError::unavailable(message),
        SessionCreationError::Internal(message) => SessionMutationError::Internal(message).into(),
    }
}

/// Session Run admission: reserve one Run against the durable Session root,
/// open its activity, activate or observe it through the coordination runtime,
/// and recover the Session's disposable Runtime projection before that. A
/// first Run of an unknown Thread creates its default profiled Session.
///
/// It holds the CAS kernel and read component, the activity, creation,
/// Resource reconciliation, realization and Work components a Run admission
/// passes through, the executable-projection refresh, the execution
/// observations, and the coordination and Thread runtime roles.
pub struct SessionRunAdmissions {
    mutations: Arc<crate::SessionMutations>,
    queries: Arc<crate::SessionQueries>,
    activities: Arc<crate::SessionActivities>,
    creation_commands: Arc<crate::SessionCreationCommands>,
    resource_reconciliation: Arc<crate::SessionResourceReconciliation>,
    realizations: Arc<crate::SessionRealizations>,
    work_assignments: Arc<crate::SessionWorkAssignments>,
    executable_projections: crate::ExecutableProjections,
    execution_admission: Arc<crate::SessionExecutionAdmission>,
    coordination: Arc<dyn awaken_session_execution_contract::CoordinationRuntime>,
    thread_reads: Arc<dyn awaken_session_execution_contract::ThreadReadRuntime>,
    streaming_runs: Arc<dyn awaken_session_execution_contract::StreamingRunRuntime>,
}

/// The collaborators one [`SessionRunAdmissions`] is built from.
pub(crate) struct SessionRunAdmissionCollaborators {
    pub(crate) mutations: Arc<crate::SessionMutations>,
    pub(crate) queries: Arc<crate::SessionQueries>,
    pub(crate) activities: Arc<crate::SessionActivities>,
    pub(crate) creation_commands: Arc<crate::SessionCreationCommands>,
    pub(crate) resource_reconciliation: Arc<crate::SessionResourceReconciliation>,
    pub(crate) realizations: Arc<crate::SessionRealizations>,
    pub(crate) work_assignments: Arc<crate::SessionWorkAssignments>,
    pub(crate) executable_projections: crate::ExecutableProjections,
    pub(crate) execution_admission: Arc<crate::SessionExecutionAdmission>,
    pub(crate) runtime: crate::SessionExecutor,
}

struct RunActivityAdmission {
    session_id: awaken_session_types::SessionId,
    run_id: awaken_agent_contract::agent::run::Id,
    projected_revision: SessionRevision,
    existing_activity_epoch: Option<u64>,
    prepared: bool,
}

impl SessionRunAdmissions {
    /// One borrowed admission role for new Outcome Worker/ACK acquisitions. It owns
    /// no Outcome state and is never retained by the Outcome runtime.
    pub fn outcome_run_admission<'a>(
        &'a self,
        session: &'a str,
    ) -> impl awaken_session_execution_contract::PreparedSessionRunAdmission + 'a {
        SessionOutcomeRunAdmission {
            admissions: self,
            session,
        }
    }

    async fn run_prepared_activation(
        &self,
        session: &str,
        activation: awaken_runtime_contract::RunActivation,
    ) -> Result<awaken_agent_contract::agent::run::RunState, RunError> {
        use awaken_session_execution_contract::AdmitPreparedSessionRun;
        if activation.thread_id.0 != session || activation.input.is_empty() {
            return Err(RunError::bad_request(
                "prepared Outcome Run does not match its Session",
            ));
        }
        let session_id = awaken_session_types::SessionId::from(session);
        let run_id = activation.run_id.clone();
        let operation = session_run_activity_operation_id(&session_id, &run_id);
        let command = AdmitPreparedSessionRun {
            session_id: session_id.clone(),
            operation_id: operation.clone(),
            activation,
            traceparent: None,
        };
        // Freeze identity before projection refresh, continuation or admission.
        let _fingerprint = command.command_fingerprint();
        let thread_id = awaken_agent_contract::ThreadId(session.to_owned());
        let previous = self
            .activities
            .recover_activity_for_operation(&session_id, &operation, Some((&thread_id, &run_id)))
            .await
            .map_err(crate::SessionActivityError::run_error)?;
        let root = self
            .mutations
            .session_aggregate(&session_id)
            .await
            .map_err(crate::session_repository_error)?;
        if previous.is_none() && !root.budget.can_admit_model_request() {
            // This is not a durable refusal. Reserving now would permanently
            // close the stable ID instead of allowing a later cap raise.
            return Err(RunError::unavailable_classified(
                "session_budget_deferred",
                "Session budget currently defers a new Outcome Run",
            ));
        }
        let owner = self
            .mutations
            .owner(&session_id)
            .await
            .map_err(mutation_error)?;
        let revision = self
            .admit_run_session(
                &owner,
                session,
                &command.activation.snapshot.root_agent_id.0,
            )
            .await?;
        let revision = self
            .activities
            .prepare_run_activity_revision(&session_id, revision)
            .await
            .map_err(crate::SessionActivityError::run_error)?;
        let activity = PreparedRunActivity {
            admissions: self,
            session_id,
            run_id,
            projected_revision: revision,
        };
        self.coordination
            .run_prepared_session_run(command, &activity)
            .await
    }
    pub(crate) fn new(collaborators: SessionRunAdmissionCollaborators) -> Self {
        let SessionRunAdmissionCollaborators {
            mutations,
            queries,
            activities,
            creation_commands,
            resource_reconciliation,
            realizations,
            work_assignments,
            executable_projections,
            execution_admission,
            runtime,
        } = collaborators;
        Self {
            mutations,
            queries,
            activities,
            creation_commands,
            resource_reconciliation,
            realizations,
            work_assignments,
            executable_projections,
            execution_admission,
            coordination: runtime.coordination.clone(),
            thread_reads: runtime.thread_reads.clone(),
            streaming_runs: runtime.streaming_runs.clone(),
        }
    }

    /// Interrupt the Run currently executing on one Session Thread.
    pub async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.streaming_runs.interrupt(thread).await
    }

    /// Reserve one stable Run, then commit or recover the exact Session activity
    /// receipt before activation. This is the sole durable Session Run admission
    /// boundary used by both public protocols and Event reconciliation.
    pub async fn admit_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, RunError> {
        self.admit_session_run_with_owner(command, None, false)
            .await
    }

    /// Admit one Run on a Session its caller owns. Ownership was decided when
    /// `access` was minted, so this never creates a Session: a Session deleted
    /// since then answers like any missing one.
    pub async fn admit_session_run_for_owner(
        &self,
        access: &crate::SessionAccess,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, RunError> {
        if command.session_id != *access.session_id() {
            return Err(crate::SessionAccessError::NotFound.into());
        }
        self.admit_session_run_with_owner(command, Some(access.owner_scope()), false)
            .await
    }

    /// The single-tenant first-Run path: create the default profiled Session
    /// when it is missing. Only this crate's own Run application uses it, with
    /// the Workspace it derives itself.
    pub(crate) async fn admit_or_create_session_run(
        &self,
        owner_scope: &str,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, RunError> {
        self.admit_session_run_with_owner(command, Some(owner_scope), true)
            .await
    }

    /// Admit one product-authored Run on an existing profiled Session under
    /// its authenticated owner. The profiled edge never addresses a Managed
    /// Session and never creates one.
    ///
    /// | Rule | Cause | Effect |
    /// |---|---|---|
    /// | P1 | no root visible to the owner, or no frozen baseline | `NotFound` |
    /// | P2 | the frozen mutation policy is Managed | `NotFound` |
    /// | P3 | the Run Agent differs from the frozen Agent | rejected |
    /// | P4 | otherwise | the shared owner-fenced durable admission |
    pub async fn admit_profiled_session_run(
        &self,
        access: &crate::SessionAccess,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, crate::SessionProjectionRecoveryError> {
        let projection = self
            .queries
            .read_session_projection(command.session_id.as_str(), Some(access.owner_scope()))
            .await?
            .ok_or(crate::SessionProjectionRecoveryError::NotFound)?;
        let baseline = projection
            .session
            .frozen_baseline()
            .ok_or(crate::SessionProjectionRecoveryError::NotFound)?;
        if baseline.mutation_policy.is_managed() {
            return Err(crate::SessionProjectionRecoveryError::NotFound);
        }
        if baseline.agent_id != command.agent_id {
            return Err(crate::SessionProjectionRecoveryError::Rejected(
                RunError::bad_request(
                    "profiled Session Run Agent does not match the frozen Session",
                ),
            ));
        }
        self.admit_session_run_for_owner(access, command)
            .await
            .map_err(crate::SessionProjectionRecoveryError::Rejected)
    }

    fn admit_session_run_with_owner<'a>(
        &'a self,
        command: AdmitSessionRun,
        expected_owner: Option<&'a str>,
        create_missing: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AdmittedSessionRun, RunError>> + Send + 'a>,
    > {
        let coordination_runtime = self.coordination.as_ref();

        // Return a boxed future at the application boundary. The admission
        // graph is intentionally deep; boxing here prevents protocol and
        // lifecycle callers from moving its complete state machine through a
        // default Tokio worker stack.
        Box::pin(async move {
            let session_id = command.session_id.clone();
            let agent_id = command.agent_id.clone();
            let run_id = command.run_id.clone();
            let thread_id = awaken_agent_contract::ThreadId(session_id.to_string());
            let operation = session_run_activity_operation_id(&session_id, &run_id);
            // Cause/effect decision table: an existing exact activity receipt is
            // response-loss truth and outranks current policy, but it says nothing
            // about this process's disposable Runtime projection. Every admission
            // therefore recovers the authoritative Session projection before Runtime
            // freezes the dispatch; only activity creation is skipped on receipt
            // replay. Reservation remains non-executable until the exact epoch is
            // activated.
            let durable_owner = match self.mutations.owner(&session_id).await {
                Ok(actual_owner) => {
                    if expected_owner.is_some_and(|expected| expected != actual_owner) {
                        return Err(RunError::bad_request("Session was not found"));
                    }
                    Some(actual_owner)
                }
                Err(SessionMutationError::NotFound) if create_missing => None,
                Err(error) => return Err(mutation_error(error)),
            };
            let existing_activity_epoch = match durable_owner.as_ref() {
                Some(_) => self
                    .activities
                    .recover_activity_for_operation(
                        &session_id,
                        &operation,
                        Some((&thread_id, &run_id)),
                    )
                    .await
                    .map_err(crate::SessionActivityError::run_error)?
                    .map(|(_, epoch)| epoch),
                None => None,
            };
            let owner_scope = durable_owner
                .as_deref()
                .or(expected_owner)
                .ok_or_else(|| RunError::bad_request("Session was not found"))?;
            let projected_revision = self
                .admit_run_session(owner_scope, session_id.as_str(), &agent_id)
                .await?;
            let reservation = coordination_runtime.reserve_session_run(command).await?;
            self.finish_run_admission(
                RunActivityAdmission {
                    session_id,
                    run_id,
                    projected_revision,
                    existing_activity_epoch,
                    prepared: false,
                },
                reservation,
                false,
                None,
            )
            .await
        })
    }

    async fn finish_run_admission(
        &self,
        admission: RunActivityAdmission,
        reservation: awaken_session_execution_contract::SessionRunReservation,
        recover_only: bool,
        admitted_after_cursor: Option<u64>,
    ) -> Result<AdmittedSessionRun, RunError> {
        let RunActivityAdmission {
            session_id,
            run_id,
            projected_revision,
            existing_activity_epoch,
            prepared,
        } = admission;
        let thread_id = awaken_agent_contract::ThreadId(session_id.to_string());
        let operation = session_run_activity_operation_id(&session_id, &run_id);
        let delivery = |session_activity_epoch| SessionRunDelivery {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            session_activity_epoch,
        };

        match session_run_activity_receipt_plan(reservation, existing_activity_epoch) {
            SessionRunActivityReceiptPlan::CommitActivity {
                reservation_replayed,
            } => {
                let session_activity_epoch = if recover_only {
                    return Err(RunError::unavailable_classified(
                        "session_run_admission_pending",
                        "Run has no committed activity receipt",
                    ));
                } else if prepared {
                    self.activities
                        .commit_prepared_run_activity(
                            &session_id,
                            &operation,
                            projected_revision,
                            (&thread_id, &run_id),
                            admitted_after_cursor,
                        )
                        .await?
                } else {
                    self.activities
                        .begin_run_activity_at_revision(
                            &session_id,
                            &operation,
                            projected_revision,
                            (&thread_id, &run_id),
                        )
                        .await?
                };
                Ok(if reservation_replayed {
                    AdmittedSessionRun::AlreadyReserved(delivery(session_activity_epoch))
                } else {
                    AdmittedSessionRun::Reserved(delivery(session_activity_epoch))
                })
            }
            SessionRunActivityReceiptPlan::UseReceipt {
                session_activity_epoch,
                reservation_replayed,
            } => Ok(if reservation_replayed {
                AdmittedSessionRun::AlreadyReserved(delivery(session_activity_epoch))
            } else {
                AdmittedSessionRun::Reserved(delivery(session_activity_epoch))
            }),
            SessionRunActivityReceiptPlan::UseActivatedReceipt {
                session_activity_epoch,
            } => Ok(AdmittedSessionRun::AlreadyActivated(delivery(
                session_activity_epoch,
            ))),
            SessionRunActivityReceiptPlan::RecoveryClaimed => {
                Ok(AdmittedSessionRun::RecoveryClaimed { session_id, run_id })
            }
            SessionRunActivityReceiptPlan::Completed => {
                Ok(AdmittedSessionRun::Completed { session_id, run_id })
            }
            SessionRunActivityReceiptPlan::RejectInvalidReceipt => Err(RunError::internal(
                "reserved or activated Session Run has no activity epoch",
            )),
        }
    }

    /// Admit, register before activation, and observe one foreground Run for
    /// a Session the caller owns. Durable reservation and foreground observation
    /// use the canonical admission and coordination ports.
    pub async fn run_admitted_session_for_owner(
        &self,
        access: &crate::SessionAccess,
        command: AdmitSessionRun,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let input_message_ids = command.input_message_ids();
        let admitted = Box::pin(self.admit_session_run_for_owner(access, command)).await?;
        Box::pin(self.coordination.activate_and_observe_session_run(
            admitted,
            input_message_ids,
            sink,
        ))
        .await
    }

    /// Publish one already-admitted Session Run without observing its terminal
    /// result. Background product coordinators use this after
    /// [`Self::admit_session_run_for_owner`] and consume the canonical lifecycle
    /// feed independently; they never need access to the dispatch store.
    pub async fn activate_admitted_session_run(
        &self,
        admitted: AdmittedSessionRun,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, RunError> {
        let coordination_runtime = self.coordination.as_ref();

        match admitted {
            AdmittedSessionRun::Reserved(delivery)
            | AdmittedSessionRun::AlreadyReserved(delivery)
            | AdmittedSessionRun::AlreadyActivated(delivery) => {
                coordination_runtime.activate_session_run(delivery).await
            }
            AdmittedSessionRun::RecoveryClaimed { .. } => {
                Ok(awaken_session_execution_contract::SessionRunActivation::RecoveryClaimed)
            }
            AdmittedSessionRun::Completed { .. } => {
                Ok(awaken_session_execution_contract::SessionRunActivation::Completed)
            }
        }
    }

    /// Rebuild the disposable Runtime projection at an execution admission
    /// boundary. This is deliberately separate from [`crate::SessionQueries::read_session_projection`].
    pub async fn recover_session_projection(
        &self,
        thread_id: &str,
        expected_owner: Option<&str>,
    ) -> Result<Option<RecoveredSessionProjection>, SessionProjectionRecoveryError> {
        let Some(recovered) = self
            .queries
            .read_session_projection(thread_id, expected_owner)
            .await?
        else {
            return Ok(None);
        };
        // Archived Sessions remain readable projections but deny every new
        // effect. Recovery must not reacquire a realization lease for them.
        if recovered.session.is_terminal() {
            return Ok(Some(recovered));
        }
        let admission = self
            .executable_projections
            .refresh()
            .await
            .map_err(|error| {
                SessionProjectionRecoveryError::Unavailable(format!(
                    "executable projection refresh failed: {error}"
                ))
            })?;
        let owner = recovered.owner_scope;
        // Resource convergence is the largest Session state machine. Keep its
        // future behind one heap indirection at this application boundary so
        // protocol handlers do not synchronously embed the complete recovery,
        // credential, and Resource reconciliation graph in their poll stack.
        // The owned aggregate still crosses exactly one domain boundary; this
        // is execution-shape isolation, not another persistence abstraction.
        let session = Box::pin(admission.enter(|| {
            self.resource_reconciliation
                .reconcile_persisted_resources(&owner, recovered.session)
        }))
        .await
        .map_err(|error| SessionProjectionRecoveryError::Rejected(preparation_error(error)))?;
        let requires_external_realization = session.requires_external_environment_owner();
        let session = if requires_external_realization {
            self.realizations
                .install_dispatch_projection(&owner, &session)
                .await
                .map_err(|error| SessionProjectionRecoveryError::Rejected(error.into()))?;
            self.work_assignments
                .dispatch(&session)
                .await
                .map_err(|error| {
                    SessionProjectionRecoveryError::Rejected(RunError::unavailable_classified(
                        "session_work_dispatch_failed",
                        format!("Session realization work could not be dispatched: {error}"),
                    ))
                })?;
            session
        } else if !session.environment.admits_realization_effects() {
            // Suspending/Hibernated/Restoring are canonical continuation
            // phases, not invalid realization states. Install only the
            // complete non-physical dispatch projection so the durable Run can
            // be reserved. Its later activity boundary joins the one
            // continuation operation and performs the Resident handoff before
            // activation; no realization effect may run against the old phase.
            self.realizations
                .install_dispatch_projection(&owner, &session)
                .await
                .map_err(|error| SessionProjectionRecoveryError::Rejected(error.into()))?;
            session
        } else {
            self.realizations
                .realize_session_after_refresh(&awaken_session_types::SessionId::from(thread_id))
                .await
                .map_err(|error| SessionProjectionRecoveryError::Rejected(error.into()))?
        };
        Ok(Some(RecoveredSessionProjection {
            owner_scope: owner,
            session,
        }))
    }

    async fn recover_admitted_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
    ) -> Result<SessionRevision, RunApplicationError> {
        let recovered = self
            .recover_session_projection(thread_id, Some(workspace_id))
            .await
            .map_err(|error| match error {
                SessionProjectionRecoveryError::NotFound => {
                    RunError::bad_request("Session was not found")
                }
                SessionProjectionRecoveryError::Rejected(error) => error,
                SessionProjectionRecoveryError::Unavailable(message) => {
                    RunError::unavailable(message)
                }
            })?
            .ok_or_else(|| RunError::bad_request("Session was not found"))?;
        if recovered.session.is_terminal() {
            return Err(RunError::bad_request("Session no longer accepts new Runs"));
        }
        if let Some(baseline) = recovered
            .session
            .frozen_baseline()
            .filter(|baseline| baseline.environment.self_hosted)
        {
            self.work_assignments
                .wake(
                    &baseline.environment.environment_id,
                    &recovered.session.session_id,
                )
                .await
                .map_err(|error| {
                    RunError::unavailable_classified(
                        "session_work_wake_failed",
                        format!("Session Work could not be awakened: {error}"),
                    )
                })?;
        }
        if !recovered.session.requires_external_environment_owner()
            && !recovered.session.execution.admits_activity()
        {
            return Err(RunError::unavailable_classified(
                "session_not_ready",
                format!(
                    "Session realization has not completed; current state is `{}`",
                    recovered.session.execution
                ),
            ));
        }
        Ok(recovered.session.revision)
    }

    async fn create_default_run_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<SessionRevision, RunApplicationError> {
        self.creation_commands
            .create_profiled_session(CreateProfiledSessionCommand {
                owner_scope: workspace_id.to_string(),
                session_id: thread_id.to_string(),
                mutation_policy: crate::session_resource::SessionMutationPolicy::Managed,
                application_origin: None,
                agent_id: agent_id.to_string(),
                source_revision: None,
                environment_id: None,
                model: None,
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                resource_inputs: Vec::new(),
                mcp_candidates: Vec::new(),
                repositories: Vec::new(),
                network_restriction: None,
                title: None,
                metadata: Default::default(),
                tools: None,
                idempotency: None,
            })
            .await
            .map_err(creation_error)
            .map(|session| session.revision)
    }

    pub async fn admit_run_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<SessionRevision, RunApplicationError> {
        match self
            .queries
            .session(&awaken_session_types::SessionId::from(thread_id))
            .await
        {
            Ok(_) => self.recover_admitted_session(workspace_id, thread_id).await,
            Err(SessionRepositoryError::NotFound) => {
                match self
                    .create_default_run_session(workspace_id, thread_id, agent_id)
                    .await
                {
                    Ok(revision) => Ok(revision),
                    // Concurrent first Runs race only at the durable create fence;
                    // every loser adopts the exact winner through the same recovery.
                    Err(error) if error.kind == awaken_session_types::RunErrorKind::Unavailable => {
                        match self
                            .queries
                            .session(&awaken_session_types::SessionId::from(thread_id))
                            .await
                        {
                            Ok(_) => self.recover_admitted_session(workspace_id, thread_id).await,
                            Err(SessionRepositoryError::NotFound) => Err(error),
                            Err(error) => Err(session_repository_error(error)),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(session_repository_error(error)),
        }
    }
}

struct SessionOutcomeRunAdmission<'a> {
    admissions: &'a SessionRunAdmissions,
    session: &'a str,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::PreparedSessionRunAdmission
    for SessionOutcomeRunAdmission<'_>
{
    async fn admit(
        &self,
        activation: awaken_runtime_contract::RunActivation,
    ) -> awaken_runtime_contract::diagnostic::Result<awaken_agent_contract::agent::run::RunState>
    {
        use awaken_runtime_contract::diagnostic::{
            RunAdmissionDisposition, RunAdmissionFailure, RuntimeDiagnostic, RuntimeErrorKind,
        };
        let run_id = activation.run_id.clone();
        match self
            .admissions
            .run_prepared_activation(self.session, activation)
            .await
        {
            Ok(state) => Ok(state),
            Err(error) => {
                let session_id = awaken_session_types::SessionId::from(self.session);
                let thread_id = awaken_agent_contract::ThreadId(self.session.to_owned());
                let operation = session_run_activity_operation_id(&session_id, &run_id);
                // Codes are projection vocabulary, not proof of a closed
                // decision. Only the original activity owner's typed replay
                // of this exact canonical negative can mint Closed. An outage,
                // corrupt hash or a positive receipt remains Deferred.
                let closed = error.code == "budget_reached"
                    && matches!(
                        self.admissions
                            .activities
                            .recover_activity_for_operation(
                                &session_id,
                                &operation,
                                Some((&thread_id, &run_id)),
                            )
                            .await,
                        Err(crate::SessionActivityError::BudgetReached),
                    );
                let disposition = if closed {
                    RunAdmissionDisposition::Closed
                } else {
                    RunAdmissionDisposition::Deferred
                };
                Err(RuntimeDiagnostic::new(
                    RuntimeErrorKind::Unavailable,
                    error.code.clone(),
                    RunAdmissionFailure::new(disposition, error),
                )
                .into())
            }
        }
    }
}

struct PreparedRunActivity<'a> {
    admissions: &'a SessionRunAdmissions,
    session_id: awaken_session_types::SessionId,
    run_id: awaken_agent_contract::agent::run::Id,
    projected_revision: SessionRevision,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRunActivityAdmitter for PreparedRunActivity<'_> {
    async fn admit(
        &self,
        reservation: awaken_session_execution_contract::SessionRunReservation,
        recover_only: bool,
        admitted_after_cursor: Option<u64>,
    ) -> Result<AdmittedSessionRun, RunError> {
        let thread = awaken_agent_contract::ThreadId(self.session_id.to_string());
        let operation = session_run_activity_operation_id(&self.session_id, &self.run_id);
        let epoch = self
            .admissions
            .activities
            .recover_activity_for_operation(
                &self.session_id,
                &operation,
                Some((&thread, &self.run_id)),
            )
            .await
            .map_err(crate::SessionActivityError::run_error)?
            .map(|(_, epoch)| epoch);
        self.admissions
            .finish_run_admission(
                RunActivityAdmission {
                    session_id: self.session_id.clone(),
                    run_id: self.run_id.clone(),
                    projected_revision: self.projected_revision,
                    existing_activity_epoch: epoch,
                    prepared: true,
                },
                reservation,
                recover_only,
                admitted_after_cursor,
            )
            .await
    }
}

#[async_trait::async_trait]
impl crate::ProfiledSessionCommands for SessionRunAdmissions {
    async fn accept_profiled_session(
        &self,
        command: CreateProfiledSessionCommand,
    ) -> Result<awaken_session_state_contract::PersistedSession, SessionCreationError> {
        self.creation_commands
            .accept_profiled_session(command)
            .await
    }

    async fn admit_profiled_session_run(
        &self,
        access: &crate::SessionAccess,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, SessionProjectionRecoveryError> {
        Box::pin(SessionRunAdmissions::admit_profiled_session_run(
            self, access, command,
        ))
        .await
    }

    async fn activate_admitted_session_run(
        &self,
        admitted: AdmittedSessionRun,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, RunError> {
        Box::pin(SessionRunAdmissions::activate_admitted_session_run(
            self, admitted,
        ))
        .await
    }

    async fn session_run_events_after(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
        dispatch_after: awaken_dispatch_contract::DispatchCursor,
        lifecycle_after: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_dispatch_contract::SessionRunEventPage, SessionProjectionRecoveryError> {
        SessionRunAdmissions::session_run_events_after(
            self,
            owner_scope,
            session_id,
            run_id,
            dispatch_after,
            lifecycle_after,
            limit,
        )
        .await
    }
}

/// The sole durable Session Run application for all public Run protocols.
/// The resolvers read the Runtime's installed Session; an unreadable one is
/// refused, never answered with the local Workspace or the default Agent.
type WorkspaceResolver = dyn Fn(&str) -> Result<String, RunError> + Send + Sync;
type ProjectedAgentResolver = dyn Fn(&str) -> Result<Option<String>, RunError> + Send + Sync;

#[derive(Clone)]
pub struct SessionRunApplication {
    runtime: Arc<dyn RunApplication>,
    admissions: Arc<SessionRunAdmissions>,
    coordination: Arc<crate::SessionCoordination>,
    workspace: Arc<WorkspaceResolver>,
    projected_agent: Arc<ProjectedAgentResolver>,
    first_run_creates: bool,
}

impl SessionRunApplication {
    #[must_use]
    pub fn new(
        runtime: Arc<dyn RunApplication>,
        admissions: Arc<SessionRunAdmissions>,
        coordination: Arc<crate::SessionCoordination>,
        workspace: impl Fn(&str) -> Result<String, RunError> + Send + Sync + 'static,
        projected_agent: impl Fn(&str) -> Result<Option<String>, RunError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            runtime,
            admissions,
            coordination,
            workspace: Arc::new(workspace),
            projected_agent: Arc::new(projected_agent),
            first_run_creates: false,
        }
    }

    /// Let an owner-scoped first Run create its default Session. Production
    /// never enables this: an application grant binds only existing Sessions.
    /// An unguarded test mount enables it to stand in for that grant.
    #[must_use]
    pub fn with_first_run_creation(mut self) -> Self {
        self.first_run_creates = true;
        self
    }

    fn agent_for(&self, thread: &str, requested_agent: Option<&str>) -> Result<String, RunError> {
        if let Some(requested) = requested_agent {
            return Ok(requested.to_string());
        }
        Ok((self.projected_agent)(thread)?.unwrap_or_else(|| "assistant".to_string()))
    }

    fn admission_command(
        &self,
        operation_id: &str,
        thread: &str,
        requested_agent: Option<&str>,
        messages: Vec<Message>,
        traceparent: Option<String>,
        replacement: awaken_session_types::SessionRunReplacement,
    ) -> Result<AdmitSessionRun, RunApplicationError> {
        if operation_id.trim().is_empty() {
            return Err(RunError::bad_request(
                "Session Run requires a stable operation identity",
            ));
        }
        if messages.is_empty() {
            return Err(RunError::bad_request(
                "Session Run requires at least one new input message",
            ));
        }
        Ok(AdmitSessionRun {
            session_id: awaken_session_types::SessionId::from(thread),
            agent_id: self.agent_for(thread, requested_agent)?,
            operation_id: operation_id.to_string(),
            run_id: awaken_session_execution_contract::session_run_id(
                &awaken_session_types::SessionId::from(thread),
                operation_id,
            ),
            messages,
            data_subject_id: None,
            traceparent,
            execution_requirements: Default::default(),
            replacement,
        })
    }

    /// Run one Session Run. `create` is the single-tenant first-Run path,
    /// which creates the default Session under the Workspace this crate
    /// derived itself; every other path decides ownership once, here, and an
    /// unknown or foreign thread is the same `NotFound`.
    async fn run_admitted(
        &self,
        owner_scope: &str,
        command: AdmitSessionRun,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
        create: bool,
    ) -> Result<StepOutcome, RunApplicationError> {
        if !(create || self.first_run_creates) {
            let access = self
                .authorize(owner_scope, command.session_id.as_str())
                .await?;
            return Box::pin(
                self.admissions
                    .run_admitted_session_for_owner(&access, command, sink),
            )
            .await;
        }
        let input_message_ids = command.input_message_ids();
        let admitted = Box::pin(
            self.admissions
                .admit_or_create_session_run(owner_scope, command),
        )
        .await?;
        Box::pin(
            self.admissions
                .coordination
                .activate_and_observe_session_run(admitted, input_message_ids, sink),
        )
        .await
    }

    async fn authorize(
        &self,
        owner_scope: &str,
        thread: &str,
    ) -> Result<crate::SessionAccess, RunError> {
        Ok(self
            .admissions
            .queries
            .authorize(owner_scope, &awaken_session_types::SessionId::from(thread))
            .await?)
    }
}

#[async_trait::async_trait]
impl RunApplication for SessionRunApplication {
    async fn run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let command = self.admission_command(
            operation_id,
            thread,
            agent.as_deref(),
            messages,
            None,
            awaken_session_types::SessionRunReplacement::PreservePrior,
        )?;
        self.run_admitted(&(self.workspace)(thread)?, command, None, true)
            .await
    }

    async fn run_streaming(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.run_streaming_for_owner(
            &(self.workspace)(thread)?,
            operation_id,
            thread,
            agent,
            messages,
            sink,
        )
        .await
    }

    async fn resume(
        &self,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.resume_for_owner(
            &(self.workspace)(thread)?,
            operation_id,
            thread,
            tool_use_id,
            resume,
        )
        .await
    }

    async fn resume_streaming(
        &self,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.resume_streaming_for_owner(
            &(self.workspace)(thread)?,
            operation_id,
            thread,
            tool_use_id,
            resume,
            sink,
        )
        .await
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunApplicationError> {
        self.runtime.interrupt(thread).await
    }

    async fn pending(&self, thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        self.runtime.pending(thread).await
    }

    async fn history(&self, thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        self.runtime.history(thread).await
    }

    fn model(&self) -> String {
        self.runtime.model()
    }

    async fn usage(&self, thread: &str) -> Result<(u64, u64), RunApplicationError> {
        self.runtime.usage(thread).await
    }
}

#[async_trait::async_trait]
impl OwnerScopedRunApplication for SessionRunApplication {
    async fn run_streaming_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let command = self.admission_command(
            operation_id,
            thread,
            agent.as_deref(),
            messages,
            None,
            awaken_session_types::SessionRunReplacement::PreservePrior,
        )?;
        self.run_admitted(owner_scope, command, Some(sink), false)
            .await
    }

    async fn resume_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        let access = self.authorize(owner_scope, thread).await?;
        self.coordination
            .resume_owned_session_run(&access, operation_id, tool_use_id, resume)
            .await
    }
    async fn resume_streaming_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let access = self.authorize(owner_scope, thread).await?;
        self.coordination
            .resume_owned_session_run_streaming(&access, operation_id, tool_use_id, resume, sink)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionRunReplacementApplication
    for SessionRunApplication
{
    async fn supersede_session_run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let command = self.admission_command(
            operation_id,
            thread,
            agent.as_deref(),
            messages,
            None,
            awaken_session_types::SessionRunReplacement::SupersedePrior,
        )?;
        self.run_admitted(&(self.workspace)(thread)?, command, None, true)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionRunBackgroundApplication
    for SessionRunApplication
{
    async fn submit_session_run_background(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        traceparent: Option<String>,
    ) -> Result<awaken_agent_contract::agent::run::Id, RunApplicationError> {
        let command = self.admission_command(
            operation_id,
            thread,
            agent.as_deref(),
            messages,
            traceparent,
            awaken_session_types::SessionRunReplacement::PreservePrior,
        )?;
        let run_id = command.run_id.clone();
        let owner_scope = (self.workspace)(thread)?;
        let admitted = Box::pin(
            self.admissions
                .admit_or_create_session_run(&owner_scope, command),
        )
        .await?;
        self.admissions
            .activate_admitted_session_run(admitted)
            .await?;
        Ok(run_id)
    }
}

#[cfg(test)]
mod preparation_presentation_tests {
    use super::*;

    #[test]
    fn profiled_creation_retains_each_original_preparation_presentation() {
        // F4 presentation table: every original preparation variant crosses
        // the existing Profiled projector, then existing From<RunError>.
        // Requested creation instead keeps Preparation. This is deliberately
        // independent of current pin-only errors (all Rejected), so future
        // pin variants cannot silently flatten NotFound/Conflict/outage.
        for (input, expected) in [
            (
                SessionPreparationError::NotFound,
                RunError::bad_request("Session was not found"),
            ),
            (
                SessionPreparationError::Conflict,
                RunError::unavailable("Session changed while it was being admitted"),
            ),
            (
                SessionPreparationError::Unavailable("original outage".into()),
                RunError::unavailable("original outage"),
            ),
            (
                SessionPreparationError::Rejected(RunError::classified(
                    "original-code",
                    "original rejection",
                )),
                RunError::classified("original-code", "original rejection"),
            ),
        ] {
            let actual = SessionCreationError::from(preparation_error(input));
            let SessionCreationError::Rejected(actual) = actual else {
                panic!("Profiled presentation changed")
            };
            assert_eq!(
                (actual.kind, actual.code, actual.message),
                (expected.kind, expected.code, expected.message)
            );
        }
    }
}
