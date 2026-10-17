// @formal surface obligation=run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal reason: Control records intent through the original Dispatch owner before nudging the
// @formal reason: existing pool and Runtime. Awaiting tickets remain committed Thread truth.
//! Durable control over the canonical Dispatch owner, without Session creation.
use std::sync::Arc;

use crate::AdmissionError;
use crate::{DispatchPool, LiveAttemptControl, LiveRunControlError};
use awaken_agent_contract::agent::{
    awaiting::{AwaitReason, ResumeTicket},
    run::Id as RunId,
    thread::Id as ThreadId,
};
use awaken_dispatch_erasure::{AnyCoordinatorDispatch, AnyDispatchStore};
use awaken_dispatch_contract::{DispatchAdministration, Outbox, PendingInput};
use awaken_runtime::Runtime;
use awaken_runtime_contract::{
    control::{LiveCommand, LiveRunControl},
    resume::ResumeResult,
};

/// The existing durable cancellation ordering, shared by all control callers.
pub struct DurableRunControl {
    owner: Arc<AnyCoordinatorDispatch>,
    pool: Option<Arc<DispatchPool<AnyDispatchStore>>>,
}
impl DurableRunControl {
    pub fn new(
        owner: Arc<AnyCoordinatorDispatch>,
        pool: Option<Arc<DispatchPool<AnyDispatchStore>>>,
    ) -> Self {
        Self { owner, pool }
    }

    pub async fn persist_cancellation(
        &self,
        run: &RunId,
        live: Option<&Runtime>,
    ) -> Result<bool, AdmissionError> {
        let accepted = self
            .owner
            .cancel(run)
            .await
            .map_err(AdmissionError::from)?
            .is_some();
        if accepted && let Some(pool) = &self.pool {
            pool.notify().await;
        }
        if accepted && let Some(runtime) = live {
            let _ = runtime.deliver(LiveCommand::Cancel {
                run_id: run.clone(),
            });
        }
        Ok(accepted)
    }
    pub async fn persist_recoverable_cancellation(
        &self,
        run: &RunId,
        live: Option<&Runtime>,
    ) -> Result<bool, AdmissionError> {
        self.owner
            .requeue(run)
            .await
            .map_err(AdmissionError::from)?;
        self.persist_cancellation(run, live).await
    }
    pub async fn cancel(&self, run: &RunId, live: Option<&Runtime>) -> Result<(), AdmissionError> {
        if self.persist_cancellation(run, live).await? {
            Ok(())
        } else {
            Err(AdmissionError::rejected(format!(
                "run not found: {}",
                run.0
            )))
        }
    }
    pub async fn stage_input(&self, input: PendingInput) -> Result<String, AdmissionError> {
        let run = input.run_id.0.clone();
        self.stage(input).await?;
        Ok(run)
    }
    async fn stage(&self, input: PendingInput) -> Result<(), AdmissionError> {
        self.owner
            .stage(input)
            .await
            .map_err(AdmissionError::from)?;
        if let Some(pool) = &self.pool {
            pool.notify().await;
        }
        Ok(())
    }
}

pub async fn wake_resident(runtime: Option<Arc<Runtime>>, run: &str) -> Result<(), AdmissionError> {
    let runtime = runtime
        .ok_or_else(|| AdmissionError::from(LiveRunControlError::NoSubscriber(run.to_owned())))?;
    LiveAttemptControl::new(runtime)
        .wake(run)
        .await
        .map_err(AdmissionError::from)
}

pub async fn pause_resident(
    runtime: Option<Arc<Runtime>>,
    worker_runtime: Option<Arc<Runtime>>,
    thread: &ThreadId,
    durable: bool,
    requested: Option<&str>,
) -> Result<String, AdmissionError> {
    let no_active = || AdmissionError::rejected("thread has no locally owned active run");
    let runtime = runtime.ok_or_else(|| {
        requested.map_or_else(no_active, |run| {
            AdmissionError::from(LiveRunControlError::NoSubscriber(run.to_owned()))
        })
    })?;
    let run = match requested {
        Some(run) => run.to_owned(),
        None => runtime
            .active_attempt_run_id(thread)
            .await
            .map(|run| run.0)
            .ok_or_else(no_active)?,
    };
    if !durable {
        return Err(AdmissionError::rejected("pause requires durable delivery"));
    }
    LiveAttemptControl::new(
        worker_runtime
            .ok_or_else(|| AdmissionError::rejected("pause requires durable delivery"))?,
    )
    .pause(&run)
    .await
    .map_err(AdmissionError::from)?;
    Ok(run)
}

pub fn decision_input(
    thread: ThreadId,
    awaiting: Option<(RunId, ResumeTicket)>,
    allow: bool,
) -> Result<PendingInput, AdmissionError> {
    let (run_id, ticket) = awaiting
        .ok_or_else(|| AdmissionError::rejected("no awaiting run on this thread to deliver to"))?;
    Ok(PendingInput {
        message_id: awaken_runtime::fresh_process_id("xthread"),
        run_id,
        thread_id: thread,
        correlation_id: ticket.correlation_id,
        available_at_ms: None,
        context_messages: Vec::new(),
        result: if allow {
            ResumeResult::allow()
        } else {
            ResumeResult::deny(None)
        },
    })
}
pub fn manual_resume_input(
    thread: ThreadId,
    awaiting: Option<(RunId, ResumeTicket)>,
    text: String,
) -> Result<PendingInput, AdmissionError> {
    let (run_id, ticket) = awaiting
        .ok_or_else(|| AdmissionError::rejected("no awaiting run on this thread to resume"))?;
    if ticket.reason() != AwaitReason::ManualPause {
        return Err(AdmissionError::rejected(
            "durable text resume requires a manual-pause ticket",
        ));
    }
    Ok(PendingInput {
        message_id: awaken_runtime::fresh_process_id("manual-resume"),
        run_id,
        thread_id: thread,
        correlation_id: ticket.correlation_id,
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input(text),
    })
}
