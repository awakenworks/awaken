// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: Session budget accounting. It reads committed usage and BudgetReached tickets
// @formal reason: and resumes a budget-paused Run through the canonical resume dispatch; the budget decision
// @formal reason: stays in the Session application. This is a product-requirement classification.
//! [`BudgetAccounting`] — Session usage and budget-paused Runs.
//!
//! It reads committed Thread usage, lists the BudgetReached tickets a Session
//! holds, and resumes a budget-paused Run through [`crate::SessionRuns`]. It
//! serves the Session application's `BudgetRuntime` role.

use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource as _;
use awaken_dispatch_contract::{
    DispatchReads as _, PendingInput, SessionContinuationOutbox as _,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_session_types::RunError;

use crate::HostError;
use crate::managed_adapter_error::to_run_error;
use crate::thread_reads::recovery_ticket;

/// Budget accounting for one process. Consumer: the Session application's
/// `BudgetRuntime` role.
pub struct BudgetAccounting {
    session_runs: Arc<crate::SessionRuns>,
    thread_reads: Arc<crate::ThreadReads>,
}

impl BudgetAccounting {
    pub(crate) fn new(session_runs: Arc<crate::SessionRuns>) -> Self {
        Self {
            thread_reads: session_runs.thread_reads.clone(),
            session_runs,
        }
    }

    pub(crate) async fn session_budget_resume_tickets(
        &self,
        session_id: &str,
    ) -> Result<Vec<awaken_session_application_contract::SessionBudgetResumeTicket>, HostError>
    {
        let parent = ThreadId(session_id.to_string());
        let mut thread_ids = vec![parent.clone()];
        let dispatch_rows = if let Some(store) = self.session_runs.optional_dispatch_store() {
            store
                .list_dispatches()
                .await
                .map_err(|error| HostError::internal(error.to_string()))?
        } else {
            Vec::new()
        };
        for thread_id in dispatch_rows
            .iter()
            // Queue phase is not pause truth: a Worker commits Awaiting
            // before it settles a Leased row. Every live parent-affined row
            // is therefore a candidate and committed recovery truth below
            // performs the sole BudgetReached classification.
            .filter(|row| row.session_thread_id.as_ref() == Some(&parent))
            .map(|row| row.thread_id.clone())
        {
            if !thread_ids.contains(&thread_id) {
                thread_ids.push(thread_id);
            }
        }
        let commit = self.thread_reads.commit_for_read(session_id).await?;
        let mut tickets = Vec::new();
        for thread_id in thread_ids {
            let Some(latest) = commit
                .authoritative_latest_run(&thread_id)
                .await
                .map_err(HostError::internal)?
            else {
                continue;
            };
            let snapshot = commit
                .recovery_snapshot(&thread_id, &latest.id)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            if let Some(ticket) = recovery_ticket(&snapshot, &latest.id)?
                && ticket.reason() == AwaitReason::BudgetReached
            {
                tickets.push(
                    awaken_session_application_contract::SessionBudgetResumeTicket {
                        ticket,
                        pause_generation: snapshot.next_commit_ordinal,
                        prior_session_activity_epoch: dispatch_rows
                            .iter()
                            .find(|row| row.run_id == latest.id)
                            .and_then(|row| row.session_activity_epoch),
                    },
                );
            }
        }
        Ok(tickets)
    }

    /// RunResume the run awaiting on `thread`, answering `tool_use_id` with `resume`.
    /// Fails closed unless `tool_use_id` names the pending tool and its binding
    /// (built-in vs client-executed) matches the resume variant.
    pub(crate) async fn resume_budget_reached(
        &self,
        delivery: awaken_session_application_contract::SessionBudgetResumeDelivery,
    ) -> Result<awaken_session_application_contract::SessionBudgetResumeDisposition, HostError>
    {
        if delivery.session_activity_epoch == 0 {
            return Err(HostError::bad_request(
                "budget resume requires a newly admitted Session activity",
            ));
        }
        let commit = self
            .thread_reads
            .commit_for_read(delivery.session_id.as_str())
            .await?;
        let snapshot = commit
            .recovery_snapshot(&delivery.thread_id, &delivery.run_id)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let Some(ticket) = recovery_ticket(&snapshot, &delivery.run_id)? else {
            return Ok(awaken_session_application_contract::SessionBudgetResumeDisposition::Stale);
        };
        if ticket.reason() != AwaitReason::BudgetReached
            || ticket.correlation_id != delivery.correlation_id
            || ticket.thread_id != delivery.thread_id
            || snapshot.next_commit_ordinal != delivery.pause_generation
        {
            return Ok(awaken_session_application_contract::SessionBudgetResumeDisposition::Stale);
        }

        let store = self.session_runs.optional_dispatch_store();
        let dispatch = match &store {
            Some(store) => store
                .list_dispatches()
                .await
                .map_err(|error| HostError::internal(error.to_string()))?
                .into_iter()
                .find(|row| row.run_id == delivery.run_id),
            None => None,
        };
        let store = if let Some(dispatch) = dispatch {
            if dispatch.thread_id != delivery.thread_id
                || dispatch.session_thread_id.as_ref()
                    != Some(&ThreadId(delivery.session_id.to_string()))
            {
                return Err(HostError::bad_request(
                    "budget resume dispatch affinity is inconsistent",
                ));
            }
            let activity_matches = match dispatch.session_activity_epoch {
                Some(epoch) => {
                    Some(epoch) == delivery.prior_session_activity_epoch
                        || epoch == delivery.session_activity_epoch
                }
                None => delivery.prior_session_activity_epoch.is_none(),
            };
            if !activity_matches {
                return Ok(
                    awaken_session_application_contract::SessionBudgetResumeDisposition::Stale,
                );
            }
            store
                .as_ref()
                .expect("dispatch was read from the installed store")
                .clone()
        } else {
            if delivery.thread_id.0 != delivery.session_id
                || delivery.prior_session_activity_epoch.is_some()
            {
                return Err(HostError::internal(
                    "budget-paused child Run has no durable dispatch row",
                ));
            }
            self.session_runs
                .enqueue_foreground_session_resume_dispatch(
                    delivery.session_id.as_str(),
                    &ticket,
                    delivery.session_activity_epoch,
                )
                .await?
        };
        // Cause/effect decision table for foreground→durable handoff: a crash
        // before enqueue leaves the committed ticket discoverable; a crash
        // after enqueue but before staging leaves an idempotently reusable row;
        // a Worker that claims first observes the committed Awaiting Run and
        // cannot execute it as fresh work. Once staged, the existing claim and
        // settlement observer owns Completed, required action, and a later
        // budget pause, including crash repair of the Session activity epoch.
        let input = PendingInput {
            message_id: format!(
                "budget-resume-{}",
                awaken_agent_contract::stable_fingerprint(&(
                    delivery.session_id.as_str(),
                    delivery.thread_id.0.as_str(),
                    delivery.run_id.0.as_str(),
                    delivery.correlation_id.as_str(),
                    delivery.pause_generation,
                ))
            ),
            run_id: delivery.run_id,
            thread_id: delivery.thread_id,
            correlation_id: delivery.correlation_id,
            available_at_ms: None,
            context_messages: Vec::new(),
            result: ResumeResult::Continue,
        };
        store
            .stage_session_resume(
                input,
                &ThreadId(delivery.session_id.into_string()),
                delivery.prior_session_activity_epoch,
                Some(delivery.session_activity_epoch),
            )
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        self.session_runs
            .dispatch
            .publish_staged(&store)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(awaken_session_application_contract::SessionBudgetResumeDisposition::Dispatched)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::BudgetRuntime for BudgetAccounting {
    async fn resume_budget_reached(
        &self,
        delivery: awaken_session_application_contract::SessionBudgetResumeDelivery,
    ) -> Result<awaken_session_application_contract::SessionBudgetResumeDisposition, RunError> {
        self.resume_budget_reached(delivery)
            .await
            .map_err(to_run_error)
    }

    async fn session_budget_resume_tickets(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<Vec<awaken_session_application_contract::SessionBudgetResumeTicket>, RunError> {
        self.session_budget_resume_tickets(session_id.as_str())
            .await
            .map_err(to_run_error)
    }

    async fn session_usage(
        &self,
        thread: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        self.thread_reads
            .session_thread_usage(
                thread,
                &awaken_agent_contract::agent::thread::Id(thread.to_string()),
            )
            .await
            .map_err(to_run_error)
    }

    async fn session_thread_usage(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        self.thread_reads
            .session_thread_usage(
                session_id.as_str(),
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            )
            .await
            .map_err(to_run_error)
    }
}
