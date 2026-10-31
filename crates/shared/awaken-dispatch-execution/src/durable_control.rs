// @formal surface obligation=run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal reason: Control records intent through the original Dispatch owner before nudging the
// @formal reason: existing pool and Runtime. Awaiting tickets remain committed Thread truth.
//! Durable control over the canonical Dispatch owner, without Session creation:
//! cancellation, recoverable cancellation and staged resume inputs. Run control
//! of a resident Session (wake, pause, resume input authoring) is the executor's.
use std::sync::Arc;

use crate::AdmissionError;
use crate::DispatchHint;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch_contract::{DispatchAdministration, Outbox, PendingInput};
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_runtime_contract::control::{LiveCommand, LiveRunControl};

/// Where the cancelled Run's attempt executes, as seen by the process that
/// cancels it. The caller states it; durable cancellation never discovers it.
#[derive(Clone, Copy)]
pub enum RunResidence<'a> {
    /// The attempt runs in this process: after the durable intent is accepted,
    /// its live control receives the cancellation as a local accelerator.
    Resident(&'a (dyn LiveRunControl + Send + Sync)),
    /// The attempt is cold, remote or not started: the durable intent alone
    /// cancels it through the claim path.
    Elsewhere,
}

/// The existing durable cancellation ordering, shared by all control callers.
///
/// Decision table of [`DurableRunControl::persist_cancellation`] (tests
/// `durable_control::tests`):
/// | Rule | owner accepts the intent | residence | Effect |
/// |---|---|---|---|
/// | C1 | yes | `Resident` | true; the live control receives `Cancel` |
/// | C2 | yes | `Elsewhere` | true; no live delivery |
/// | C3 | no (unknown Run) | any | false; no live delivery |
pub struct DurableRunControl {
    owner: Arc<AnyCoordinatorDispatch>,
    /// The local claimer's hint, when this process runs one.
    claimer: Option<DispatchHint>,
}
impl DurableRunControl {
    pub fn new(owner: Arc<AnyCoordinatorDispatch>, claimer: Option<DispatchHint>) -> Self {
        Self { owner, claimer }
    }

    pub async fn persist_cancellation(
        &self,
        run: &RunId,
        residence: RunResidence<'_>,
    ) -> Result<bool, AdmissionError> {
        let accepted = self
            .owner
            .cancel(run)
            .await
            .map_err(AdmissionError::from)?
            .is_some();
        if accepted && let Some(claimer) = &self.claimer {
            claimer.publish().await;
        }
        if accepted && let RunResidence::Resident(runtime) = residence {
            let _ = runtime.deliver(LiveCommand::Cancel {
                run_id: run.clone(),
            });
        }
        Ok(accepted)
    }
    pub async fn persist_recoverable_cancellation(
        &self,
        run: &RunId,
        residence: RunResidence<'_>,
    ) -> Result<bool, AdmissionError> {
        self.owner
            .requeue(run)
            .await
            .map_err(AdmissionError::from)?;
        self.persist_cancellation(run, residence).await
    }
    pub async fn cancel(
        &self,
        run: &RunId,
        residence: RunResidence<'_>,
    ) -> Result<(), AdmissionError> {
        if self.persist_cancellation(run, residence).await? {
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
        if let Some(claimer) = &self.claimer {
            claimer.publish().await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_dispatch::MemoryDispatchStore;
    use awaken_dispatch_contract::{DispatchEnqueue as _, RunDispatch};
    use awaken_runtime_contract::control::Error as LiveControlError;
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts the `Cancel` commands a resident attempt receives.
    #[derive(Default)]
    struct CountingLiveControl(AtomicUsize);

    impl LiveRunControl for CountingLiveControl {
        fn deliver(&self, command: LiveCommand) -> Result<(), LiveControlError> {
            if matches!(command, LiveCommand::Cancel { .. }) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    async fn control_with_queued(run: &str) -> DurableRunControl {
        let store = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
        store
            .enqueue(RunDispatch::new(
                awaken_runtime_contract::activation::RunActivation::new(
                    RunId(run.into()),
                    awaken_agent_contract::agent::thread::Id("durable-control".into()),
                    ExecutableAgentSnapshot::builder("durable-control")
                        .model(
                            ModelBinding::parse("provider", "model", "backend")
                                .expect("model binding"),
                        )
                        .build(),
                    Vec::new(),
                ),
            ))
            .await
            .expect("enqueue");
        DurableRunControl::new(
            Arc::new(AnyCoordinatorDispatch::from_durable_dispatch(store)),
            None,
        )
    }

    #[tokio::test]
    async fn the_live_accelerator_follows_an_accepted_intent_only_for_a_resident_attempt() {
        let live = CountingLiveControl::default();

        let control = control_with_queued("resident").await;
        let accepted = control
            .persist_cancellation(&RunId("resident".into()), RunResidence::Resident(&live))
            .await
            .expect("C1 durable intent");
        assert!(accepted, "C1 the owner accepts the intent");
        assert_eq!(
            live.0.load(Ordering::SeqCst),
            1,
            "C1 the resident attempt is signalled"
        );

        let control = control_with_queued("elsewhere").await;
        let accepted = control
            .persist_cancellation(&RunId("elsewhere".into()), RunResidence::Elsewhere)
            .await
            .expect("C2 durable intent");
        assert!(accepted, "C2 the owner accepts the intent");
        assert_eq!(live.0.load(Ordering::SeqCst), 1, "C2 no live delivery");

        let control = control_with_queued("queued").await;
        let accepted = control
            .persist_cancellation(&RunId("unknown".into()), RunResidence::Resident(&live))
            .await
            .expect("C3 an unknown Run is not an error here");
        assert!(!accepted, "C3 the owner has no such Run");
        assert_eq!(live.0.load(Ordering::SeqCst), 1, "C3 no live delivery");
    }
}
