// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Native Run reads project existing committed Session and Dispatch
// @formal reason: facts; replies delegate to the one Session coordination admission.
//! Exact Run reads over the existing Session, dispatch and Thread authorities.

use awaken_agent_contract::RunLifecycleCursor;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::run::RunState;
use awaken_dispatch_contract::{DispatchCursor, DispatchPage, SessionRunEventPage};
use awaken_session_types::RunError;
use awaken_session_types::SessionId;

use crate::{SessionApplication, SessionProjectionRecoveryError};

impl SessionApplication {
    async fn require_profiled_run_owner(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<(), SessionProjectionRecoveryError> {
        let projection = self
            .read_session_projection(session_id.as_str(), Some(owner_scope))
            .await?
            .ok_or(SessionProjectionRecoveryError::NotFound)?;
        let baseline = projection.session.frozen_baseline().ok_or_else(|| {
            SessionProjectionRecoveryError::Unavailable(
                "Session creation finalization is incomplete".into(),
            )
        })?;
        if baseline.mutation_policy.is_managed() {
            return Err(SessionProjectionRecoveryError::NotFound);
        }
        Ok(())
    }

    /// Authorize the native primary-Run reply, then delegate the exact command
    /// to the existing Session activity/continuation/receipt owner.
    pub async fn reply_profiled_session_run_for_owner(
        &self,
        owner_scope: &str,
        command: awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), SessionProjectionRecoveryError> {
        use awaken_session_execution_contract::{SessionAgentCoordination, SessionThreadTarget};
        self.require_profiled_run_owner(owner_scope, &command.session_id)
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

    /// Observe a bounded native page after durable Session authorization.
    /// Dispatch and Thread are separate consistency boundaries: observe the
    /// delivery page first, then derive lifecycle/output/ticket from one exact
    /// recovery prefix. No Managed event history or process-local state is read.
    pub async fn session_run_events_after(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        run_id: &RunId,
        dispatch_after: DispatchCursor,
        lifecycle_after: RunLifecycleCursor,
        limit: usize,
    ) -> Result<SessionRunEventPage, SessionProjectionRecoveryError> {
        let thread_reads = self.runtime().thread_reads();

        self.require_profiled_run_owner(owner_scope, session_id)
            .await?;
        if run_id.0.trim().is_empty() || limit > 1_024 {
            return Err(SessionProjectionRecoveryError::Rejected(
                RunError::bad_request(
                    "native Run observation requires an exact Run and a limit no greater than 1024",
                ),
            ));
        }
        let dispatch = if limit == 0 {
            DispatchPage {
                events: Vec::new(),
                next_cursor: dispatch_after,
            }
        } else {
            self.dispatch_observations
                .get()
                .ok_or_else(|| {
                    SessionProjectionRecoveryError::Unavailable(
                        "Session dispatch observation source is unavailable".into(),
                    )
                })?
                .events_after(dispatch_after, limit, Some(run_id))
                .await
                .map_err(|error| SessionProjectionRecoveryError::Unavailable(error.to_string()))?
        };
        let snapshot = thread_reads
            .session_thread_run_recovery_snapshot(session_id, session_id.as_str(), run_id)
            .await
            .map_err(SessionProjectionRecoveryError::Rejected)?;
        let Some(snapshot) = snapshot else {
            // Admission precedes the first Thread commit. Reuse its durable
            // Session activity receipt to prove membership without creating a
            // Run record or relying on process-local "latest Run" state.
            let operation = awaken_session_execution_contract::session_run_activity_operation_id(
                session_id, run_id,
            );
            self.recover_activity_for_operation(
                session_id,
                &operation,
                Some((
                    &awaken_agent_contract::ThreadId(session_id.to_string()),
                    run_id,
                )),
            )
            .await
            .map_err(|error| SessionProjectionRecoveryError::Unavailable(error.to_string()))?
            .ok_or(SessionProjectionRecoveryError::NotFound)?;
            return Ok(SessionRunEventPage {
                session_id: session_id.to_string(),
                run_id: run_id.clone(),
                thread_version: None,
                state: None,
                dispatch,
                lifecycle: awaken_agent_contract::RunLifecyclePage {
                    events: Vec::new(),
                    next_cursor: lifecycle_after,
                },
                resume_ticket: None,
                response: String::new(),
                usage: None,
            });
        };
        let run = snapshot
            .run_record(run_id)
            .map_err(|error| {
                SessionProjectionRecoveryError::Rejected(RunError::internal(error.to_string()))
            })?
            .filter(|run| run.thread_id.0 == session_id.as_str())
            .ok_or(SessionProjectionRecoveryError::NotFound)?;
        let mut lifecycle = snapshot
            .lifecycle_events_after(lifecycle_after, limit)
            .map_err(|error| {
                SessionProjectionRecoveryError::Rejected(RunError::internal(error.to_string()))
            })?;
        lifecycle
            .events
            .retain(|event| &event.run_id == run_id && event.thread_id.0 == session_id.as_str());
        let resume_ticket = if run.state == RunState::Awaiting {
            snapshot
                .resume_tickets
                .iter()
                .find(|entry| {
                    &entry.run_id == run_id && entry.ticket.thread_id.0 == session_id.as_str()
                })
                .map(|entry| entry.ticket.clone())
        } else {
            None
        };
        let response = snapshot
            .messages
            .iter()
            .rev()
            .find(|message| {
                message.role == awaken_agent_contract::agent::message::Role::Assistant
                    && message.id.assistant_step_of(run_id).is_some()
            })
            .map(|message| message.text_content())
            .unwrap_or_default();
        let usage =
            awaken_runtime_contract::llm::ThreadUsage::from_committed_state(&snapshot.state);
        Ok(SessionRunEventPage {
            session_id: session_id.to_string(),
            run_id: run_id.clone(),
            thread_version: Some(snapshot.thread_version),
            state: Some(run.state.clone()),
            dispatch,
            lifecycle,
            resume_ticket,
            response,
            usage: (!usage.is_empty())
                .then(|| serde_json::to_value(usage).expect("ThreadUsage serializes")),
        })
    }
}
