//! Session-owned admission for asynchronous Agent coordination.
//!
//! This module adds no child registry. It resolves the roster already frozen by
//! the Session baseline and delegates ordinary Thread/Run persistence to the
//! existing [`awaken_session_contract::SessionRuntime`] port.

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::{RunLifecycleEventKind, classify_run_lifecycle_event};
use awaken_session_contract::{
    CoordinatedRunCommand, CoordinatedRunIntent, RunError, SessionAgentBoundaryCommand,
    SessionAgentCoordination, SessionAgentMessageCommand, SessionAgentMessageReceipt,
    SessionAgentReportContinuation, SessionAgentRosterEntry, SessionAgentTarget,
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode, SessionThreadTarget,
    SessionThreadToolReplyCommand, SessionThreadToolReplyDelivery, coordinated_thread_failed,
    session_agent_report_text, session_run_activity_operation_id,
};

use crate::SessionApplication;

struct ResolvedRosterMember {
    entry: SessionAgentRosterEntry,
    snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
}

// Anthropic's public 25-Thread ceiling includes the primary Thread. Ordinary
// coordinated children therefore own the remaining 24 slots; Advisor
// consultations are explicitly capacity-exempt at the dispatch boundary.
const MAX_UNARCHIVED_AGENT_CHILD_THREADS: usize = 24;

#[derive(Debug, Clone, PartialEq)]
enum CoordinatedBoundary {
    Awaiting,
    Completed,
    Failed,
    Cancelled,
}

fn coordinated_boundary(state: &RunState) -> Result<CoordinatedBoundary, RunError> {
    match classify_run_lifecycle_event(state, None) {
        RunLifecycleEventKind::Awaiting => Ok(CoordinatedBoundary::Awaiting),
        RunLifecycleEventKind::Completed => Ok(CoordinatedBoundary::Completed),
        RunLifecycleEventKind::Failed => Ok(CoordinatedBoundary::Failed),
        RunLifecycleEventKind::Cancelled => Ok(CoordinatedBoundary::Cancelled),
        RunLifecycleEventKind::Running
        | RunLifecycleEventKind::Resumed
        | RunLifecycleEventKind::Rescheduled => Err(RunError::bad_request(
            "Agent settlement does not identify a committed child boundary",
        )),
    }
}

impl SessionApplication {
    async fn settle_definitive_coordination_rejection<T>(
        &self,
        session_id: &str,
        activity_epoch: u64,
        result: &Result<T, RunError>,
    ) -> Result<(), RunError> {
        if result
            .as_ref()
            .is_err_and(|error| error.kind == awaken_session_contract::RunErrorKind::BadRequest)
        {
            // A caller rejection proves the Runtime did not durably accept the
            // operation. Dependency/internal failures deliberately retain the
            // activity receipt: enqueue/delivery may have committed before its
            // response was lost, and exact retry must reuse the same epoch.
            self.settle_activity(session_id, activity_epoch)
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
        if session.is_terminal()
            || matches!(
                session.budget,
                awaken_session_contract::SessionBudgetState::Absent
            )
        {
            return Ok(session);
        }
        let usage = self
            .session_usage_for_model_request(&session.session_id, &source_thread_id.0)
            .await?;
        self.reconcile_managed_budget_usage(&session.session_id, usage)
            .await
            .map(|outcome| outcome.session)
            .map_err(|error| {
                RunError::unavailable(format!("Session budget settlement is unavailable: {error}"))
            })
    }

    async fn validate_coordinated_thread(
        &self,
        session_id: &str,
        child_thread_id: &ThreadId,
    ) -> Result<(), RunError> {
        if child_thread_id.0 == session_id {
            return Err(RunError::bad_request(
                "a coordinated child Thread must differ from its primary Session",
            ));
        }
        let links = self.runtime().coordinated_threads(session_id).await?;
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
        match baseline.agent_revision {
            Some(revision) => {
                self.session_profile_at_revision(owner_scope, &baseline.agent_id, revision)
            }
            None => self.session_profile(owner_scope, &baseline.agent_id),
        }
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
        let source = self
            .config_source
            .as_ref()
            .ok_or_else(|| RunError::unavailable("Agent publication source is unavailable"))?;
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
                .session_profile_at_revision(owner_scope, agent_id, revision)
                .ok_or_else(|| {
                    RunError::unavailable(format!(
                        "roster Agent `{agent_id}` revision {revision} is unavailable"
                    ))
                })?;
            let snapshot = source
                .executable_snapshot_at_revision_in(owner_scope, agent_id, revision)
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
        session_id: &str,
    ) -> Result<(String, awaken_session_contract::PersistedSession), RunError> {
        let session = self
            .session(session_id)
            .await
            .map_err(|error| match error {
                awaken_session_contract::SessionRepositoryError::NotFound => {
                    RunError::bad_request("Session was not found")
                }
                other => RunError::unavailable(other.to_string()),
            })?;
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
        let owner = self.owner(session_id).await.map_err(|error| match error {
            crate::SessionMutationError::NotFound => RunError::bad_request("Session was not found"),
            other => RunError::unavailable(other.to_string()),
        })?;
        Ok((owner, session))
    }
}

#[async_trait::async_trait]
impl SessionAgentCoordination for SessionApplication {
    async fn admit_session_run_activity(
        &self,
        session_id: &str,
        agent_id: &str,
        run_id: &RunId,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, RunError> {
        let operation_id = session_run_activity_operation_id(session_id, run_id);
        let admitted = match mode {
            SessionRunActivityAdmissionMode::RecoverOrAdmit => self
                .begin_admitted_activity_for_operation(agent_id, session_id, &operation_id)
                .await
                .map(Some),
            SessionRunActivityAdmissionMode::RecoverOnly => self
                .recover_activity_for_operation(session_id, &operation_id)
                .await
                .map_err(crate::SessionActivityError::run_error),
        };
        match admitted {
            Ok(None) => Ok(SessionRunActivityAdmission::Rejected),
            Ok(Some((_, session_activity_epoch))) => Ok(SessionRunActivityAdmission::Admitted {
                session_activity_epoch,
            }),
            Err(error)
                if error.kind == awaken_session_contract::RunErrorKind::BadRequest
                    || error.code == "budget_reached" =>
            {
                Ok(SessionRunActivityAdmission::Rejected)
            }
            Err(error) => Err(error),
        }
    }

    async fn admit_session_model_request(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<bool, RunError> {
        let session = self
            .session(session_id)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        if session.is_terminal() {
            return Err(RunError::bad_request(
                "a terminal Session cannot admit model requests",
            ));
        }
        let snapshot = self
            .runtime()
            .session_thread_recovery_snapshot(session_id, &thread_id.0)
            .await?
            .ok_or_else(|| {
                RunError::bad_request(
                    "model-request admission does not identify a committed Session Thread",
                )
            })?;
        if snapshot.latest_run_id.as_ref() != Some(run_id)
            || !snapshot.runs.iter().any(|run| {
                run.id == *run_id && matches!(run.state, RunState::Running | RunState::Awaiting)
            })
        {
            return Err(RunError::bad_request(
                "model-request admission does not identify the latest live Run",
            ));
        }
        let usage = self
            .session_usage_for_model_request(session_id, &thread_id.0)
            .await?;
        self.reconcile_managed_budget_usage(session_id, usage)
            .await
            .map(|outcome| outcome.session.budget.can_admit_model_request())
            .map_err(|error| {
                RunError::unavailable(format!("Session budget admission is unavailable: {error}"))
            })
    }

    async fn list_session_agents(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionAgentRosterEntry>, RunError> {
        let (owner, session) = self.coordination_session(session_id).await?;
        self.coordination_roster(&owner, &session)
            .map(|members| members.into_iter().map(|member| member.entry).collect())
    }

    async fn send_session_agent_message(
        &self,
        command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, RunError> {
        if command.source_thread_id.0 != command.session_id {
            return Err(RunError::bad_request(
                "only the primary Session Thread may coordinate Agents",
            ));
        }
        if command.message.trim().is_empty()
            || command.operation_id.trim().is_empty()
            || command.source_call_id.trim().is_empty()
        {
            return Err(RunError::bad_request(
                "Agent coordination requires a message and Runtime operation identity",
            ));
        }
        let (owner, session) = self.coordination_session(&command.session_id).await?;
        let roster = self.coordination_roster(&owner, &session)?;
        let existing_links = self
            .runtime()
            .coordinated_threads(&command.session_id)
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
                    awaken_session_contract::coordinated_thread_id(
                        &command.session_id,
                        &command.source_run_id,
                        &command.operation_id,
                    ),
                    member,
                    CoordinatedRunIntent::Spawn,
                )
            }
            SessionAgentTarget::ExistingThread { thread_id } => {
                if self
                    .runtime()
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
                            "only ordinary Agent Threads accept send_to_agent follow-ups",
                        )
                    })?;
                if let Some(snapshot) = self
                    .runtime()
                    .session_thread_recovery_snapshot(&command.session_id, &thread_id.0)
                    .await?
                {
                    let latest_run_id = snapshot.latest_run_id.as_ref().ok_or_else(|| {
                        RunError::internal(
                            "coordinated Thread recovery omitted its latest committed Run",
                        )
                    })?;
                    let latest_state = snapshot
                        .runs
                        .iter()
                        .find(|run| &run.id == latest_run_id)
                        .map(|run| &run.state)
                        .ok_or_else(|| {
                            RunError::internal(
                                "coordinated Thread recovery omitted its latest Run state",
                            )
                        })?;
                    if coordinated_thread_failed(latest_state) {
                        return Err(RunError::bad_request(
                            "Agent Thread terminated after a failed Run",
                        ));
                    }
                }
                (thread_id.clone(), member, CoordinatedRunIntent::FollowUp)
            }
        };

        let run_identity = awaken_session_contract::stable_fingerprint(&(
            "managed-coordinated-run-v1",
            command.session_id.as_str(),
            thread_id.0.as_str(),
            command.source_run_id.0.as_str(),
            command.operation_id.as_str(),
        ));
        let (_, activity_epoch) = self
            .begin_activity_for_operation(&command.session_id, &command.operation_id)
            .await
            .map_err(crate::SessionActivityError::run_error)?;
        let session_id = command.session_id.clone();
        let admitted = self
            .runtime()
            .admit_coordinated_run(CoordinatedRunCommand {
                intent,
                session_id: command.session_id,
                thread_id: thread_id.clone(),
                run_id: RunId(format!("coord-run-{run_identity}")),
                parent_run_id: command.source_run_id,
                parent_call_id: command.source_call_id,
                operation_id: command.operation_id,
                snapshot: member.snapshot,
                message: command.message,
                session_activity_epoch: activity_epoch,
                max_unarchived_threads: MAX_UNARCHIVED_AGENT_CHILD_THREADS,
            })
            .await;
        self.settle_definitive_coordination_rejection(&session_id, activity_epoch, &admitted)
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
        if command.session_activity_epoch == 0 {
            return Err(RunError::bad_request(
                "Agent settlement does not identify a committed Session activity",
            ));
        }
        let primary_continuation = command.source_thread_id.0 == command.session_id;
        let current = self
            .session(&command.session_id)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        if current.is_terminal() {
            return Ok(());
        }
        let snapshot = self
            .runtime()
            .session_thread_recovery_snapshot(&command.session_id, &command.source_thread_id.0)
            .await?
            .ok_or_else(|| RunError::bad_request("settled Agent Run was not committed"))?;
        if snapshot.claimed_run_id != command.source_run_id
            || snapshot.latest_run_id.as_ref() != Some(&command.source_run_id)
        {
            return Err(RunError::bad_request(
                "Agent settlement does not identify the latest committed Run",
            ));
        }
        let state = snapshot
            .runs
            .iter()
            .find(|run| run.id == command.source_run_id)
            .map(|run| &run.state)
            .ok_or_else(|| RunError::bad_request("settled Agent Run is absent from its Thread"))?;
        let boundary = coordinated_boundary(state)?;
        // Failure is absorbing for an ordinary coordinated Thread. Record
        // cancellation on any already-admitted later dispatch before settling
        // this activity; Dispatch remains the sole queued-work authority and
        // the latest failed Run remains the sole durable admission fence.
        if !primary_continuation && boundary == CoordinatedBoundary::Failed {
            self.runtime()
                .interrupt_session_thread(&command.session_id, &command.source_thread_id)
                .await?;
        }
        // Awaiting, Failed, Cancelled, cancellation-requested children, the
        // deterministic primary report Run, and a Completed child without a
        // committed non-empty report are true activity boundaries with no
        // automatic report continuation. Only a normally Completed child whose
        // exact snapshot contains a classifier-selected reply follows the
        // transfer path below and keeps that epoch active until its report Run
        // reaches this same observer. Cancellation provenance comes from the
        // claimed dispatch, never from transcript text or an overloaded cause.
        let child_report = (!primary_continuation
            && boundary == CoordinatedBoundary::Completed
            && !command.cancellation_requested)
            .then(|| session_agent_report_text(&snapshot.messages, &command.source_run_id))
            .filter(|report| !report.is_empty());
        if child_report.is_none() {
            // Keep the activity fence active until usage/budget reconciliation
            // has also committed. If that dependency is unavailable the queue
            // retains the boundary and Session remains Running for exact retry.
            self.reconcile_coordination_boundary_usage(current, &command.source_thread_id)
                .await?;
            self.settle_activity(&command.session_id, command.session_activity_epoch)
                .await
                .map_err(crate::SessionActivityError::run_error)?;
            self.wake_lifecycle_supervisor();
            return Ok(());
        }

        let settled = current;
        // Child execution happens outside the Managed HTTP request that
        // normally reconciles cumulative usage. Reconcile at the terminal
        // boundary while retaining the activity epoch; the cumulative cursor
        // makes crash redelivery idempotent.
        let settled = self
            .reconcile_coordination_boundary_usage(settled, &command.source_thread_id)
            .await?;
        let owner = self.owner(&command.session_id).await.map_err(|error| {
            RunError::unavailable(format!("Session ownership is unavailable: {error}"))
        })?;
        let member = self
            .coordination_roster(&owner, &settled)?
            .into_iter()
            .find(|member| member.entry.agent_id == command.source_agent_id)
            .ok_or_else(|| {
                RunError::bad_request("settled child Agent is not in the frozen Session roster")
            })?;
        let child_report = child_report.expect("normal Completed child has a committed report");
        let report = format!(
            "Message from agent {} (thread {}):\n{}",
            member.entry.name, command.source_thread_id.0, child_report
        );
        let continued = self
            .runtime()
            .continue_session_agent_report(SessionAgentReportContinuation {
                session_id: command.session_id.clone(),
                source_thread_id: command.source_thread_id,
                source_run_id: command.source_run_id.clone(),
                session_activity_epoch: command.session_activity_epoch,
                message: Message::text(
                    MessageId::agent_thread_report(&command.source_run_id),
                    Role::User,
                    report,
                ),
            })
            .await;
        self.settle_definitive_coordination_rejection(
            &command.session_id,
            command.session_activity_epoch,
            &continued,
        )
        .await?;
        continued
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &str,
        child_thread_id: &ThreadId,
    ) -> Result<(), RunError> {
        self.validate_coordinated_thread(session_id, child_thread_id)
            .await?;
        self.runtime()
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    async fn reply_session_thread_tool(
        &self,
        command: SessionThreadToolReplyCommand,
    ) -> Result<(), RunError> {
        if command.session_id.trim().is_empty()
            || command.expected_run_id.0.trim().is_empty()
            || command.expected_correlation_id.trim().is_empty()
            || command.tool_use_id.trim().is_empty()
        {
            return Err(RunError::bad_request(
                "Session Thread reply requires exact Run, correlation, and tool identities",
            ));
        }
        if let SessionThreadTarget::Child(child_thread_id) = &command.target {
            self.validate_coordinated_thread(&command.session_id, child_thread_id)
                .await?;
            if self
                .runtime()
                .session_thread_disposition(&command.session_id, &child_thread_id.0)
                .await?
                == awaken_agent_contract::ThreadDisposition::Archived
            {
                return Err(RunError::bad_request("Agent Thread is archived"));
            }
        }
        // Runtime reads the exact committed Awaiting Run/correlation from the
        // Session partition and checks it against the admission-time command.
        // The Session owns activity admission; delivery revalidates the same
        // command and transient activity coordinate after this root CAS.
        let fence = self
            .runtime()
            .session_thread_tool_reply_fence(&command)
            .await?;
        let operation_id = command.activity_operation_id();
        let session_id = command.session_id.clone();
        let (_, session_activity_epoch) = self
            .transfer_committed_activity_for_operation(
                &session_id,
                &operation_id,
                fence.prior_session_activity_epoch,
            )
            .await
            .map_err(crate::SessionActivityError::run_error)?;
        let result = self
            .runtime()
            .reply_session_thread_tool(SessionThreadToolReplyDelivery {
                command,
                fence,
                session_activity_epoch,
            })
            .await;
        self.settle_definitive_coordination_rejection(&session_id, session_activity_epoch, &result)
            .await?;
        result
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
            coordinated_boundary(&RunState::Awaiting).expect("B1"),
            CoordinatedBoundary::Awaiting,
            "B1/E1"
        );
        assert_eq!(
            coordinated_boundary(&RunState::Ended(EndCause::NaturalEnd,)).expect("B2"),
            CoordinatedBoundary::Completed,
            "B2/E2"
        );
        assert_eq!(
            coordinated_boundary(&RunState::Ended(EndCause::Error(Failure::StateConflict,)))
                .expect("B3"),
            CoordinatedBoundary::Failed,
            "B3/E3"
        );
        assert_eq!(
            coordinated_boundary(&RunState::Ended(EndCause::Cancelled)).expect("B4"),
            CoordinatedBoundary::Cancelled,
            "B4/E3"
        );
        assert!(coordinated_boundary(&RunState::Running).is_err(), "B5/E4");
    }
}
