// @formal surface obligation=run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal reason: Control records intent through the original Dispatch owner before nudging the
// @formal reason: existing pool and Runtime. Awaiting tickets remain committed Thread truth.
//! Durable control over the canonical Dispatch owner, without Session creation:
//! cancellation, recoverable cancellation and staged resume inputs. Run control
//! of a resident Session (wake, pause, resume input authoring) is the executor's.
use std::sync::Arc;

use crate::AdmissionError;
use crate::DispatchPool;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch_erasure::{AnyCoordinatorDispatch, AnyDispatchStore};
use awaken_dispatch_contract::{DispatchAdministration, Outbox, PendingInput};
use awaken_runtime_contract::control::{LiveCommand, LiveRunControl};

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
        live: Option<&(dyn LiveRunControl + Send + Sync)>,
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
        live: Option<&(dyn LiveRunControl + Send + Sync)>,
    ) -> Result<bool, AdmissionError> {
        self.owner
            .requeue(run)
            .await
            .map_err(AdmissionError::from)?;
        self.persist_cancellation(run, live).await
    }
    pub async fn cancel(
        &self,
        run: &RunId,
        live: Option<&(dyn LiveRunControl + Send + Sync)>,
    ) -> Result<(), AdmissionError> {
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
