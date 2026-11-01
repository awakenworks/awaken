// @formal entrypoint session.agent_message_completed_target_allows_late_source_receipt_without_reexecution
// @formal entrypoint session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters
// @formal entrypoint session.agent_message_missing_lifecycle_projection_relinquishes_without_settlement
// @formal entrypoint session.agent_message_requires_exact_committed_tool_source_and_payload
// @formal entrypoint session.agent_message_target_boundary_not_source_receipt_settles_activity
// @formal entrypoint session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
// @formal entrypoint session_execution.initial_event_path_reaches_idle_only_after_full_settlement
//! Session-owned admission for asynchronous Agent coordination.
//!
//! This module adds no child registry. It resolves the roster already frozen by
//! the Session baseline and delegates ordinary Thread/Run persistence to the
//! existing [`awaken_session_execution_contract::SessionRuns`] port.

mod task_attention;

use crate::SessionRootReads as _;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_session_application_contract::{Pending, RunResume, StepOutcome};
use awaken_session_contract::{
    SessionThreadTarget, SessionThreadToolReply, SessionThreadToolReplyCommand,
    SessionThreadToolReplyDelivery,
};
use awaken_session_execution_contract::{
    CoordinatedRunIntent, SessionAgentBoundaryAction, SessionAgentBoundaryCommand,
    SessionAgentBoundaryRole, SessionAgentBoundaryVisibility, SessionAgentCommittedReport,
    SessionAgentCoordination, SessionAgentMessageCommand, SessionAgentMessageReceipt,
    SessionAgentReportContinuation, SessionAgentRosterEntry, SessionAgentTarget,
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode, coordinated_thread_closed,
    session_agent_boundary_action, session_agent_report_text, session_agent_run_boundary,
    session_run_activity_operation_id,
};
use awaken_session_types::RunError;

use std::sync::Arc;

use crate::SessionProjectionRecoveryError;

struct ResolvedRosterMember {
    entry: SessionAgentRosterEntry,
    snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
}

// Anthropic's public 25-Thread ceiling includes the primary Thread. Ordinary
// coordinated children therefore own the remaining 24 slots; Advisor
// consultations are explicitly capacity-exempt at the dispatch boundary.
const MAX_UNARCHIVED_AGENT_CHILD_THREADS: usize = 24;

async fn admit_fresh_agent_report(
    app: &SessionCoordination,
    owner: &str,
    settled: awaken_session_contract::PersistedSession,
    observation: awaken_session_contract::SessionRuntimeIntervalObservation,
    continuation: SessionAgentReportContinuation,
) -> Result<(), RunError> {
    // Only confirmed Missing with the original active membership reaches
    // the existing fresh order: usage, interval, projection, admission.
    // A dependency failure remains pending; it does not settle the epoch.
    let settled = app
        .reconcile_coordination_boundary_usage(settled, &continuation.source_thread_id)
        .await?;
    app.lifecycle
        .observe_activity_runtime_boundary(&continuation.session_id, observation)
        .await
        .map_err(crate::SessionActivityError::run_error)?;
    app.realizations
        .install_dispatch_projection(owner, &settled)
        .await
        .map_err(|error| {
            RunError::unavailable(format!(
                "Agent report Resource projection is unavailable: {error}"
            ))
        })?;
    let session_id = continuation.session_id.clone();
    let epoch = continuation.session_activity_epoch;
    let continued = app
        .coordination
        .continue_session_agent_report(continuation)
        .await;
    app.settle_definitive_coordination_rejection(session_id.as_str(), epoch, &continued)
        .await?;
    continued
}

/// Asynchronous Agent coordination within one Session: the frozen Agent
/// roster, messages between coordinated Threads and their boundary reports,
/// tool replies, Thread interrupts and the resumption of a paused Run, each
/// admitted against the durable Session root and its activity fence.
///
/// It holds the CAS kernel, the lifecycle signal, the read, activity, Run
/// admission, realization and observation components it passes through, and
/// the coordination and Thread runtime roles.
pub struct SessionCoordination {
    mutations: Arc<crate::SessionMutations>,
    lifecycle: Arc<crate::SessionLifecycle>,
    queries: Arc<crate::SessionQueries>,
    activities: Arc<crate::SessionActivities>,
    run_admissions: Arc<crate::SessionRunAdmissions>,
    realizations: Arc<crate::SessionRealizations>,
    observations: Arc<crate::SessionObservations>,
    coordination: Arc<dyn awaken_session_execution_contract::ChildRunCoordination>,
    coordination_reads: Arc<dyn awaken_session_execution_contract::SessionCoordinationReads>,
    thread_reads: Arc<dyn awaken_session_execution_contract::ThreadReads>,
}

/// The collaborators one [`SessionCoordination`] is built from.
pub(crate) struct SessionCoordinationCollaborators {
    pub(crate) mutations: Arc<crate::SessionMutations>,
    pub(crate) lifecycle: Arc<crate::SessionLifecycle>,
    pub(crate) queries: Arc<crate::SessionQueries>,
    pub(crate) activities: Arc<crate::SessionActivities>,
    pub(crate) run_admissions: Arc<crate::SessionRunAdmissions>,
    pub(crate) realizations: Arc<crate::SessionRealizations>,
    pub(crate) observations: Arc<crate::SessionObservations>,
    pub(crate) runtime: crate::SessionExecutor,
}

impl SessionCoordination {
    pub(crate) fn new(collaborators: SessionCoordinationCollaborators) -> Self {
        let SessionCoordinationCollaborators {
            mutations,
            lifecycle,
            queries,
            activities,
            run_admissions,
            realizations,
            observations,
            runtime,
        } = collaborators;
        Self {
            mutations,
            lifecycle,
            queries,
            activities,
            run_admissions,
            realizations,
            observations,
            coordination: runtime.coordination.clone(),
            coordination_reads: runtime.coordination_reads.clone(),
            thread_reads: runtime.thread_reads.clone(),
        }
    }

    /// Open the activity of a Run admitted for this Agent. An exact committed
    /// receipt is recovery truth and is re-read before fresh admission policy,
    /// so a response-loss repair recovers its original epoch even after the
    /// Session reached a budget or terminal state.
    pub(crate) async fn begin_admitted_activity_for_operation(
        &self,
        agent_id: &str,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        source: (&ThreadId, &RunId),
    ) -> Result<(awaken_session_contract::PersistedSession, u64), RunError> {
        if operation_id.trim().is_empty() {
            return Err(RunError::internal(
                "Session activity operation identity is required",
            ));
        }
        let activity_error =
            |error| crate::SessionActivities::activity_operation_error(operation_id, error);
        if let Some(committed) = self
            .activities
            .recover_activity_for_operation(session_id, operation_id, Some(source))
            .await
            .map_err(activity_error)?
        {
            return Ok(committed);
        }
        let owner_scope = self
            .mutations
            .owner(session_id)
            .await
            .map_err(crate::SessionActivityError::mutation)
            .map_err(activity_error)?;
        self.run_admissions
            .admit_run_session(&owner_scope, session_id.as_str(), agent_id)
            .await?;
        self.activities
            .begin_activity_for_operation_with_policy(
                session_id,
                operation_id,
                false,
                None,
                None,
                Some(source),
            )
            .await
            .map_err(activity_error)
    }

    async fn recover_coordinated_admission(
        &self,
        session: &awaken_session_contract::PersistedSession,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<Option<SessionAgentMessageReceipt>, RunError> {
        let (operation, thread, run) = command.activity_source();
        let epoch = command.activity_epoch();
        awaken_session_contract::validate_complete_run_activity(
            session, operation, epoch, thread, run,
        )
        .map_err(|error| RunError::unavailable(error.to_string()))?;
        let receipt = self
            .coordination_reads
            .coordinated_admission_receipt(command)
            .await
            .map_err(|error| RunError::unavailable(error.message))?;
        match receipt {
            Some(receipt) if receipt.thread_id == command.thread_id() => Ok(Some(receipt)),
            None if epoch.is_none_or(|epoch| session.active_activity_epochs.contains(&epoch)) => {
                Ok(None)
            }
            _ => Err(RunError::unavailable(
                "original coordinated admission cannot be proven",
            )),
        }
    }

    async fn settle_definitive_coordination_rejection<T>(
        &self,
        session_id: &str,
        activity_epoch: u64,
        result: &Result<T, RunError>,
    ) -> Result<(), RunError> {
        if result
            .as_ref()
            .is_err_and(|error| error.kind == awaken_session_types::RunErrorKind::BadRequest)
        {
            // A caller rejection proves the Runtime did not durably accept the
            // operation. Dependency/internal failures deliberately retain the
            // activity receipt: enqueue/delivery may have committed before its
            // response was lost, and exact retry must reuse the same epoch.
            self.lifecycle
                .settle_activity_observed(
                    &awaken_session_types::SessionId::from(session_id),
                    activity_epoch,
                    None,
                )
                .await
                .map_err(crate::SessionActivityError::run_error)?;
        }
        Ok(())
    }

    async fn reconcile_coordination_boundary_usage(
        &self,
        session: awaken_session_contract::PersistedSession,
        source_thread_id: &ThreadId,
    ) -> Result<awaken_session_contract::PersistedSession, RunError> {
        if session.is_terminal() {
            return Ok(session);
        }
        let usage = self
            .observations
            .session_usage_for_model_request(session.session_id.as_str(), &source_thread_id.0)
            .await?;
        self.lifecycle
            .reconcile_managed_budget_usage(&session.session_id, usage)
            .await
            .map(|outcome| outcome.session)
            .map_err(|error| {
                RunError::unavailable(format!("Session budget settlement is unavailable: {error}"))
            })
    }

    async fn committed_agent_boundary(
        &self,
        command: &SessionAgentBoundaryCommand,
    ) -> Result<
        (
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
            RunState,
            Option<awaken_session_contract::SessionRuntimeIntervalObservation>,
        ),
        RunError,
    > {
        let thread_reads = self.thread_reads.as_ref();

        let snapshot = thread_reads
            .session_thread_run_recovery_snapshot(
                &command.session_id,
                &command.source_thread_id.0,
                &command.source_run_id,
            )
            .await?
            .ok_or_else(|| RunError::bad_request("settled Agent Run was not committed"))?;
        let state = snapshot
            .run_record(&command.source_run_id)
            .map_err(|error| RunError::internal(error.to_string()))?
            .map(|run| run.state.clone())
            .ok_or_else(|| RunError::bad_request("settled Agent Run is absent from its Thread"))?;
        if session_agent_run_boundary(&state).is_none() {
            return Err(RunError::bad_request(
                "Agent settlement does not identify a committed child boundary",
            ));
        }
        let observation =
            awaken_session_contract::SessionRuntimeIntervalObservation::from_committed_boundary(
                command.session_activity_epoch,
                &command.source_thread_id,
                &command.source_run_id,
                &state,
                &snapshot,
            )
            .map_err(|error| RunError::internal(error.to_string()))?;
        Ok((snapshot, state, observation))
    }

    async fn validate_coordinated_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &ThreadId,
    ) -> Result<(), RunError> {
        let coordination_reads = self.coordination_reads.as_ref();

        if child_thread_id.0 == session_id {
            return Err(RunError::bad_request(
                "a coordinated child Thread must differ from its primary Session",
            ));
        }
        let links = coordination_reads
            .coordinated_threads(session_id, None)
            .await?;
        if !links
            .iter()
            .any(|link| link.session_id == session_id && link.thread_id == *child_thread_id)
        {
            return Err(RunError::bad_request(
                "Agent Thread was not found in this Session",
            ));
        }
        Ok(())
    }

    fn coordination_root_profile(
        &self,
        owner_scope: &str,
        session: &awaken_session_contract::PersistedSession,
    ) -> Result<awaken_executable_agent_contract::ExecutableAgentSessionProfile, RunError> {
        let baseline = session
            .frozen_baseline()
            .ok_or_else(|| RunError::unavailable("Session creation has not completed"))?;
        self.queries
            .selected_session_profile(owner_scope, &baseline.agent_id, baseline.agent_revision)
            .ok_or_else(|| {
                RunError::unavailable(format!(
                    "the Session's frozen coordinator Agent `{}` is unavailable",
                    baseline.agent_id
                ))
            })
    }

    fn coordination_roster(
        &self,
        owner_scope: &str,
        session: &awaken_session_contract::PersistedSession,
    ) -> Result<Vec<ResolvedRosterMember>, RunError> {
        let baseline = session
            .frozen_baseline()
            .ok_or_else(|| RunError::unavailable("Session creation has not completed"))?;
        let root = self.coordination_root_profile(owner_scope, session)?;
        if !self.queries.has_agent_profile_source() {
            return Err(RunError::unavailable(
                "Agent publication source is unavailable",
            ));
        }
        let mut roster = Vec::with_capacity(baseline.delegate_ids.len());
        for agent_id in &baseline.delegate_ids {
            let revision = if agent_id == &baseline.agent_id {
                baseline
                    .agent_revision
                    .or_else(|| (root.source_revision > 0).then_some(root.source_revision))
            } else {
                root.delegates
                    .iter()
                    .find(|delegate| delegate.agent_id == *agent_id)
                    .and_then(|delegate| delegate.source_revision)
            }
            .ok_or_else(|| {
                RunError::unavailable(format!(
                    "the frozen roster has no exact revision for Agent `{agent_id}`"
                ))
            })?;
            let profile = self
                .queries
                .selected_session_profile(owner_scope, agent_id, Some(revision))
                .ok_or_else(|| {
                    RunError::unavailable(format!(
                        "roster Agent `{agent_id}` revision {revision} is unavailable"
                    ))
                })?;
            let snapshot = self
                .queries
                .executable_snapshot_at_revision(owner_scope, agent_id, revision)
                .ok_or_else(|| {
                    RunError::unavailable(format!(
                        "roster Agent `{agent_id}` executable revision {revision} is unavailable"
                    ))
                })?;
            if snapshot.root_agent_id.0 != *agent_id {
                return Err(RunError::internal(format!(
                    "roster Agent `{agent_id}` executable identity does not match its profile"
                )));
            }
            roster.push(ResolvedRosterMember {
                entry: SessionAgentRosterEntry {
                    agent_id: agent_id.clone(),
                    name: profile.name.unwrap_or_else(|| agent_id.clone()),
                    description: profile.description,
                },
                snapshot,
            });
        }
        Ok(roster)
    }

    async fn coordination_session(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(String, awaken_session_contract::PersistedSession), RunError> {
        let session = self
            .queries
            .session(session_id)
            .await
            .map_err(crate::session_repository_error)?;
        if session.is_terminal() || session.terminal_cleanup.is_fenced() {
            return Err(RunError::bad_request(
                "Session no longer accepts Agent coordination",
            ));
        }
        if !session.execution.admits_activity() {
            return Err(RunError::unavailable_classified(
                "session_not_ready",
                "Session realization has not completed",
            ));
        }
        let owner = self
            .mutations
            .owner(session_id)
            .await
            .map_err(crate::run_admission::mutation_error)?;
        Ok((owner, session))
    }
}

impl SessionCoordination {
    async fn prepare_session_thread_tool_reply(
        &self,
        command: SessionThreadToolReplyCommand,
    ) -> Result<(String, Option<u64>, SessionThreadToolReplyDelivery), RunError> {
        let coordination_reads = self.coordination_reads.as_ref();
        let thread_reads = self.thread_reads.as_ref();

        if !command.has_complete_coordinates() {
            return Err(RunError::bad_request(
                "Session Thread reply requires exact Run, correlation, and tool identities",
            ));
        }
        let fence = coordination_reads
            .session_thread_tool_reply_fence(&command)
            .await?;
        if !fence.already_applied
            && let SessionThreadTarget::Child(child_thread_id) = &command.target
        {
            self.validate_coordinated_thread(&command.session_id, child_thread_id)
                .await?;
            if thread_reads
                .session_thread_disposition(&command.session_id, &child_thread_id.0)
                .await?
                == awaken_agent_contract::ThreadDisposition::Archived
            {
                return Err(RunError::bad_request("Agent Thread is archived"));
            }
        }
        let operation_id = command.activity_operation_id();
        let session_id = command.session_id.clone();
        let session_activity_epoch = if fence.already_applied {
            None
        } else {
            match fence.activity {
                awaken_session_contract::SessionThreadToolReplyActivity::ThreadExtension => None,
                awaken_session_contract::SessionThreadToolReplyActivity::Session {
                    prior_epoch,
                } => {
                    let (_, epoch) = self
                        .activities
                        .transfer_committed_activity_for_operation(
                            &session_id,
                            &operation_id,
                            prior_epoch,
                            (
                                &command.target.thread_id(&command.session_id),
                                &command.expected_run_id,
                            ),
                        )
                        .await
                        .map_err(crate::SessionActivityError::run_error)?;
                    Some(epoch)
                }
            }
        };
        Ok((
            String::from(session_id),
            session_activity_epoch,
            SessionThreadToolReplyDelivery {
                command,
                fence,
                session_activity_epoch,
            },
        ))
    }

    /// Resume the primary Thread through the same exact-ticket, activity-transfer,
    /// durable Inbox path used by Managed Event replies, then observe the next
    /// committed Step. This replaces the former inline `RunApplication::resume`
    /// execution path without introducing another resume state machine.
    pub async fn resume_session_run(
        &self,
        operation_id: &str,
        session_id: &awaken_session_types::SessionId,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        self.resume_session_run_observed(operation_id, session_id, tool_use_id, resume, None)
            .await
    }

    /// Resume a public-protocol Session Run on a Session its caller owns.
    /// Ownership was decided when `access` was minted; exact operation replay
    /// therefore cannot bypass it.
    pub async fn resume_owned_session_run(
        &self,
        access: &crate::SessionAccess,
        operation_id: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        self.resume_session_run_observed(
            operation_id,
            access.session_id(),
            tool_use_id,
            resume,
            None,
        )
        .await
    }

    /// Reuse the exact authorized reply state machine, changing observation only.
    pub async fn resume_owned_session_run_streaming(
        &self,
        access: &crate::SessionAccess,
        operation_id: &str,
        tool_use_id: &str,
        resume: RunResume,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.resume_session_run_observed(
            operation_id,
            access.session_id(),
            tool_use_id,
            resume,
            Some(sink),
        )
        .await
    }

    async fn resume_session_run_observed(
        &self,
        operation_id: &str,
        session_id: &awaken_session_types::SessionId,
        tool_use_id: &str,
        resume: RunResume,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let coordination_runtime = self.coordination.as_ref();
        let thread_reads = self.thread_reads.as_ref();

        if operation_id.trim().is_empty() {
            return Err(RunError::bad_request(
                "Session Run resume requires a stable operation identity",
            ));
        }
        let snapshot = thread_reads
            .session_thread_recovery_snapshot(session_id, session_id.as_str())
            .await?
            .ok_or_else(|| RunError::bad_request("Session Thread has no committed Run"))?;
        let current = snapshot
            .current_run()
            .map_err(|error| RunError::internal(error.to_string()))?
            .ok_or_else(|| RunError::bad_request("Session Thread has no committed Run"))?;
        let run_id = current.id.clone();
        if current.thread_id != snapshot.thread_id || current.state != RunState::Awaiting {
            return Err(RunError::bad_request(
                "Session Thread is not awaiting a reply",
            ));
        }
        let ticket = snapshot
            .answerable_ticket(&run_id)
            .map_err(|error| RunError::internal(error.to_string()))?
            .filter(|ticket| ticket.call_id() == Some(tool_use_id))
            .ok_or_else(|| RunError::bad_request("tool_use_id does not name the active ticket"))?;
        let pending = Pending::from_resume_ticket(ticket)
            .ok_or_else(|| RunError::bad_request("active ticket is not externally answerable"))?;
        let reply = match resume {
            RunResume::Permission(decision) if !pending.client_executed => {
                SessionThreadToolReply::Confirm(decision)
            }
            RunResume::ClientResult { content, is_error } if pending.client_executed => {
                SessionThreadToolReply::Result { content, is_error }
            }
            RunResume::Permission(_) => {
                return Err(RunError::bad_request(
                    "client-executed tool requires a client result",
                ));
            }
            RunResume::ClientResult { .. } => {
                return Err(RunError::bad_request(
                    "permission-gated tool requires a permission decision",
                ));
            }
        };
        let command = SessionThreadToolReplyCommand {
            session_id: session_id.clone(),
            tool_request_event_id: Some(operation_id.to_string()),
            expected_thread_version: Some(snapshot.thread_version),
            target: SessionThreadTarget::Primary,
            expected_run_id: run_id,
            expected_correlation_id: ticket.correlation_id.clone(),
            tool_use_id: tool_use_id.to_string(),
            reply,
            accompanying_system: None,
        };
        let (session_id, session_activity_epoch, delivery) =
            self.prepare_session_thread_tool_reply(command).await?;
        let result = coordination_runtime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await;
        if let Some(epoch) = session_activity_epoch {
            self.settle_definitive_coordination_rejection(&session_id, epoch, &result)
                .await?;
        }
        result
    }
}

fn activity_admission_result(
    admitted: Result<Option<u64>, RunError>,
) -> Result<SessionRunActivityAdmission, RunError> {
    match admitted {
        Ok(None) => Ok(SessionRunActivityAdmission::Rejected),
        Ok(Some(session_activity_epoch)) => Ok(SessionRunActivityAdmission::Admitted {
            session_activity_epoch,
        }),
        Err(error)
            if error.kind == awaken_session_types::RunErrorKind::BadRequest
                || error.code == "budget_reached" =>
        {
            Ok(SessionRunActivityAdmission::Rejected)
        }
        Err(error) => Err(error),
    }
}

#[async_trait::async_trait]
impl SessionAgentCoordination for SessionCoordination {
    async fn prepare_prepared_session_run_activity(
        &self,
        session_id: &awaken_session_types::SessionId,
        agent_id: &str,
        run_id: &RunId,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, RunError> {
        let operation = session_run_activity_operation_id(session_id, run_id);
        let thread = ThreadId(session_id.to_string());
        match self
            .activities
            .recover_activity_for_operation(session_id, &operation, Some((&thread, run_id)))
            .await
        {
            Ok(Some(_)) | Err(crate::SessionActivityError::BudgetReached) => return Ok(None),
            Ok(None) => {}
            Err(error) => {
                return Err(crate::SessionActivities::activity_operation_error(
                    &operation, error,
                ));
            }
        }
        if mode == SessionRunActivityAdmissionMode::RecoverOnly {
            return Ok(None);
        }
        let owner = self
            .mutations
            .owner(session_id)
            .await
            .map_err(crate::SessionActivityError::mutation)
            .map_err(|error| {
                crate::SessionActivities::activity_operation_error(&operation, error)
            })?;
        let revision = self
            .run_admissions
            .admit_run_session(&owner, session_id.as_str(), agent_id)
            .await?;
        self.activities
            .prepare_run_activity_revision(session_id, revision)
            .await
            .map(Some)
            .map_err(|error| crate::SessionActivities::activity_operation_error(&operation, error))
    }

    async fn admit_prepared_session_run_activity(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &RunId,
        mode: SessionRunActivityAdmissionMode,
        preparation: awaken_session_execution_contract::PreparedSessionRunActivity,
    ) -> Result<SessionRunActivityAdmission, RunError> {
        let operation = session_run_activity_operation_id(session_id, run_id);
        let thread = ThreadId(session_id.to_string());
        let admitted = async {
            if let Some((_, epoch)) = self
                .activities
                .recover_activity_for_operation(session_id, &operation, Some((&thread, run_id)))
                .await
                .map_err(|error| {
                    crate::SessionActivities::activity_operation_error(&operation, error)
                })?
            {
                return Ok(Some(epoch));
            }
            if mode == SessionRunActivityAdmissionMode::RecoverOnly {
                return Ok(None);
            }
            let revision = preparation.projected_revision.ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_run_admission_pending",
                    "prepared activity has no current execution revision or receipt",
                )
            })?;
            self.activities
                .commit_prepared_run_activity(
                    session_id,
                    &operation,
                    revision,
                    (&thread, run_id),
                    Some(preparation.admitted_after_cursor),
                )
                .await
                .map(Some)
        }
        .await;
        activity_admission_result(admitted)
    }

    async fn admit_session_run_activity(
        &self,
        session_id: &awaken_session_types::SessionId,
        agent_id: &str,
        run_id: &RunId,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, RunError> {
        let operation_id = session_run_activity_operation_id(session_id, run_id);
        let thread_id = ThreadId(session_id.to_string());
        let admitted = match mode {
            SessionRunActivityAdmissionMode::RecoverOrAdmit => self
                .begin_admitted_activity_for_operation(
                    agent_id,
                    session_id,
                    &operation_id,
                    (&thread_id, run_id),
                )
                .await
                .map(Some),
            SessionRunActivityAdmissionMode::RecoverOnly => self
                .activities
                .recover_activity_for_operation(
                    session_id,
                    &operation_id,
                    Some((&thread_id, run_id)),
                )
                .await
                .map_err(|error| {
                    crate::SessionActivities::activity_operation_error(&operation_id, error)
                }),
        };
        activity_admission_result(admitted.map(|value| value.map(|(_, epoch)| epoch)))
    }

    async fn admit_session_model_request(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<bool, RunError> {
        let thread_reads = self.thread_reads.as_ref();

        let session = self
            .queries
            .session(session_id)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        if session.is_terminal() {
            return Err(RunError::bad_request(
                "a terminal Session cannot admit model requests",
            ));
        }
        let snapshot = thread_reads
            .session_thread_recovery_snapshot(session_id, &thread_id.0)
            .await?
            .ok_or_else(|| {
                RunError::bad_request(
                    "model-request admission does not identify a committed Session Thread",
                )
            })?;
        if snapshot.latest_run_id.as_ref() != Some(run_id)
            || !snapshot
                .run_record(run_id)
                .map_err(|error| RunError::internal(error.to_string()))?
                .is_some_and(|run| matches!(run.state, RunState::Running | RunState::Awaiting))
        {
            return Err(RunError::bad_request(
                "model-request admission does not identify the latest live Run",
            ));
        }
        // Runtime commits its input before entering this gate. Wake the sole
        // recovery owner while inference is still running, rather than waiting
        // for the completion observer or its scheduled recovery scan. An
        // already anchored input needs no further publication wakeups here.
        if thread_id.0 == session_id.as_str()
            && session.event_batches.iter().any(|batch| {
                batch.events.iter().any(|entry| {
                    !entry.processed
                        && entry.projection_anchor.is_none()
                        && matches!(
                            &entry.event,
                            awaken_session_contract::SessionEventCommand::UserMessage {
                                run_id: pending_run, ..
                            } if pending_run == run_id
                        )
                })
            })
        {
            self.lifecycle.request_reconciliation();
        }
        let usage = self
            .observations
            .session_usage_for_model_request(session_id.as_str(), &thread_id.0)
            .await?;
        self.lifecycle
            .reconcile_managed_budget_usage(session_id, usage)
            .await
            .map(|outcome| outcome.session.budget.can_admit_model_request())
            .map_err(|error| {
                RunError::unavailable(format!("Session budget admission is unavailable: {error}"))
            })
    }

    async fn list_session_agents(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<Vec<SessionAgentRosterEntry>, RunError> {
        let (owner, session) = self.coordination_session(session_id).await?;
        self.coordination_roster(&owner, &session)
            .map(|members| members.into_iter().map(|member| member.entry).collect())
    }

    async fn send_session_agent_message(
        &self,
        command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, RunError> {
        let coordination_runtime = self.coordination.as_ref();

        let coordination_reads = self.coordination_reads.as_ref();
        let thread_reads = self.thread_reads.as_ref();

        command.validate_coordinates()?;
        let command = coordination_reads
            .validate_session_agent_message_source(&command)
            .await?;
        let (owner, session) = self.coordination_session(&command.session_id).await?;
        let roster = self.coordination_roster(&owner, &session)?;
        let existing_links = coordination_reads
            .coordinated_threads(&command.session_id, None)
            .await?;

        let (thread_id, member, intent) = match &command.target {
            SessionAgentTarget::Spawn { agent_id } => {
                let member = roster
                    .into_iter()
                    .find(|member| member.entry.agent_id == *agent_id)
                    .ok_or_else(|| {
                        RunError::bad_request(format!(
                            "Agent `{agent_id}` is not in this Session's frozen roster"
                        ))
                    })?;
                (
                    awaken_session_execution_contract::coordinated_thread_id(
                        &command.session_id,
                        &command.source_run_id,
                        &command.operation_id,
                    ),
                    member,
                    CoordinatedRunIntent::Spawn,
                )
            }
            SessionAgentTarget::ExistingThread { thread_id } => {
                if thread_reads
                    .session_thread_disposition(&command.session_id, &thread_id.0)
                    .await?
                    == awaken_agent_contract::ThreadDisposition::Archived
                {
                    return Err(RunError::bad_request("Agent Thread is archived"));
                }
                let link = existing_links
                    .iter()
                    .find(|link| {
                        link.session_id == command.session_id && link.thread_id == *thread_id
                    })
                    .ok_or_else(|| {
                        RunError::bad_request("Agent Thread was not found in this Session")
                    })?;
                let member = roster
                    .into_iter()
                    .find(|member| {
                        link.target
                            .agent_id()
                            .is_some_and(|agent_id| member.entry.agent_id == agent_id)
                    })
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "only ordinary Agent Threads accept send_message follow-ups",
                        )
                    })?;
                if let Some(snapshot) = thread_reads
                    .session_thread_recovery_snapshot(&command.session_id, &thread_id.0)
                    .await?
                {
                    let latest = snapshot
                        .current_run()
                        .map_err(|error| RunError::internal(error.to_string()))?
                        .ok_or_else(|| {
                            RunError::internal(
                                "coordinated Thread recovery omitted its latest committed Run",
                            )
                        })?;
                    if coordinated_thread_closed(link.target.kind(), &latest.state) {
                        return Err(RunError::bad_request(
                            "Agent Thread terminated after a failed Run",
                        ));
                    }
                }
                (thread_id.clone(), member, CoordinatedRunIntent::FollowUp)
            }
        };

        let mut admission = command.into_coordinated_run(
            intent,
            thread_id.clone(),
            member.snapshot,
            None,
            MAX_UNARCHIVED_AGENT_CHILD_THREADS,
        );
        let replayed = self
            .activities
            .recover_activity_for_operation(
                &admission.session_id,
                &admission.operation_id,
                Some((&thread_id, &admission.run_id)),
            )
            .await
            .map_err(crate::SessionActivityError::run_error)?;
        let original = replayed.as_ref().map_or(&session, |(session, _)| session);
        admission.session_activity_epoch = replayed.as_ref().map(|(_, epoch)| *epoch);
        if let Some(receipt) = self
            .recover_coordinated_admission(
                original,
                awaken_session_execution_contract::CoordinatedAdmissionRef::Child(&admission),
            )
            .await?
        {
            return Ok(receipt);
        }
        // A new child is authored after the committed source call, so its
        // Resource envelope must use the Session's current desired generation.
        // Reuse complete dispatch projection; this stages facts only. The
        // claimed Worker remains the sole physical realization owner.
        self.realizations
            .install_dispatch_projection(&owner, &session)
            .await
            .map_err(|error| {
                RunError::unavailable(format!(
                    "Agent coordination Resource projection is unavailable: {error}"
                ))
            })?;
        let (_, activity_epoch) = self
            .activities
            .begin_activity_for_operation_with_policy(
                &admission.session_id,
                &admission.operation_id,
                false,
                None,
                None,
                Some((&thread_id, &admission.run_id)),
            )
            .await
            .map_err(crate::SessionActivityError::run_error)?;
        let session_id = admission.session_id.clone();
        admission.session_activity_epoch = Some(activity_epoch);
        let admitted = coordination_runtime.admit_coordinated_run(admission).await;
        self.settle_definitive_coordination_rejection(
            session_id.as_str(),
            activity_epoch,
            &admitted,
        )
        .await?;
        let admitted = admitted?;
        if admitted.thread_id != thread_id {
            // A mismatched receipt is an internal/ambiguous Runtime fault: it may
            // name work that was durably accepted, so keep the activity fence for
            // recovery instead of making the Session falsely idle.
            return Err(RunError::internal(
                "Runtime returned a different coordinated Thread identity",
            ));
        }
        Ok(admitted)
    }

    async fn settle_session_agent_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), RunError> {
        let coordination_runtime = self.coordination.as_ref();

        if command.session_activity_epoch == 0 {
            return Err(RunError::bad_request(
                "Agent settlement does not identify a committed Session activity",
            ));
        }
        let role = if command.source_thread_id.0 == command.session_id {
            SessionAgentBoundaryRole::PrimaryContinuation
        } else {
            SessionAgentBoundaryRole::Child
        };
        let current = self
            .queries
            .session(&command.session_id)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        if current.is_terminal() {
            return Ok(());
        }
        // The dispatch claim already authenticates the exact frozen source Run.
        // A settlement may be replayed after its Session effect committed but
        // before the queue row reached Done; by then a successor Run can
        // legitimately be latest. Recovery therefore validates the exact
        // committed source below instead of creating a second, time-relative
        // identity fence from the Thread's latest projection.
        let (snapshot, state, observation) = self.committed_agent_boundary(&command).await?;
        let child_report = (role == SessionAgentBoundaryRole::Child)
            .then(|| session_agent_report_text(&snapshot.messages, &command.source_run_id))
            .filter(|report| !report.is_empty());
        let action = session_agent_boundary_action(
            role,
            &state,
            if observation.is_some() {
                SessionAgentBoundaryVisibility::Visible
            } else {
                SessionAgentBoundaryVisibility::Missing
            },
            command.cancellation_requested,
            if child_report.is_some() {
                SessionAgentCommittedReport::Present
            } else {
                SessionAgentCommittedReport::Absent
            },
        );
        match action {
            SessionAgentBoundaryAction::RejectNonBoundary => {
                return Err(RunError::bad_request(
                    "Agent settlement does not identify a committed child boundary",
                ));
            }
            SessionAgentBoundaryAction::RetryMissingProjection => {
                // The committed Run is authoritative but its lifecycle projection
                // is not visible yet. Wake the sole supervisor and return before
                // any root mutation; Pool relinquishes this exact claim for retry.
                self.lifecycle.request_reconciliation();
                return Err(RunError::unavailable(
                    "committed Agent boundary is not yet visible in the lifecycle feed",
                ));
            }
            SessionAgentBoundaryAction::InterruptThenSettle => {
                // Failure is absorbing for an ordinary coordinated Thread. The
                // existing Dispatch interrupt precedes direct settlement.
                coordination_runtime
                    .interrupt_session_thread(&command.session_id, &command.source_thread_id)
                    .await?;
            }
            SessionAgentBoundaryAction::Settle => {}
            SessionAgentBoundaryAction::ContinueReport => {}
        }
        let observation = observation.expect("visible action requires a lifecycle observation");
        let retained_user_event = role == SessionAgentBoundaryRole::PrimaryContinuation
            && awaken_session_contract::SessionEventHistory::from(&current)
                .retains_user_run(&command.source_run_id)
                .map_err(|error| RunError::internal(error.to_string()))?;
        if action != SessionAgentBoundaryAction::ContinueReport {
            // Keep the activity fence active until usage/budget reconciliation
            // has also committed. If that dependency is unavailable the queue
            // retains the boundary and Session remains Running for exact retry.
            self.reconcile_coordination_boundary_usage(current, &command.source_thread_id)
                .await?;
            if !retained_user_event {
                self.lifecycle
                    .settle_activity_observed(
                        &command.session_id,
                        command.session_activity_epoch,
                        Some(observation),
                    )
                    .await
                    .map_err(crate::SessionActivityError::run_error)?;
            }
            self.lifecycle.request_reconciliation();
            return Ok(());
        }

        let settled = current;
        // Current ownership, frozen roster and exact committed source are
        // checked before recovery. Mutable fresh preparation follows only a
        // confirmed Missing request, so lost-ACK replay has no new effects.
        let owner = self
            .mutations
            .owner(&command.session_id)
            .await
            .map_err(|error| {
                RunError::unavailable(format!("Session ownership is unavailable: {error}"))
            })?;
        let member = self
            .coordination_roster(&owner, &settled)?
            .into_iter()
            .find(|member| member.entry.agent_id == command.source_agent_id)
            .ok_or_else(|| {
                RunError::bad_request("settled child Agent is not in the frozen Session roster")
            })?;
        let child_report = child_report.expect("continue action requires a committed child report");
        let continuation = SessionAgentReportContinuation::from_committed_report(
            &command,
            &member.entry.name,
            &child_report,
        );
        let baseline = settled
            .frozen_baseline()
            .ok_or_else(|| RunError::unavailable("original report root publication is unknown"))?;
        if self
            .recover_coordinated_admission(
                &settled,
                awaken_session_execution_contract::CoordinatedAdmissionRef::Report(
                    &continuation,
                    baseline,
                ),
            )
            .await?
            .is_some()
        {
            return Ok(());
        }
        admit_fresh_agent_report(self, &owner, settled, observation, continuation).await
    }

    async fn finalize_session_agent_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), RunError> {
        if command.session_activity_epoch == 0 {
            return Err(RunError::bad_request(
                "Agent settlement does not identify a committed Session activity",
            ));
        }
        if command.source_thread_id.0 != command.session_id {
            return Ok(());
        }
        let current = self
            .queries
            .session(&command.session_id)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        if current.is_terminal() {
            return Ok(());
        }
        if !awaken_session_contract::SessionEventHistory::from(&current)
            .retains_user_run(&command.source_run_id)
            .map_err(|error| RunError::internal(error.to_string()))?
        {
            return Ok(());
        }
        let (_, state, observation) = self.committed_agent_boundary(&command).await?;
        let observation = observation.ok_or_else(|| {
            self.lifecycle.request_reconciliation();
            RunError::unavailable(
                "committed Agent boundary is not yet visible in the lifecycle feed",
            )
        })?;
        self.lifecycle
            .settle_user_event_dispatch_observed(
                &command.session_id,
                command.session_activity_epoch,
                observation,
                &command.source_run_id,
                session_agent_run_boundary(&state).ok_or_else(|| {
                    RunError::bad_request("invalid committed settlement boundary")
                })?,
            )
            .await
            .map_err(crate::SessionActivityError::run_error)?;
        self.lifecycle.request_reconciliation();
        Ok(())
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &ThreadId,
    ) -> Result<(), RunError> {
        let coordination_runtime = self.coordination.as_ref();

        self.validate_coordinated_thread(session_id, child_thread_id)
            .await?;
        coordination_runtime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    async fn reply_session_thread_tool(
        &self,
        command: SessionThreadToolReplyCommand,
    ) -> Result<(), RunError> {
        let coordination_runtime = self.coordination.as_ref();

        let (session_id, session_activity_epoch, delivery) =
            self.prepare_session_thread_tool_reply(command).await?;
        let result = coordination_runtime
            .reply_session_thread_tool(delivery)
            .await;
        if let Some(epoch) = session_activity_epoch {
            self.settle_definitive_coordination_rejection(&session_id, epoch, &result)
                .await?;
        }
        result
    }
}

#[async_trait::async_trait]
impl crate::ProfiledSessionReplies for SessionCoordination {
    /// Authorize the native primary-Run reply, then delegate the exact command
    /// to the existing Session activity/continuation/receipt owner.
    async fn reply_profiled_session_run_for_owner(
        &self,
        owner_scope: &str,
        command: awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), SessionProjectionRecoveryError> {
        use awaken_session_contract::SessionThreadTarget;
        use awaken_session_execution_contract::SessionAgentCoordination;
        self.run_admissions
            .require_profiled_run_owner(owner_scope, &command.session_id)
            .await?;
        if command.target != SessionThreadTarget::Primary
            || command
                .expected_thread_version
                .is_none_or(|version| version == 0)
            || command
                .tool_request_event_id
                .as_ref()
                .is_none_or(|id| id.trim().is_empty())
            || command.accompanying_system.is_some()
        {
            return Err(SessionProjectionRecoveryError::Rejected(
                RunError::bad_request(
                    "native Run reply requires one primary Run, exact version and stable operation",
                ),
            ));
        }
        SessionAgentCoordination::reply_session_thread_tool(self, command)
            .await
            .map_err(SessionProjectionRecoveryError::Rejected)
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[test]
    fn boundary_classification_admits_reports_only_for_completed_runs() {
        // Constraint/Invariant: the authoritative Session inputs and repository CAS
        // documented here remain the only decision source; no parallel ledger is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        use awaken_agent_contract::agent::run::{EndCause, Failure};

        // Cause/effect graph: C1 committed state is Awaiting, Running, or one of
        // the terminal lifecycle classes. Effects: E1 Awaiting settles directly;
        // E2 only Completed selects report continuation; E3 Failed/Cancelled
        // settle directly; E4 Running is rejected before Session mutation.
        //
        // | Rule | State | Effect |
        // | B1 | Awaiting | E1 |
        // | B2 | Ended(NaturalEnd) | E2 |
        // | B3 | Ended(Error) | E3 Failed |
        // | B4 | Ended(Cancelled) | E3 Cancelled |
        // | B5 | Running | E4 |
        assert_eq!(
            session_agent_run_boundary(&RunState::Awaiting).expect("B1"),
            awaken_session_contract::SessionAgentRunBoundary::Awaiting,
            "B1/E1"
        );
        assert_eq!(
            session_agent_run_boundary(&RunState::Ended(EndCause::NaturalEnd,)).expect("B2"),
            awaken_session_contract::SessionAgentRunBoundary::Completed,
            "B2/E2"
        );
        assert_eq!(
            session_agent_run_boundary(&RunState::Ended(EndCause::Error(Failure::StateConflict,)))
                .expect("B3"),
            awaken_session_contract::SessionAgentRunBoundary::Failed,
            "B3/E3"
        );
        assert_eq!(
            session_agent_run_boundary(&RunState::Ended(EndCause::Cancelled)).expect("B4"),
            awaken_session_contract::SessionAgentRunBoundary::Cancelled,
            "B4/E3"
        );
        assert!(
            session_agent_run_boundary(&RunState::Running).is_none(),
            "B5/E4"
        );
    }
}
