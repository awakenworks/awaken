// @formal surface obligation=session_run.execution_requires_dispatch_work_and_realization_owner
// @formal reason: A Coordinator delivers every ordinary Thread Run and resume as durable dispatch work a
// @formal reason: registered Worker claims, and observes only the committed Run it names. This classifies the
// @formal reason: submission and observation edge, not a proof claim.
//! Ordinary Thread Runs on the Coordinator: the flat-protocol Runs (AI SDK,
//! AG-UI, A2A) and the Session application's direct Run roles. Each Run is
//! enqueued as durable dispatch work a registered Worker claims; the caller
//! observes it settle through the completion relay and committed truth.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_dispatch_contract::{DispatchEnqueue as _, Inbox as _};
use awaken_runtime_contract::resume::ResumeCommand;
use awaken_runtime_contract::{DataSubjectId, RunActivation};
use awaken_session_application_contract::{RunResume, StepOutcome};
use awaken_session_realization_contract::CanonicalSessionProjection;
use awaken_session_types::RunError;

use super::{SETTLED_RECONCILIATION_INTERVAL, WorkerPlacedSessions};
use crate::session_run_activation::read_settled_phase;
use crate::{
    SessionLowering as _, StepCommitExpectation, authoritative_step_snapshot,
    await_completion_state, durable_resume_input, project_committed_step, resume_result,
    settled_step,
};

/// The Agent an ordinary Run selects when it names none.
const DEFAULT_AGENT: &str = "assistant";

/// One ordinary Run of a Thread.
pub(crate) struct ThreadRun<'a> {
    pub(crate) agent: Option<&'a str>,
    pub(crate) thread: &'a str,
    pub(crate) input: Vec<Message>,
    pub(crate) sink: Option<Arc<dyn StreamSink>>,
    pub(crate) data_subject_id: Option<DataSubjectId>,
}

impl WorkerPlacedSessions {
    /// Run `run.thread` once and observe the Run settle.
    ///
    /// Decision table (causes → effect):
    /// | Rule | cause | effect |
    /// |---|---|---|
    /// | T1 | the Thread is a Managed Session root | refused: Sessions admit their own Runs |
    /// | T2 | the Thread awaits a tool decision | refused |
    /// | T3 | the Agent has no publication in the Workspace | refused: a Coordinator runs published Agents |
    /// | T4 | otherwise | one durable Run a Worker claims; the committed step after its inputs |
    pub(crate) async fn run_thread(&self, run: ThreadRun<'_>) -> Result<StepOutcome, RunError> {
        let ThreadRun {
            agent,
            thread,
            input,
            sink,
            data_subject_id,
        } = run;
        let projections = self.ports.projections.as_ref();
        if projections.projection(thread)?.is_some() {
            return Err(RunError::bad_request(
                "Managed Session roots must use Session-owned Run ingress",
            ));
        }
        let thread_id = ThreadId(thread.to_owned());
        let commit = self.ports.reads.commit_for_read(thread).await?;
        if commit
            .open_wait_for_thread(&thread_id)
            .await
            .map_err(RunError::internal)?
            .is_some()
        {
            return Err(RunError::bad_request("thread is awaiting a tool decision"));
        }
        let agent = agent.unwrap_or(DEFAULT_AGENT);
        let (publication, _) =
            awaken_session_realization_contract::resolve_canonical_session_projection(
                projections.publications(),
                projections.local_workspace(),
                CanonicalSessionProjection::LegacyAgent(agent),
                None,
                None,
            )?;
        let snapshot = publication.ok_or_else(|| {
            RunError::bad_request(format!(
                "Agent `{agent}` has no publication in this Workspace; a Coordinator runs published Agents only"
            ))
        })?;
        let run_id = RunId(awaken_agent_contract::fresh_process_id("run"));
        let messages_before = authoritative_step_snapshot(&commit, &thread_id, &run_id)
            .await?
            .messages
            .len();
        let mut activation = RunActivation::new(run_id.clone(), thread_id.clone(), snapshot, input);
        activation.data_subject_id = data_subject_id;
        let input_ids = activation
            .input
            .iter()
            .map(|message| message.id.0.clone())
            .collect::<Vec<_>>();
        let (settled, _waiter_guard) = self.ports.completion.register(&run_id, sink)?;
        let request = projections.prepare(activation).fresh()?;
        self.ports
            .owner
            .enqueue(request)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        let state = await_completion_state(settled, SETTLED_RECONCILIATION_INTERVAL, || {
            read_settled_phase(&commit, &thread_id, &run_id, None)
        })
        .await?;
        let step = self.step_projection(thread)?;
        let _projection = step.lock().await;
        settled_step(
            project_committed_step(
                &commit,
                &self.ports.hub,
                &thread_id,
                run_id,
                state,
                StepCommitExpectation {
                    messages_before,
                    input_ids: &input_ids,
                },
                thread,
            )
            .await?,
        )
    }

    /// Answer `thread`'s awaiting Run and observe the resumed Run settle; the
    /// answer reaches the Worker as the Run's durable input.
    pub(crate) async fn resume_thread(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
        sink: Option<Arc<dyn StreamSink>>,
    ) -> Result<StepOutcome, RunError> {
        let thread_id = ThreadId(thread.to_owned());
        let commit = self.ports.reads.commit_for_read(thread).await?;
        let (run_id, ticket) = commit
            .open_wait_for_thread(&thread_id)
            .await
            .map_err(RunError::internal)?
            .ok_or_else(|| RunError::bad_request("no awaiting run to resume"))?;
        let awaiting = authoritative_step_snapshot(&commit, &thread_id, &run_id).await?;
        let result =
            resume_result(&commit, &awaiting, &run_id, &ticket, tool_use_id, resume).await?;
        let messages_before = awaiting.messages.len();
        let command = ResumeCommand::from_ticket(&ticket, result, 0);
        let correlation = command.correlation_id.clone();
        let (settled, _waiter_guard) = self.ports.completion.register(&run_id, sink)?;
        self.ports
            .owner
            .append(durable_resume_input(command))
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        let state = await_completion_state(settled, SETTLED_RECONCILIATION_INTERVAL, || {
            read_settled_phase(&commit, &thread_id, &run_id, Some(&correlation))
        })
        .await?;
        let step = self.step_projection(thread)?;
        let _projection = step.lock().await;
        settled_step(
            project_committed_step(
                &commit,
                &self.ports.hub,
                &thread_id,
                run_id,
                state,
                StepCommitExpectation {
                    messages_before,
                    input_ids: &[],
                },
                thread,
            )
            .await?,
        )
    }

    /// Persist cancellation of every executable dispatch of `thread`; the
    /// claiming Worker settles each cancelled Run.
    pub(crate) async fn interrupt_thread(&self, thread: &str) -> Result<(), RunError> {
        let commit = self.ports.reads.commit_for_read(thread).await?;
        let executable = self
            .threads
            .executable_session_dispatch_runs(thread, &ThreadId(thread.to_owned()), &commit)
            .await?;
        let control = self.threads.durable_run_control()?;
        for run_id in executable {
            control
                .persist_recoverable_cancellation(
                    &run_id,
                    awaken_dispatch_execution::durable_control::RunResidence::Elsewhere,
                )
                .await?;
        }
        Ok(())
    }

    /// The model `thread` runs on: its Session's frozen selection, else the
    /// default.
    pub(crate) fn model_for_thread(&self, thread: &str) -> Result<String, RunError> {
        Ok(self.ports.projections.projection(thread)?.map_or_else(
            || self.ports.default_model.clone(),
            |projection| projection.baseline().execution_model_ref.clone(),
        ))
    }
}
