//! Coalesces Worker-observation changes into the canonical publication
//! reconciler. Heartbeats remain the retry clock; identical evidence does no
//! repeated authoring work after a successful reconcile.

use awaken_config_service::PublicationBindingReconciler;

pub(crate) struct WorkerObservationReconcileGate {
    last_reconciled: tokio::sync::Mutex<Option<String>>,
    source: std::sync::Arc<dyn awaken_coordinator::WorkerObservationSource>,
}

impl WorkerObservationReconcileGate {
    pub(crate) fn new(
        source: std::sync::Arc<dyn awaken_coordinator::WorkerObservationSource>,
    ) -> Self {
        Self {
            last_reconciled: tokio::sync::Mutex::new(None),
            source,
        }
    }

    pub(crate) async fn reconcile(
        &self,
        reconciler: &dyn PublicationBindingReconciler,
    ) -> Result<usize, String> {
        let fingerprint = current_worker_observation_fingerprint(self.source.as_ref()).await?;
        self.reconcile_fingerprint(fingerprint, reconciler).await
    }

    /// Split Control has no Worker heartbeat route in its process. Poll the
    /// authenticated Coordinator projection and feed changes into this same
    /// fingerprint/retry state machine; AllInOne continues to use heartbeat as
    /// its immediate clock.
    pub(crate) fn spawn_periodic(
        self: std::sync::Arc<Self>,
        reconciler: std::sync::Arc<dyn PublicationBindingReconciler>,
        period: std::time::Duration,
    ) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if let Err(error) = self.reconcile(reconciler.as_ref()).await {
                    eprintln!("Worker observation reconciliation remains pending: {error}");
                }
            }
        });
    }

    async fn reconcile_fingerprint(
        &self,
        fingerprint: String,
        reconciler: &dyn PublicationBindingReconciler,
    ) -> Result<usize, String> {
        // The lock coalesces concurrent Worker heartbeats. It deliberately spans
        // reconcile: only a successful publication refresh advances the fence,
        // so a failure is retried by the next heartbeat.
        let mut last = self.last_reconciled.lock().await;
        if last.as_deref() == Some(fingerprint.as_str()) {
            return Ok(0);
        }
        let changed = reconciler.reconcile_all().await?;
        *last = Some(fingerprint);
        Ok(changed)
    }
}

async fn current_worker_observation_fingerprint(
    source: &dyn awaken_coordinator::WorkerObservationSource,
) -> Result<String, String> {
    let mut workers = source.list().await.map_err(|error| error.to_string())?;
    workers.sort_by(|left, right| {
        (
            &left.snapshot.identity.worker_id,
            &left.snapshot.identity.incarnation_id,
        )
            .cmp(&(
                &right.snapshot.identity.worker_id,
                &right.snapshot.identity.incarnation_id,
            ))
    });
    let observations: Vec<_> = workers
        .into_iter()
        .map(|worker| {
            (
                worker.snapshot.identity.worker_id,
                worker.snapshot.identity.incarnation_id,
                worker.snapshot.credential_observations,
                worker.snapshot.acp_capability_observations,
            )
        })
        .collect();
    awaken_runtime_contract::content_fingerprint(&observations).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct FlakySource(AtomicBool);

    #[async_trait::async_trait]
    impl awaken_coordinator::WorkerObservationSource for FlakySource {
        async fn list(
            &self,
        ) -> Result<
            Vec<awaken_worker_contract::RegisteredWorker>,
            awaken_worker_contract::RegistryError,
        > {
            if self.0.swap(false, Ordering::SeqCst) {
                Err(awaken_worker_contract::RegistryError::Persistence(
                    "offline".into(),
                ))
            } else {
                Ok(Vec::new())
            }
        }
    }

    struct RecordingReconciler {
        calls: AtomicUsize,
        fail: AtomicBool,
    }

    #[async_trait::async_trait]
    impl PublicationBindingReconciler for RecordingReconciler {
        async fn reconcile(&self) -> Result<usize, String> {
            unreachable!("the Worker event uses the all-policy operation")
        }

        async fn reconcile_all(&self) -> Result<usize, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.swap(false, Ordering::SeqCst) {
                Err("transient".into())
            } else {
                Ok(2)
            }
        }
    }

    #[tokio::test]
    async fn changed_observations_coalesce_and_failed_reconcile_retries() {
        // Cause/effect decision table:
        // R1 new fingerprint + success -> reconcile once and fence it;
        // R2 same fingerprint          -> no second reconcile;
        // R3 changed fingerprint       -> reconcile again;
        // R4 reconcile failure         -> do not advance, next heartbeat retries.
        // These rules cover both fingerprint equality branches and both
        // reconciler terminal outcomes.
        let gate = WorkerObservationReconcileGate::new(awaken_coordinator::test_worker_directory());
        let reconciler = RecordingReconciler {
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        assert_eq!(
            gate.reconcile_fingerprint("a".into(), &reconciler)
                .await
                .unwrap(),
            2,
            "R1"
        );
        assert_eq!(
            gate.reconcile_fingerprint("a".into(), &reconciler)
                .await
                .unwrap(),
            0,
            "R2"
        );
        assert_eq!(
            gate.reconcile_fingerprint("b".into(), &reconciler)
                .await
                .unwrap(),
            2,
            "R3"
        );
        reconciler.fail.store(true, Ordering::SeqCst);
        assert!(
            gate.reconcile_fingerprint("c".into(), &reconciler)
                .await
                .is_err(),
            "R4 first attempt"
        );
        assert_eq!(
            gate.reconcile_fingerprint("c".into(), &reconciler)
                .await
                .unwrap(),
            2,
            "R4 retry"
        );
        assert_eq!(reconciler.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn observation_source_failure_retries_without_advancing_the_fence() {
        // Cause/effect decision table for split-Control polling:
        // P1 remote source unavailable -> no publication call and no fence;
        // P2 next poll succeeds -> reconcile and advance; P3 unchanged next
        // projection -> coalesce. Changed projections and reconciler failures
        // are the R3/R4 rules in the sibling gate test.
        let gate = WorkerObservationReconcileGate::new(std::sync::Arc::new(FlakySource(
            AtomicBool::new(true),
        )));
        let reconciler = RecordingReconciler {
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        assert!(gate.reconcile(&reconciler).await.is_err(), "P1");
        assert_eq!(reconciler.calls.load(Ordering::SeqCst), 0, "P1");
        assert_eq!(gate.reconcile(&reconciler).await.unwrap(), 2, "P2");
        assert_eq!(gate.reconcile(&reconciler).await.unwrap(), 0, "P3");
        assert_eq!(reconciler.calls.load(Ordering::SeqCst), 1, "P2+P3");
    }
}
