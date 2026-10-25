// @formal surface obligation=run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal reason: The Coordinator records every control intent through the dispatch owner a Worker's claim is
// @formal reason: fenced by: cancellation reaches the owning Worker through its claimed row, an unknown Run is
// @formal reason: refused, and no attempt is controlled locally. Classification, not a formal-proof claim.
//! The durable-ingress operations of a Coordinator whose Runs registered
//! Workers execute.
//!
//! The Coordinator owns no attempt. Control reaches the Worker that owns a
//! Run through the Run's dispatch row: a cancellation is the row's durable
//! intent, which fences the owning claim and makes that Worker commit the
//! cancelled boundary; a resume input is staged on the row the Worker claims.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::{
    DispatchAdministration as _, DispatchMaintenance as _, DispatchReads as _, DispatchState,
    DispatchSummary,
};
use awaken_dispatch_execution::durable_control::DurableRunControl;
use awaken_dispatch_execution::{AdmissionError, LiveRunControlError};
use awaken_session_execution_contract::{
    ApplicationError, DurableDispatchStatus, DurableRunOperations,
};
use awaken_session_types::RunError;

use super::WorkerPlacedSessions;
use crate::{application_error, decision_input, manual_resume_input};

fn refused(error: AdmissionError) -> ApplicationError {
    application_error(RunError::from(error))
}

fn store_failure(error: impl std::fmt::Display) -> ApplicationError {
    application_error(RunError::internal(error.to_string()))
}

impl WorkerPlacedSessions {
    /// Control over the dispatch owner; the Coordinator runs no claimer to
    /// nudge.
    fn durable_control(&self) -> DurableRunControl {
        DurableRunControl::new(self.ports.owner.clone(), None)
    }

    async fn dispatch_rows(&self, thread: &str) -> Result<Vec<DispatchSummary>, ApplicationError> {
        Ok(self
            .ports
            .owner
            .list_dispatches()
            .await
            .map_err(store_failure)?
            .into_iter()
            .filter(|row| row.thread_id.0 == thread)
            .collect())
    }

    async fn awaiting(
        &self,
        thread: &str,
    ) -> Result<
        Option<(RunId, awaken_agent_contract::agent::awaiting::ResumeTicket)>,
        ApplicationError,
    > {
        self.ports
            .reads
            .commit_for_read(thread)
            .await
            .map_err(application_error)?
            .open_wait_for_thread(&ThreadId(thread.to_owned()))
            .await
            .map_err(store_failure)
    }
}

/// Decision table (operation → effect on a Coordinator):
/// | Rule | operation | effect |
/// |---|---|---|
/// | O1 | cancel a Run with a live dispatch row | its durable cancellation; the owning Worker commits the cancelled boundary |
/// | O2 | cancel an unknown or settled Run | refused: no owner can be reached |
/// | O3 | pause / wake | refused: the Coordinator owns no attempt |
/// | O4 | resume / deliver to an awaiting Run | the answer staged on its row |
/// | O5 | resume / deliver with nothing awaiting | refused |
/// | O6 | reconcile | refused: committed terminals are reconciled by the Coordinator's loop |
#[async_trait::async_trait]
impl DurableRunOperations for WorkerPlacedSessions {
    async fn cancel(&self, _thread: &str, run_id: &str) -> Result<(), ApplicationError> {
        self.durable_control()
            .cancel(&RunId(run_id.to_owned()), None)
            .await
            .map_err(refused)
    }

    async fn pause(&self, _thread: &str, run_id: Option<&str>) -> Result<String, ApplicationError> {
        Err(refused(match run_id {
            Some(run) => AdmissionError::from(LiveRunControlError::NoSubscriber(run.to_owned())),
            None => AdmissionError::rejected("thread has no locally owned active run"),
        }))
    }

    async fn resume(&self, thread: &str, text: String) -> Result<String, ApplicationError> {
        let input = manual_resume_input(
            ThreadId(thread.to_owned()),
            self.awaiting(thread).await?,
            text,
        )
        .map_err(refused)?;
        self.durable_control()
            .stage_input(input)
            .await
            .map_err(refused)
    }

    async fn wake(&self, _thread: &str, run_id: &str) -> Result<(), ApplicationError> {
        Err(refused(AdmissionError::from(
            LiveRunControlError::NoSubscriber(run_id.to_owned()),
        )))
    }

    async fn deliver(&self, thread: &str, allow: bool) -> Result<String, ApplicationError> {
        let input = decision_input(
            ThreadId(thread.to_owned()),
            self.awaiting(thread).await?,
            allow,
        )
        .map_err(refused)?;
        self.durable_control()
            .stage_input(input)
            .await
            .map_err(refused)
    }

    async fn messages(&self, thread: &str) -> Result<Vec<Message>, ApplicationError> {
        self.ports
            .reads
            .committed_messages(thread)
            .await
            .map_err(application_error)
    }

    async fn superseded(&self, thread: &str) -> Result<Vec<String>, ApplicationError> {
        Ok(self
            .dispatch_rows(thread)
            .await?
            .into_iter()
            .filter(|row| row.state == DispatchState::Superseded)
            .map(|row| row.run_id.0)
            .collect())
    }

    async fn dispatches(
        &self,
        thread: &str,
    ) -> Result<Vec<DurableDispatchStatus>, ApplicationError> {
        Ok(self
            .dispatch_rows(thread)
            .await?
            .into_iter()
            .map(|row| DurableDispatchStatus {
                run_id: row.run_id.0,
                status: format!("{:?}", row.state),
                attempts: row.attempt_count,
                sandbox_bound: row.sandbox_bound,
            })
            .collect())
    }

    async fn reconcile(&self, _thread: &str) -> Result<Vec<String>, ApplicationError> {
        Err(refused(AdmissionError::rejected(
            "a Coordinator executes no Run: its terminal reconciliation loop settles committed terminals",
        )))
    }

    async fn quarantine_retry_exhausted(
        &self,
        _thread: &str,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, ApplicationError> {
        self.ports
            .owner
            .quarantine_retry_exhausted(max_attempts, now_ms)
            .await
            .map_err(store_failure)
    }

    async fn dead_letters(&self, thread: &str) -> Result<Vec<String>, ApplicationError> {
        Ok(self
            .dispatch_rows(thread)
            .await?
            .into_iter()
            .filter(|row| row.state == DispatchState::DeadLetter)
            .map(|row| row.run_id.0)
            .collect())
    }

    async fn requeue_dead_letter(
        &self,
        thread: &str,
        run_id: &str,
    ) -> Result<bool, ApplicationError> {
        if !self
            .dispatch_rows(thread)
            .await?
            .iter()
            .any(|row| row.run_id.0 == run_id && row.state == DispatchState::DeadLetter)
        {
            return Ok(false);
        }
        self.ports
            .owner
            .requeue(&RunId(run_id.to_owned()))
            .await
            .map_err(store_failure)
    }

    async fn purge_dead_letters(&self, _thread: &str) -> Result<usize, ApplicationError> {
        self.ports
            .owner
            .purge_dead_letters()
            .await
            .map_err(store_failure)
    }
}
