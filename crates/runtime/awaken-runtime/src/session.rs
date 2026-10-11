// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: The embedded SDK binds one Snapshot, Thread and execution Context;
// @formal reason: lifecycle and recovery continue through the existing Runtime authority.
//! Embedded conversation bindings, independent of hosted Session aggregates.

use crate::{ApprovalError, ApprovalOptions, ConfigurationError, RunInput, RunOutcome, Runtime};
use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::Error;
use awaken_runtime_contract::{ExecutableAgentSnapshot, ResumeResult, RuntimeRunContext};
use std::future::Future;

/// Owns the bindings for a conversation, not a second lifecycle/permission store.
pub struct EmbeddedSession<'a> {
    runtime: &'a Runtime,
    snapshot: &'a ExecutableAgentSnapshot,
    thread_id: ThreadId,
    context: RuntimeRunContext,
}

impl<'a> EmbeddedSession<'a> {
    pub(crate) fn new(
        runtime: &'a Runtime,
        snapshot: &'a ExecutableAgentSnapshot,
        thread_id: ThreadId,
        context: RuntimeRunContext,
    ) -> Result<Self, Error> {
        crate::configuration::validate_configuration(runtime)?;
        if context.reader.is_none() {
            return Err(ConfigurationError::MissingHistoryReader.into());
        }
        if context.commit.is_none() {
            return Err(ConfigurationError::MissingCommitCoordinator.into());
        }
        Ok(Self {
            runtime,
            snapshot,
            thread_id,
            context,
        })
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    /// Start a new Run on this conversation and read its committed receipt.
    pub async fn run(&self, input: impl Into<RunInput>) -> Result<RunOutcome, Error> {
        let (run_id, _) = self
            .runtime
            .start_run(
                self.snapshot,
                self.thread_id.0.clone(),
                input,
                self.context.clone(),
            )
            .await?;
        self.outcome(&run_id)
    }

    /// The current committed receipt for a Run belonging to this conversation.
    pub fn outcome(&self, run_id: &RunId) -> Result<RunOutcome, Error> {
        let reader = self
            .context
            .reader
            .as_deref()
            .ok_or(ConfigurationError::MissingHistoryReader)?;
        Ok(RunOutcome::read(reader, run_id, &self.thread_id)?)
    }

    /// Drive async approval with no callback timeout; context cancellation is live.
    pub async fn run_to_completion_async<F, Fut>(
        &self,
        input: impl Into<RunInput>,
        decide: F,
    ) -> Result<RunOutcome, Error>
    where
        F: FnMut(ResumeTicket) -> Fut,
        Fut: Future<Output = Result<ResumeResult, ApprovalError>>,
    {
        self.run_to_completion_async_with_options(input, ApprovalOptions::default(), decide)
            .await
    }

    /// Timeout/failure preserves Awaiting; cancellation uses durable Run control.
    pub async fn run_to_completion_async_with_options<F, Fut>(
        &self,
        input: impl Into<RunInput>,
        options: ApprovalOptions,
        mut decide: F,
    ) -> Result<RunOutcome, Error>
    where
        F: FnMut(ResumeTicket) -> Fut,
        Fut: Future<Output = Result<ResumeResult, ApprovalError>>,
    {
        let (run_id, activation) = self.runtime.prepare_with_context(
            self.snapshot,
            self.thread_id.0.clone(),
            input,
            &self.context,
        );
        crate::approval::drive_to_completion(
            self.runtime,
            run_id.clone(),
            activation,
            self.context.clone(),
            &mut decide,
            &options,
        )
        .await?;
        self.outcome(&run_id)
    }

    /// Recover/replay an existing Run identity using the exact committed ticket.
    pub async fn resume_with_id_async<F, Fut>(
        &self,
        run_id: RunId,
        input: impl Into<RunInput>,
        decide: F,
    ) -> Result<RunOutcome, Error>
    where
        F: FnMut(ResumeTicket) -> Fut,
        Fut: Future<Output = Result<ResumeResult, ApprovalError>>,
    {
        self.resume_with_id_async_with_options(run_id, input, ApprovalOptions::default(), decide)
            .await
    }

    /// Recover with the same store clock and callback policy as a fresh Run.
    pub async fn resume_with_id_async_with_options<F, Fut>(
        &self,
        run_id: RunId,
        input: impl Into<RunInput>,
        options: ApprovalOptions,
        mut decide: F,
    ) -> Result<RunOutcome, Error>
    where
        F: FnMut(ResumeTicket) -> Fut,
        Fut: Future<Output = Result<ResumeResult, ApprovalError>>,
    {
        let activation = RunActivation::new(
            run_id.clone(),
            self.thread_id.clone(),
            self.snapshot.clone(),
            input.into().0,
        )
        .with_inherited_toolsets(self.context.inherited_toolsets.clone());
        crate::approval::drive_to_completion(
            self.runtime,
            run_id.clone(),
            activation,
            self.context.clone(),
            &mut decide,
            &options,
        )
        .await?;
        self.outcome(&run_id)
    }
}
