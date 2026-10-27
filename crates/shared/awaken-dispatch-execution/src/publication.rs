// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Immutable topology selection over the existing latency hint and canonical Outbox.
// @formal reason: Publication owns no claim, admission phase, pool handle, queue or mutable lifecycle.
//! Durable change publication without retaining the process's dispatch services.

use crate::{DispatchHint, durable_control::DurableRunControl};
use awaken_dispatch_contract::{DispatchError, Outbox};
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use std::sync::Arc;

/// The publication capability selected by the process's immutable topology.
/// Every publication first relays the original owner's durable outbox. Local
/// publication then hints the configured claimer, including before it starts.
/// The hint is never readiness or admission evidence. Dropping services does
/// not revoke durable commands, change routing, or create another drainer.
#[derive(Clone)]
pub struct DispatchPublication {
    claimer: Option<DispatchHint>,
}

impl DispatchPublication {
    pub(crate) fn selected(local_pool: bool, hint: DispatchHint) -> Self {
        Self {
            claimer: local_pool.then_some(hint),
        }
    }

    /// Nudge the configured local claimer within the existing hint budget.
    pub async fn hint_local_pool(&self) {
        if let Some(claimer) = &self.claimer {
            claimer.publish().await;
        }
    }

    /// Publish an already-staged durable change through its original owner.
    pub async fn publish_staged(
        &self,
        owner: &AnyCoordinatorDispatch,
    ) -> Result<(), DispatchError> {
        Outbox::relay(owner).await?;
        self.hint_local_pool().await;
        Ok(())
    }

    /// Existing durable control with the same construction-selected hint.
    #[must_use]
    pub fn durable_run_control(&self, owner: Arc<AnyCoordinatorDispatch>) -> DurableRunControl {
        DurableRunControl::new(owner, self.claimer.clone())
    }
}
