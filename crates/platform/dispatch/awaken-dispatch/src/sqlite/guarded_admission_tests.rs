// @formal evidence session_execution.one_physical_attempt_per_thread
#[cfg(test)]
mod tests {
    use super::super::*;
    use awaken_dispatch_contract::{
        DispatchAttemptControl, DispatchClaims, DispatchCommitAuthority, DispatchControlReads,
        DispatchEnqueue, DispatchExecution,
    };
    use awaken_dispatch_testkit::worker_http::activation;
    use awaken_lease_testkit::ManualClock;
    use awaken_worker_contract::{
        WorkerCurrentSource, WorkerDirectory, WorkerHeartbeat, WorkerManifest, WorkerRegistration,
    };
    use awaken_worker_registry::SqliteWorkerDirectory;

    struct Fixture {
        directory: tempfile::TempDir,
        store: SqliteDispatchStore,
        registry: SqliteWorkerDirectory,
        identity: crate::WorkerIdentity,
        claim: RunClaim,
        clock: Arc<ManualClock>,
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let database = directory.path().join("process.db");
            let clock = Arc::new(ManualClock::new(10_000_000));
            let store = SqliteDispatchStore::open(database.to_str().unwrap(), clock.clone())
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
                directory,
                store,
                registry,
                identity,
                claim,
                clock,
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

    #[tokio::test]
    async fn shared_database_commit_guard_and_registry_writer_allow_sdk_sql_after_busy_rollback() {
        // L1: the real D commit guard precedes the original R writer. A real
        // same-file SDK commit is Pending behind that writer. Admission must
        // return Busy and roll back R, allowing SDK SQL to finish while D stays
        // held. Awaiting D from R would deadlock both operations. L2: releasing
        // D then admits the SAME claim exactly once. This is the original 5s
        // progress bound, not the retired shared-resource lock protocol.
        use awaken_agent_contract::thread::commit::{
            coordinator::Coordinator as _,
            staged::{RunDisposition, ThreadCommit},
        };
        use std::future::Future as _;

        let f = Fixture::new().await;
        let commits = awaken_store_sqlite::SqliteCommitCoordinator::open(
            f.directory.path().join("process.db").to_str().unwrap(),
        )
        .await
        .unwrap();
        let original_operations = f.operations().await;
        let held = f.store.lock_commit_epoch(&f.claim).await.unwrap().unwrap();
        let thread = held.observation().request.thread_id().clone();
        let guard = f.guard().await;
        let mut writing = Box::pin(commits.commit(ThreadCommit::assemble(
            thread.clone(),
            RunDisposition::running(f.claim.run_id.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )));
        std::future::poll_fn(|cx| {
            assert!(writing.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let (admission, written) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                f.store.admit_attempt_guarded(&f.claim, None, guard, false),
                writing
            )
        })
        .await
        .expect("Busy admission must free its SQL writer for the guarded SDK commit");
        assert!(matches!(admission, Err(DispatchError::Unavailable(_))));
        assert_eq!(written.unwrap().sequence, 1);
        held.release().await.unwrap();
        assert_eq!(f.operations().await, original_operations);
        assert!(
            f.store
                .load_physical_attempt(&f.claim.run_id)
                .await
                .unwrap()
                .is_none()
        );
        let durable_commits: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM runtime_commit WHERE thread_id = ?1 AND run_id = ?2",
        )
        .bind(&thread.0)
        .bind(&f.claim.run_id.0)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
        assert_eq!(durable_commits, 1);
        assert_eq!(
            f.store
                .admit_attempt_guarded(&f.claim, None, f.guard().await, false)
                .await
                .unwrap(),
            AttemptAdmission::Applied
        );
        assert_eq!(f.operations().await, original_operations + 1);
    }

    #[tokio::test]
    async fn shared_database_rejected_identity_and_stale_claim_release_without_effects() {
        // I1 foreign claim owner / I2 foreign binding => the canonical identity
        // diagnostic, before SQL slot mutation; I3 matching incarnation but a
        // stale epoch => Fenced. All rows leave BOTH slot and operation table
        // unchanged, release R, and permit the original claim to succeed.
        // The standing × binding table separately covers all six time states.
        for rule in ["foreign-owner", "foreign-binding", "stale-epoch"] {
            let f = Fixture::new().await;
            let original_operations = f.operations().await;
            let mut attempted = f.claim.clone();
            if rule == "foreign-owner" {
                attempted.owner = "another-worker:1:boot".into();
            } else if rule == "stale-epoch" {
                attempted.epoch =
                    awaken_lease_contract::fence::Epoch::from_persisted(attempted.epoch)
                        .and_then(awaken_lease_contract::fence::Epoch::next)
                        .unwrap()
                        .get();
            }
            let binding = (rule == "foreign-binding").then(|| {
                AttemptExecutionBinding::new(
                    crate::WorkerIdentity::new("worker", "another-boot", 1),
                    "provider",
                    "execution",
                )
                .unwrap()
            });
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                f.store
                    .admit_attempt_guarded(&attempted, binding.as_ref(), f.guard().await, false),
            )
            .await
            .expect("rejected admission must release its original transaction");
            if rule == "stale-epoch" {
                assert_eq!(result.unwrap(), AttemptAdmission::Fenced);
            } else {
                assert!(
                    matches!(result, Err(DispatchError::Rejected(ref message))
                    if message == "Registry guard does not match the physical attempt"),
                    "{rule}: {result:?}"
                );
            }
            assert!(
                f.store
                    .load_physical_attempt(&f.claim.run_id)
                    .await
                    .unwrap()
                    .is_none(),
                "{rule}"
            );
            assert_eq!(f.operations().await, original_operations, "{rule}");
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    f.store
                        .admit_attempt_guarded(&f.claim, None, f.guard().await, false)
                        .await
                })
                .await
                .unwrap()
                .unwrap(),
                AttemptAdmission::Applied,
                "{rule}"
            );
            assert_eq!(f.operations().await, original_operations + 1, "{rule}");
        }
    }

    #[tokio::test]
    async fn shared_database_handoff_replaces_only_predecessor_and_preserves_retry_binding() {
        // H1 actual lease expiry + same-incarnation reclaim => a higher claim
        // epoch, while the old physical slot remains. Ordinary begin is Blocked;
        // H2 explicit guarded handoff returns Replaced: the new attempt inherits
        // predecessor responsibility exactly once;
        // H3 old finish is Fenced; H4 late None -> Some binding backfill conflicts
        // without another operation; exact None replay stays AlreadyApplied.
        // No executor runs here: this is port wiring, not physical-stop proof.
        let f = Fixture::new().await;
        assert_eq!(
            f.store
                .admit_attempt_guarded(&f.claim, None, f.guard().await, false)
                .await
                .unwrap(),
            AttemptAdmission::Applied
        );
        let predecessor = f
            .store
            .load_physical_attempt(&f.claim.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            f.store.renew_lease(&f.claim, 0).await.unwrap(),
            LeaseRenewalOutcome::Current
        );
        f.clock.set(10_000_001);
        let worker = f
            .registry
            .current("worker")
            .await
            .unwrap()
            .unwrap()
            .snapshot;
        let current = RunClaim::from(
            &f.store
                .claim_run_compatible(&f.claim.run_id, &worker, 60_000)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        assert!(current.epoch > f.claim.epoch);
        assert_eq!(current.owner, f.claim.owner);
        let before_handoff = f.operations().await;
        assert_eq!(
            f.store
                .admit_attempt_guarded(&current, None, f.guard().await, false)
                .await
                .unwrap(),
            AttemptAdmission::Blocked
        );
        assert_eq!(
            f.store
                .load_physical_attempt(&f.claim.run_id)
                .await
                .unwrap(),
            Some(predecessor)
        );
        assert_eq!(f.operations().await, before_handoff);
        assert_eq!(
            f.store
                .admit_attempt_guarded(&current, None, f.guard().await, true)
                .await
                .unwrap(),
            AttemptAdmission::Replaced
        );
        let installed = f
            .store
            .load_physical_attempt(&current.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(installed.claim, current);
        assert!(installed.binding.is_none());
        assert_eq!(f.operations().await, before_handoff + 1);
        assert_eq!(
            f.store.finish_attempt(&f.claim, None).await.unwrap(),
            SettleOutcome::Fenced
        );
        let binding =
            AttemptExecutionBinding::new(f.identity.clone(), "provider", "execution").unwrap();
        let backfill = f
            .store
            .admit_attempt_guarded(&current, Some(&binding), f.guard().await, true)
            .await;
        assert!(
            matches!(backfill, Err(DispatchError::Conflict(_))),
            "{backfill:?}"
        );
        assert_eq!(
            f.store
                .load_physical_attempt(&current.run_id)
                .await
                .unwrap(),
            Some(installed)
        );
        assert_eq!(f.operations().await, before_handoff + 1);
        assert_eq!(
            f.store
                .admit_attempt_guarded(&current, None, f.guard().await, true)
                .await
                .unwrap(),
            AttemptAdmission::AlreadyApplied
        );
        assert_eq!(f.operations().await, before_handoff + 1);
    }

    #[tokio::test]
    async fn shared_database_unpolled_admission_cancellation_releases_original_registry() {
        // C1 R owns its original writer but admission has not been polled.
        // Cancelling that future drops the same transaction: no SQL effect and
        // the exact R guard can be reacquired before the same claim succeeds.
        // This makes no claim about cancellation of an already-dispatched
        // executor or physical quiescence; those have their own closure owner.
        let f = Fixture::new().await;
        let original_operations = f.operations().await;
        let admission = f
            .store
            .admit_attempt_guarded(&f.claim, None, f.guard().await, false);
        drop(admission);
        assert_eq!(f.operations().await, original_operations);
        assert!(
            f.store
                .load_physical_attempt(&f.claim.run_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                f.store
                    .admit_attempt_guarded(&f.claim, None, f.guard().await, false)
                    .await
            })
            .await
            .unwrap()
            .unwrap(),
            AttemptAdmission::Applied
        );
        assert_eq!(f.operations().await, original_operations + 1);
    }
}
