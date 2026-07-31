//! Coalesces Worker-observation changes into the canonical publication
//! reconciler. Heartbeats remain the retry clock; identical evidence does no
//! repeated authoring work after a successful reconcile.

use awaken_config_service::PublicationBindingReconciler;

#[derive(Default)]
pub(crate) struct WorkerObservationReconcileGate {
    last_reconciled: tokio::sync::Mutex<Option<String>>,
}

impl WorkerObservationReconcileGate {
    pub(crate) async fn reconcile(
        &self,
        reconciler: &dyn PublicationBindingReconciler,
    ) -> Result<usize, String> {
        let fingerprint = current_worker_observation_fingerprint().await?;
        self.reconcile_fingerprint(fingerprint, reconciler).await
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

async fn current_worker_observation_fingerprint() -> Result<String, String> {
    let mut workers = awaken_server::worker_directory()
        .list()
        .await
        .map_err(|error| error.to_string())?;
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
        let gate = WorkerObservationReconcileGate::default();
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
}
