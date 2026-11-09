// @formal evidence session_execution.one_physical_attempt_per_thread
#[cfg(test)]
mod tests {
    use super::super::*;
    use awaken_dispatch_contract::{
        DispatchAttemptControl, DispatchClaims, DispatchControlReads, DispatchEnqueue,
    };
    use awaken_dispatch_testkit::worker_http::activation;
    use awaken_lease_testkit::ManualClock;
    use awaken_worker_contract::{
        WorkerCurrentSource, WorkerDirectory, WorkerHeartbeat, WorkerManifest, WorkerRegistration,
    };
    use awaken_worker_registry::SqliteWorkerDirectory;

    struct Fixture {
        _directory: tempfile::TempDir,
        store: SqliteDispatchStore,
        registry: SqliteWorkerDirectory,
        identity: crate::WorkerIdentity,
        claim: RunClaim,
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let database = directory.path().join("process.db");
            let clock = Arc::new(ManualClock::new(10_000_000));
            let store = SqliteDispatchStore::open(database.to_str().unwrap(), clock)
                .await
                .unwrap();
            let registry = SqliteWorkerDirectory::open(&database).await.unwrap();
            let mut manifest = WorkerManifest {
                runtime_protocol: awaken_worker_contract::VersionRange::exact(
                    awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
                ),
                ..WorkerManifest::default()
            };
            manifest.capabilities.extend([
                awaken_worker_contract::PREPARED_SESSION_DISPATCH_CAPABILITY.to_owned(),
                awaken_worker_contract::TOOL_RESULT_JOURNAL_CAPABILITY.to_owned(),
            ]);
            let identity = registry
                .register(
                    WorkerRegistration {
                        worker_id: "worker".into(),
                        incarnation_id: "boot".into(),
                        manifest,
                        create_ledger: None,
                    },
                    10_000_000,
                    60_000,
                )
                .await
                .unwrap()
                .snapshot
                .identity;
            registry
                .heartbeat(
                    &identity,
                    WorkerHeartbeat {
                        sequence: 1,
                        ready: true,
                        in_flight: 0,
                        warm_environment_shapes: Default::default(),
                        credential_observations: Default::default(),
                        acp_capability_observations: Default::default(),
                        source_retention: Default::default(),
                    },
                    10_000_000,
                    60_000,
                )
                .await
                .unwrap();
            let snapshot = registry.current("worker").await.unwrap().unwrap().snapshot;
            let request = RunDispatch::new(activation("shared-admission"))
                .with_placement(crate::PlacementRequirements::remote_required())
                .with_worker_codec_requirements();
            let id = request.run_id().clone();
            store.enqueue(request).await.unwrap();
            let claim = RunClaim::from(
                &store
                    .claim_run_compatible(&id, &snapshot, 60_000)
                    .await
                    .unwrap()
                    .unwrap()
                    .lease,
            );
            Self {
                _directory: directory,
                store,
                registry,
                identity,
                claim,
            }
        }

        async fn operations(&self) -> i64 {
            sqlx::query_scalar("SELECT COUNT(*) FROM runtime_dispatch_operation")
                .fetch_one(&self.store.pool)
                .await
                .unwrap()
        }

        async fn guard(&self) -> awaken_worker_contract::WorkerIncarnationGuard {
            self.registry
                .lock_incarnation(&self.identity)
                .await
                .unwrap()
                .unwrap()
        }
    }

    #[tokio::test]
    async fn shared_database_busy_dispatch_exclusion_rolls_back_without_waiting_or_effects() {
        let f = Fixture::new().await;
        let original_operations = f.operations().await;
        let guard = f.guard().await;
        let exclusion = f.store.authority.lock().await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            f.store.admit_attempt_guarded(&f.claim, None, guard, false),
        )
        .await
        .expect("admission must not await a mutex behind its own database writer");
        assert!(
            matches!(result, Err(DispatchError::Unavailable(_))),
            "{result:?}"
        );
        drop(exclusion);
        assert!(
            f.store
                .load_physical_attempt(&f.claim.run_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(f.operations().await, original_operations);
        // A new guard and the same original claim succeed after the rollback.
        assert_eq!(
            f.store
                .admit_attempt_guarded(&f.claim, None, f.guard().await, false)
                .await
                .unwrap(),
            AttemptAdmission::Applied
        );
        let admitted_operations = f.operations().await;
        assert_eq!(admitted_operations, original_operations + 1);
        // Lost acknowledgement: repeat the original claim, never install twice.
        assert_eq!(
            f.store
                .admit_attempt_guarded(&f.claim, None, f.guard().await, false)
                .await
                .unwrap(),
            AttemptAdmission::AlreadyApplied
        );
        assert_eq!(f.operations().await, admitted_operations);
    }

    #[tokio::test]
    async fn shared_database_slot_failure_rolls_back_operation_and_releases_original_registry() {
        let f = Fixture::new().await;
        let original_operations = f.operations().await;
        sqlx::query("CREATE TRIGGER fail_slot BEFORE UPDATE OF active_attempt_owner ON runtime_dispatch BEGIN SELECT RAISE(ABORT, 'injected slot failure'); END")
            .execute(&f.store.pool).await.unwrap();
        let result = f
            .store
            .admit_attempt_guarded(&f.claim, None, f.guard().await, false)
            .await;
        assert!(
            matches!(result, Err(DispatchError::Unavailable(_))),
            "{result:?}"
        );
        assert!(
            f.store
                .load_physical_attempt(&f.claim.run_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(f.operations().await, original_operations);
        sqlx::query("DROP TRIGGER fail_slot")
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            f.store
                .admit_attempt_guarded(&f.claim, None, f.guard().await, false)
                .await
                .unwrap(),
            AttemptAdmission::Applied
        );
        assert_eq!(f.operations().await, original_operations + 1);
    }
}
