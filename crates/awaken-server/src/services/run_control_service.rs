//! Framework-level run control operations.
//!
//! This service centralizes the semantics used by HTTP routes and protocol
//! adapters for active-run lookup, cancellation, interrupt, HITL decisions, and
//! user input injection.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::mailbox::MailboxInterrupt;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunQuery, RunRecord, RunWaitingState, StorageError};
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_runtime::RunActivation;

use crate::app::RunModuleState;
use crate::mailbox::{Mailbox, MailboxError, MailboxSubmitResult};
use std::sync::Arc;

/// How injected user input should interact with any active work.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    /// Queue the new input behind the current active run.
    #[default]
    Queue,
    /// Interrupt the active run for the thread, then queue the new input.
    InterruptThenQueue,
    /// Append input to the current open waiting run and continue the same run ID.
    ResumeOpenRun,
}

/// Thread interrupt policy.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InterruptMode {
    /// Request cooperative cancellation and supersede queued mailbox dispatches.
    #[default]
    Graceful,
}

/// A run that is still controllable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActiveRun {
    pub thread_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<awaken_contract::contract::lifecycle::TerminationReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<RunWaitingState>,
}

impl From<RunRecord> for ActiveRun {
    fn from(run: RunRecord) -> Self {
        Self {
            thread_id: run.thread_id,
            run_id: run.run_id,
            agent_id: run.agent_id,
            status: run.status,
            termination_reason: run.termination_reason,
            dispatch_id: run.dispatch_id,
            session_id: run.session_id,
            waiting: run.waiting,
        }
    }
}

/// Errors raised by framework run-control operations.
#[derive(Debug, Error)]
pub enum RunControlError {
    #[error("thread not found: {0}")]
    ThreadNotFound(String),
    #[error("run not found: {0}")]
    RunNotFound(String),
    #[error("decision target not found: {0}")]
    DecisionTargetNotFound(String),
    #[error("storage error: {0}")]
    Store(#[from] StorageError),
    #[error("mailbox error: {0}")]
    Mailbox(#[from] MailboxError),
}

/// Unified control plane for active runs.
#[derive(Clone)]
pub struct RunControlService {
    state: RunModuleState,
}

impl RunControlService {
    pub fn new(state: RunModuleState) -> Self {
        Self { state }
    }

    fn mailbox(&self) -> Arc<Mailbox> {
        self.state.mailbox()
    }

    /// Return the most recent running or waiting run for a thread.
    pub async fn get_active_run(
        &self,
        thread_id: &str,
    ) -> Result<Option<ActiveRun>, RunControlError> {
        if let Some(thread) = self.state.store().load_thread(thread_id).await? {
            if let Some(active) = self
                .load_projected_run(
                    thread_id,
                    thread.active_run_id.as_deref(),
                    &[RunStatus::Running],
                )
                .await?
            {
                return Ok(Some(active));
            }

            if let Some(open) = self
                .load_projected_run(
                    thread_id,
                    thread.open_run_id.as_deref(),
                    &[RunStatus::Running, RunStatus::Waiting],
                )
                .await?
            {
                return Ok(Some(open));
            }
        }

        self.scan_active_run(thread_id).await
    }

    async fn load_projected_run(
        &self,
        thread_id: &str,
        run_id: Option<&str>,
        allowed_statuses: &[RunStatus],
    ) -> Result<Option<ActiveRun>, RunControlError> {
        let Some(run_id) = run_id else {
            return Ok(None);
        };
        let Some(run) = self.state.store().load_run(run_id).await? else {
            return Ok(None);
        };
        if run.thread_id == thread_id && allowed_statuses.contains(&run.status) {
            Ok(Some(ActiveRun::from(run)))
        } else {
            Ok(None)
        }
    }

    async fn scan_active_run(&self, thread_id: &str) -> Result<Option<ActiveRun>, RunControlError> {
        let mut candidates = Vec::new();
        for status in [RunStatus::Running, RunStatus::Waiting] {
            let page = self
                .state
                .store()
                .list_runs(&RunQuery {
                    offset: 0,
                    limit: 200,
                    thread_id: Some(thread_id.to_string()),
                    status: Some(status),
                })
                .await?;
            candidates.extend(page.items);
        }

        Ok(candidates
            .into_iter()
            .max_by_key(|run| run.updated_at)
            .map(ActiveRun::from))
    }

    /// Submit a tool-call decision to a waiting active run.
    ///
    /// Remote live delivery is at-least-once when its ack is lost before the
    /// durable fallback is enqueued; `(tool_call_id, decision_id)` identifies
    /// duplicate decisions.
    pub async fn decide(
        &self,
        id: &str,
        tool_call_id: String,
        resume: ToolCallResume,
    ) -> Result<(), RunControlError> {
        let mailbox = self.mailbox();
        if mailbox
            .send_decision_live(id, tool_call_id.clone(), resume.clone())
            .await?
        {
            Ok(())
        } else {
            self.enqueue_durable_decision(id, tool_call_id, resume)
                .await
        }
    }

    async fn enqueue_durable_decision(
        &self,
        id: &str,
        tool_call_id: String,
        resume: ToolCallResume,
    ) -> Result<(), RunControlError> {
        let run = if let Some(run) = self.state.store().load_run(id).await? {
            run
        } else if let Some(active) = self.get_active_run(id).await? {
            self.state
                .store()
                .load_run(&active.run_id)
                .await?
                .ok_or_else(|| RunControlError::RunNotFound(active.run_id.clone()))?
        } else {
            return Err(RunControlError::DecisionTargetNotFound(id.to_string()));
        };

        if run.status != RunStatus::Waiting
            || !run.is_resumable_waiting()
            || !waiting_contains_ticket(&run, &tool_call_id)
        {
            return Err(RunControlError::DecisionTargetNotFound(id.to_string()));
        }

        let request = RunActivation::new(run.thread_id.clone(), Vec::new())
            .with_agent_id(run.agent_id.clone())
            .with_continue_run_id(run.run_id.clone())
            .with_decisions(vec![(tool_call_id.clone(), resume.clone())]);
        let mailbox = self.mailbox();
        mailbox.submit_background(request).await?;
        mailbox
            .record_mailbox_decision_received_for_run(
                &run,
                &tool_call_id,
                &resume,
                "durable_dispatch",
            )
            .await;
        Ok(())
    }

    /// Cancel an active run or queued mailbox dispatch by run ID, dispatch ID, or thread ID.
    pub async fn cancel_run(&self, id: &str) -> Result<(), RunControlError> {
        if self.mailbox().cancel(id).await? {
            Ok(())
        } else {
            Err(RunControlError::RunNotFound(id.to_string()))
        }
    }

    /// Interrupt a thread, superseding queued work and cancelling the active run.
    pub async fn interrupt_thread(
        &self,
        thread_id: &str,
        _mode: InterruptMode,
    ) -> Result<MailboxInterrupt, RunControlError> {
        let interrupted = self.mailbox().interrupt(thread_id).await?;
        if interrupted.active_dispatch.is_some() || interrupted.superseded_count > 0 {
            Ok(interrupted)
        } else {
            Err(RunControlError::ThreadNotFound(thread_id.to_string()))
        }
    }

    /// Inject messages into a thread, optionally interrupting the active run first.
    pub async fn inject_user_input(
        &self,
        thread_id: &str,
        agent_id: Option<String>,
        messages: Vec<Message>,
        mode: InputMode,
    ) -> Result<MailboxSubmitResult, RunControlError> {
        let thread = self
            .state
            .store()
            .load_thread(thread_id)
            .await?
            .ok_or_else(|| RunControlError::ThreadNotFound(thread_id.to_string()))?;

        if mode == InputMode::InterruptThenQueue {
            let _ = self
                .mailbox()
                .interrupt(thread_id)
                .await
                .map_err(RunControlError::Mailbox)?;
        }

        let mut request = RunActivation::new(thread_id.to_string(), messages);
        if mode == InputMode::ResumeOpenRun {
            let run = self
                .load_open_waiting_run(thread_id, thread.open_run_id.as_deref())
                .await?;
            request = request
                .with_agent_id(run.agent_id)
                .with_continue_run_id(run.run_id);
        } else if let Some(agent_id) = agent_id {
            request = request.with_agent_id(agent_id);
        }

        self.mailbox()
            .submit_background(request)
            .await
            .map_err(RunControlError::Mailbox)
    }

    /// Inject messages into the active run when possible, otherwise queue them.
    pub async fn inject_user_input_live_then_queue(
        &self,
        thread_id: &str,
        agent_id: Option<String>,
        messages: Vec<Message>,
    ) -> Result<MailboxSubmitResult, RunControlError> {
        let _thread = self
            .state
            .store()
            .load_thread(thread_id)
            .await?
            .ok_or_else(|| RunControlError::ThreadNotFound(thread_id.to_string()))?;

        let mut request = RunActivation::new(thread_id.to_string(), messages);
        if let Some(agent_id) = agent_id {
            request = request.with_agent_id(agent_id);
        }

        self.mailbox()
            .submit_live_then_queue(request, None)
            .await
            .map_err(RunControlError::Mailbox)
    }

    /// Inject messages using an existing run as the thread and agent anchor.
    pub async fn inject_run_input(
        &self,
        run_id: &str,
        messages: Vec<Message>,
        mode: InputMode,
    ) -> Result<MailboxSubmitResult, RunControlError> {
        let run = self
            .state
            .store()
            .load_run(run_id)
            .await?
            .ok_or_else(|| RunControlError::RunNotFound(run_id.to_string()))?;

        if mode == InputMode::InterruptThenQueue {
            return self
                .inject_user_input(&run.thread_id, Some(run.agent_id), messages, mode)
                .await;
        }

        if run.status == RunStatus::Waiting && run.is_resumable_waiting() {
            let request = RunActivation::new(run.thread_id.clone(), messages)
                .with_agent_id(run.agent_id)
                .with_continue_run_id(run.run_id);
            return self
                .mailbox()
                .submit_background(request)
                .await
                .map_err(RunControlError::Mailbox);
        }

        self.inject_user_input(&run.thread_id, Some(run.agent_id), messages, mode)
            .await
    }

    /// Inject messages using an existing run as the live-delivery anchor.
    pub async fn inject_run_input_live_then_queue(
        &self,
        run_id: &str,
        messages: Vec<Message>,
    ) -> Result<MailboxSubmitResult, RunControlError> {
        let run = self
            .state
            .store()
            .load_run(run_id)
            .await?
            .ok_or_else(|| RunControlError::RunNotFound(run_id.to_string()))?;

        let request =
            RunActivation::new(run.thread_id.clone(), messages).with_agent_id(run.agent_id.clone());
        self.mailbox()
            .submit_live_then_queue(request, Some(&run.run_id))
            .await
            .map_err(RunControlError::Mailbox)
    }

    async fn load_open_waiting_run(
        &self,
        thread_id: &str,
        open_run_id: Option<&str>,
    ) -> Result<RunRecord, RunControlError> {
        let Some(open_run_id) = open_run_id else {
            return Err(RunControlError::RunNotFound(format!(
                "open run for thread {thread_id}"
            )));
        };
        let run = self
            .state
            .store()
            .load_run(open_run_id)
            .await?
            .ok_or_else(|| RunControlError::RunNotFound(open_run_id.to_string()))?;
        if run.thread_id != thread_id
            || run.status != RunStatus::Waiting
            || !run.is_resumable_waiting()
        {
            return Err(RunControlError::RunNotFound(open_run_id.to_string()));
        }
        Ok(run)
    }
}

fn waiting_contains_ticket(run: &RunRecord, target: &str) -> bool {
    let Some(waiting) = run.waiting.as_ref() else {
        return false;
    };
    waiting.ticket_ids.iter().any(|id| id == target)
        || waiting
            .tickets
            .iter()
            .any(|ticket| ticket.ticket_id == target || ticket.tool_call_id == target)
}
