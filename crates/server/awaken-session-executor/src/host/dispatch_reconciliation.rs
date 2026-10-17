//! Integration tests for the canonical dispatch terminal reconciler.
use super::*;
use awaken_dispatch_contract::{DispatchControlClaims as _, DispatchSettlement as _};
#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::worker_resolver::test_support::{AdoptionModel, claim, test_activation};
    use async_trait::async_trait;
    use awaken_agent_contract::agent::{run::EndCause, thread::Id as ThreadId};
    use awaken_dispatch_contract::DispatchAttemptRecordReads as _;
    use awaken_dispatch_contract::{
        DispatchClaims, DispatchCommitAuthority, DispatchControlReads, DispatchExecution,
        DispatchReads, SessionRunReservations,
    };
    use awaken_worker_contract::WorkerIncarnationSource;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct IdleEvidence {
        released: std::sync::Mutex<Vec<awaken_dispatch_contract::WorkerIdentity>>,
        fail_first: std::sync::atomic::AtomicBool,
        stops:
            std::sync::Mutex<Vec<awaken_dispatch_contract::dispatch::AttemptExecutionBinding>>,
        attempts: std::sync::Mutex<Vec<awaken_dispatch_contract::dispatch::PhysicalAttempt>>,
        complete: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl awaken_dispatch::WorkerExecutionStopSource for IdleEvidence {
        fn owns_worker_evidence(&self, _: &awaken_dispatch_contract::WorkerIdentity) -> bool {
            true
        }
        async fn observe_closed_idle_worker(
            &self,
            identity: &awaken_dispatch_contract::WorkerIdentity,
            _: bool,
            _: Option<&awaken_dispatch::WorkerCreateLedger>,
        ) -> Result<
            Option<awaken_dispatch::WorkerProcessExit>,
            awaken_session_execution_contract::ApplicationError,
        > {
            Ok(Some(
                awaken_dispatch::WorkerProcessExit::new(
                    "fixture-process",
                    identity.lease_owner(),
                    "durable-exit",
                )
                .unwrap(),
            ))
        }
        async fn release_closed_idle_worker_evidence(
            &self,
            identity: &awaken_dispatch_contract::WorkerIdentity,
            _original_create_ledger: Option<&awaken_dispatch_contract::WorkerCreateLedger>,
        ) -> Result<(), awaken_session_execution_contract::ApplicationError> {
            if self.fail_first.swap(false, Ordering::SeqCst) {
                return Err(
                    awaken_session_execution_contract::ApplicationError::unavailable(
                        "provider unavailable",
                    ),
                );
            }
            self.released.lock().unwrap().push(identity.clone());
            Ok(())
        }
        async fn stop_closed_worker(
            &self,
            command: &awaken_dispatch::ClosedWorkerExecution,
        ) -> Result<
            awaken_dispatch::WorkerProcessStopStatus,
            awaken_session_execution_contract::ApplicationError,
        > {
            self.stops.lock().unwrap().push(command.binding().clone());
            command.observed_exit("durable-exit")
        }
        async fn close_attempt_execution(
            &self,
            command: &awaken_dispatch::ClosedAttemptExecution,
            recovery: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        ) -> Result<
            awaken_dispatch::AttemptStopStatus,
            awaken_session_execution_contract::ApplicationError,
        > {
            assert_eq!(recovery.claimed_run_id, command.attempt().claim.run_id);
            assert_eq!(
                Some(&recovery.thread_id),
                command.attempt().thread_id.as_ref()
            );
            self.attempts
                .lock()
                .unwrap()
                .push(command.attempt().clone());
            if self.complete.load(Ordering::SeqCst) {
                command.observed_all_execution_stopped("all-original-participants-terminal")
            } else {
                Ok(awaken_dispatch::AttemptStopStatus::Unknown)
            }
        }
    }

    #[tokio::test]
    async fn idle_retention_retries_expired_originals_without_starving_later_workers() {
        // M0 one injected Ingress adapter crosses the neutral idle leaf; its
        // explicit terminal role shares the same Source Arc and is not invoked
        // by idle maintenance. Host has no production Ingress implementation edge;
        // Causes: C1 expired exact registration; C2 no Dispatch slot; C3
        // independent process exit; C4 first provider release fails; C5 later
        // Worker waits; C6 retry after Registry already stored exit.
        // Effects: E1 exact exit is durable before release, E2 failed release
        // retains that exit, E3 later Worker progresses, E4 retry does not
        // create another exit and releases exactly once.
        // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
        // | M1   | Y  | Y  | Y  | Y  | -  | N  | E1+E2  |
        // | M2   | Y  | Y  | Y  | N  | Y  | N  | E1+E3  |
        // | M3   | Y  | Y  | Y  | N  | -  | Y  | E4     |
        // The cursor is scheduling only; Registry and Dispatch are re-read.
        use awaken_dispatch_contract::WorkerDirectory as _;
        let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
        let mut identities = Vec::new();
        for id in ["retention-a", "retention-b"] {
            identities.push(
                directory
                    .register(
                        awaken_dispatch_contract::WorkerRegistration {
                            worker_id: id.into(),
                            incarnation_id: "boot".into(),
                            manifest: Default::default(),
                            create_ledger: None,
                        },
                        0,
                        1,
                    )
                    .await
                    .unwrap()
                    .snapshot
                    .identity,
            );
        }
        let host =
            (crate::HostDependencies::ephemeral(Arc::new(crate::host::MemoryHostModel), "fixture")
                .await
                .with_ephemeral_dispatch(Arc::new(
                    awaken_dispatch::open_sqlite_dispatch_store_in_memory(Arc::new(
                        awaken_lease_testkit::ManualClock::new(0),
                    ))
                    .await
                    .unwrap(),
                )))
            .build()
            .expect("validated Host dependencies");
        let source = Arc::new(IdleEvidence {
            released: Default::default(),
            fail_first: std::sync::atomic::AtomicBool::new(true),
            stops: Default::default(),
            attempts: Default::default(),
            complete: std::sync::atomic::AtomicBool::new(false),
        });
        host.bind_worker_evidence_reconciliation(
            directory.clone(),
            Arc::new(awaken_dispatch::WorkerEvidenceReleaseAdapter::new(
                source.clone(),
            )),
        )
        .unwrap();
        crate::host::session_worker_resolver(&host)
            .expect("Host dispatch resolver")
            .reconcile_worker_evidence()
            .await
            .unwrap();
        assert!(
            directory
                .incarnation(&identities[0])
                .await
                .unwrap()
                .unwrap()
                .process_exit
                .is_some(),
            "M1 exit was committed before failed release"
        );
        crate::host::session_worker_resolver(&host)
            .expect("Host dispatch resolver")
            .reconcile_worker_evidence()
            .await
            .unwrap();
        crate::host::session_worker_resolver(&host)
            .expect("Host dispatch resolver")
            .reconcile_worker_evidence()
            .await
            .unwrap();
        assert_eq!(
            *source.released.lock().unwrap(),
            [identities[1].clone(), identities[0].clone()]
        );
        assert!(
            source.stops.lock().unwrap().is_empty(),
            "M0 idle retention cannot stop an attempt"
        );
        assert!(
            source.attempts.lock().unwrap().is_empty(),
            "M0 narrow adapter cannot close a domain"
        );
        for identity in identities {
            let record = directory.incarnation(&identity).await.unwrap().unwrap();
            assert_eq!(
                record.snapshot.state,
                awaken_dispatch_contract::WorkerState::Dead
            );
            assert_eq!(
                record.process_exit.unwrap().provider_id(),
                "fixture-process",
                "M1-M3"
            );
        }
    }

    struct LeaseDeadlineEvidence {
        observed: std::sync::Mutex<Vec<bool>>,
        released: AtomicUsize,
    }

    #[async_trait]
    impl awaken_dispatch::WorkerExecutionStopSource for LeaseDeadlineEvidence {
        async fn observe_closed_idle_worker(
            &self,
            identity: &awaken_dispatch_contract::WorkerIdentity,
            may_stop_active: bool,
            _: Option<&awaken_dispatch::WorkerCreateLedger>,
        ) -> Result<
            Option<awaken_dispatch::WorkerProcessExit>,
            awaken_session_execution_contract::ApplicationError,
        > {
            self.observed.lock().unwrap().push(may_stop_active);
            Ok(may_stop_active.then(|| {
                awaken_dispatch::WorkerProcessExit::new(
                    "fixture-process",
                    identity.lease_owner(),
                    "observed-stop",
                )
                .unwrap()
            }))
        }
        async fn release_closed_idle_worker_evidence(
            &self,
            _: &awaken_dispatch_contract::WorkerIdentity,
            _: Option<&awaken_dispatch_contract::WorkerCreateLedger>,
        ) -> Result<(), awaken_session_execution_contract::ApplicationError> {
            self.released.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn stop_closed_worker(
            &self,
            _: &awaken_dispatch::ClosedWorkerExecution,
        ) -> Result<
            awaken_dispatch::WorkerProcessStopStatus,
            awaken_session_execution_contract::ApplicationError,
        > {
            panic!("no Dispatch slot exists")
        }
    }

    #[tokio::test]
    async fn graceful_closed_idle_worker_waits_until_original_lease_deadline_to_stop_live_process()
    {
        // Causes: C1 exact Worker deregisters while still active; C2 the
        // original Registry lease deadline is in the future/equal/past; C3
        // empty Dispatch; C4 independent provider stops only when allowed.
        // Effects: E1 C1+C3+future => no stop, receipt or release;
        // E2 C1+C3+equal/past+C4 => exact durable exit and one release;
        // E3 replay after receipt => no second stop observation.
        // Natural-exit probes grant no active stop. Their count is independent
        // of E2; exactly one allowed stop, then no new probe on replay, is the
        // behavioral boundary rather than a fixed false/true call sequence.
        // | Rule | C1 | C2       | C3 | C4 | Effect |
        // | G1   | Y  | future   | Y  | -  | E1     |
        // | G2   | Y  | equal    | Y  | Y  | E2     |
        // | G3   | Y  | past     | Y  | -  | E3     |
        use awaken_dispatch_contract::WorkerDirectory as _;
        let directory = awaken_worker_registry::MemoryWorkerDirectory::new();
        let identity = directory
            .register(
                awaken_dispatch_contract::WorkerRegistration {
                    worker_id: "graceful-idle".into(),
                    incarnation_id: "boot".into(),
                    manifest: Default::default(),
                    create_ledger: None,
                },
                100,
                1000,
            )
            .await
            .unwrap()
            .snapshot
            .identity;
        directory.deregister(&identity).await.unwrap();
        let dispatch = awaken_dispatch::open_sqlite_dispatch_store_in_memory(Arc::new(
            awaken_lease_testkit::ManualClock::new(100),
        ))
        .await
        .unwrap();
        let source = LeaseDeadlineEvidence {
            observed: Default::default(),
            released: AtomicUsize::new(0),
        };
        awaken_dispatch::release_closed_idle_worker_evidence(
            &dispatch, &directory, &source, &identity, 1099,
        )
        .await
        .unwrap();
        assert_eq!(*source.observed.lock().unwrap(), [false], "G1");
        assert!(
            directory
                .incarnation(&identity)
                .await
                .unwrap()
                .unwrap()
                .process_exit
                .is_none(),
            "G1"
        );
        assert_eq!(source.released.load(Ordering::SeqCst), 0, "G1");
        awaken_dispatch::release_closed_idle_worker_evidence(
            &dispatch, &directory, &source, &identity, 1100,
        )
        .await
        .unwrap();
        let after_exit = source.observed.lock().unwrap().clone();
        assert_eq!(
            after_exit.iter().filter(|allowed| **allowed).count(),
            1,
            "G2"
        );
        assert_eq!(after_exit.last(), Some(&true), "G2");
        assert!(
            directory
                .incarnation(&identity)
                .await
                .unwrap()
                .unwrap()
                .process_exit
                .is_some(),
            "G2"
        );
        assert_eq!(source.released.load(Ordering::SeqCst), 1, "G2");
        awaken_dispatch::release_closed_idle_worker_evidence(
            &dispatch, &directory, &source, &identity, 1200,
        )
        .await
        .unwrap();
        assert_eq!(*source.observed.lock().unwrap(), after_exit, "G3");
        assert_eq!(
            source.released.load(Ordering::SeqCst),
            2,
            "G3 idempotent release replay"
        );
    }

    struct UnknownIdleEvidence;

    #[async_trait]
    impl awaken_dispatch::WorkerExecutionStopSource for UnknownIdleEvidence {
        fn owns_worker_evidence(&self, _: &awaken_dispatch_contract::WorkerIdentity) -> bool {
            true
        }
        async fn release_closed_idle_worker_evidence(
            &self,
            _: &awaken_dispatch_contract::WorkerIdentity,
            _: Option<&awaken_dispatch_contract::WorkerCreateLedger>,
        ) -> Result<(), awaken_session_execution_contract::ApplicationError> {
            panic!("Unknown physical exit cannot release provider evidence")
        }
        async fn stop_closed_worker(
            &self,
            _: &awaken_dispatch::ClosedWorkerExecution,
        ) -> Result<
            awaken_dispatch::WorkerProcessStopStatus,
            awaken_session_execution_contract::ApplicationError,
        > {
            panic!("idle maintenance has no Dispatch attempt")
        }
    }

    #[tokio::test]
    async fn closed_idle_worker_without_process_evidence_keeps_provider_retention() {
        // Causes: C1 exact Registry lease expires; C2 Dispatch has no physical
        // slot; C3 selected independent provider returns Unknown exit.
        // Effects: E1 Registry closes admission, E2 no process-exit receipt,
        // E3 provider retention remains. Rule U1=C1+C2+C3=>E1+E2+E3;
        // empty Dispatch alone cannot become a physical proof.
        // C0 select this exact local queue before the canonical Host build;
        // the construction migration must preserve U1 and its zero-release panic.
        use awaken_dispatch_contract::WorkerDirectory as _;
        let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
        let identity = directory
            .register(
                awaken_dispatch_contract::WorkerRegistration {
                    worker_id: "unknown-idle".into(),
                    incarnation_id: "boot".into(),
                    manifest: Default::default(),
                    create_ledger: None,
                },
                0,
                1,
            )
            .await
            .unwrap()
            .snapshot
            .identity;
        let host =
            crate::HostDependencies::ephemeral(Arc::new(crate::host::MemoryHostModel), "fixture")
                .await
                .with_ephemeral_dispatch(Arc::new(
                    awaken_dispatch::open_sqlite_dispatch_store_in_memory(Arc::new(
                        awaken_lease_testkit::ManualClock::new(0),
                    ))
                    .await
                    .unwrap(),
                ))
                .build()
                .expect("validated Host dependencies");
        host.bind_worker_evidence_reconciliation(
            directory.clone(),
            Arc::new(awaken_dispatch::WorkerEvidenceReleaseAdapter::new(
                Arc::new(UnknownIdleEvidence),
            )),
        )
        .unwrap();
        crate::host::session_worker_resolver(&host)
            .expect("Host dispatch resolver")
            .reconcile_worker_evidence()
            .await
            .unwrap();
        let record = directory.incarnation(&identity).await.unwrap().unwrap();
        assert_eq!(
            record.snapshot.state,
            awaken_dispatch_contract::WorkerState::Dead,
            "U1/E1"
        );
        assert!(
            record.process_exit.is_none(),
            "U1/E2, and release would panic"
        );
    }

    struct UnownedEvidence;

    #[async_trait]
    impl awaken_dispatch::WorkerExecutionStopSource for UnownedEvidence {
        async fn release_closed_idle_worker_evidence(
            &self,
            _: &awaken_dispatch_contract::WorkerIdentity,
            _: Option<&awaken_dispatch_contract::WorkerCreateLedger>,
        ) -> Result<(), awaken_session_execution_contract::ApplicationError> {
            panic!("an unowned domain cannot release provider evidence")
        }
        async fn stop_closed_worker(
            &self,
            _: &awaken_dispatch::ClosedWorkerExecution,
        ) -> Result<
            awaken_dispatch::WorkerProcessStopStatus,
            awaken_session_execution_contract::ApplicationError,
        > {
            panic!("expiry is not physical termination")
        }
    }

    #[tokio::test]
    async fn registry_expiry_does_not_depend_on_provider_ownership_or_retire_unknown_attempts() {
        // Cause/effect graph: U1 the selected provider does not own a legacy
        // identity; U2 its exact Registry lease has expired; U3 another legacy
        // identity is live; U4 an unbound physical attempt remains occupied.
        // Effects: E1 U1+U2 closes only Registry admission; E2 U1+U3 preserves
        // the live identity; E3 no provider call/exit receipt is invented; E4
        // U4 keeps the exact original slot. One injected Ingress release
        // adapter owns the optional provider action; the Host only scans and
        // expires the Registry. Rules UE1=U1+U2=>E1+E3+E4;
        // UE2=U1+U3=>E2+E3. This uses normal registration/claim/admission APIs,
        // not a database backfill or synthetic termination assertion.
        use awaken_dispatch::WorkerDirectory as _;
        let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
        let mut identities = Vec::new();
        for (id, ttl) in [("legacy-a", 1), ("legacy-b", u64::MAX)] {
            identities.push(
                directory
                    .register(
                        awaken_dispatch_contract::WorkerRegistration {
                            worker_id: id.into(),
                            incarnation_id: "boot".into(),
                            manifest: Default::default(),
                            create_ledger: None,
                        },
                        0,
                        ttl,
                    )
                    .await
                    .unwrap()
                    .snapshot
                    .identity,
            );
        }
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let dispatch = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .unwrap(),
        );
        let claimed = claim(
            &dispatch,
            "legacy-thread",
            "legacy-run",
            &identities[0].lease_owner(),
            0,
            &clock,
        )
        .await;
        assert_eq!(
            dispatch
                .begin_attempt(
                    &awaken_dispatch_contract::RunClaim::from(&claimed.lease),
                    None
                )
                .await
                .unwrap(),
            awaken_dispatch_contract::AttemptAdmission::Applied
        );
        let before = dispatch
            .load_physical_attempt(claimed.lease.run_id())
            .await
            .unwrap()
            .unwrap();
        assert!(before.binding.is_none(), "U4 historical unbound slot");
        let host =
            (crate::HostDependencies::ephemeral(Arc::new(crate::host::MemoryHostModel), "fixture")
                .await
                .with_ephemeral_dispatch(dispatch.clone()))
            .build()
            .expect("validated Host dependencies");
        host.bind_worker_evidence_reconciliation(
            directory.clone(),
            Arc::new(awaken_dispatch::WorkerEvidenceReleaseAdapter::new(
                Arc::new(UnownedEvidence),
            )),
        )
        .unwrap();
        for _ in 0..2 {
            crate::host::session_worker_resolver(&host)
                .expect("Host dispatch resolver")
                .reconcile_worker_evidence()
                .await
                .unwrap();
        }
        let expired = directory
            .incarnation(&identities[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            expired.snapshot.state,
            awaken_dispatch_contract::WorkerState::Dead,
            "UE1/E1"
        );
        assert!(expired.process_exit.is_none(), "UE1/E3");
        let live = directory
            .incarnation(&identities[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            live.snapshot.state,
            awaken_dispatch_contract::WorkerState::Starting,
            "UE2/E2"
        );
        assert!(live.process_exit.is_none(), "UE2/E3");
        let after = dispatch
            .load_physical_attempt(claimed.lease.run_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after, before,
            "UE1/E4 exact unknown physical attempt retained"
        );
    }

    #[tokio::test]
    async fn host_terminal_scan_closes_original_occupancy_without_a_successor_or_environment() {
        // Causes: C1 exact committed Ended; C2 original bound occupied slot;
        // C3 expired/closed original Worker; C4 same trusted startup source;
        // C5 full domain evidence. C4 is the explicit neutral terminal role;
        // private proof construction remains in its Ingress adapter/kernel.
        // C2 also requires real Registry Ready+compatible claim -> exact typed
        // WorkerAssignment; merely spelling its lease-owner string does not
        // authorize a bound physical attempt. The original binding law stays.
        // Effects: E0 no stop or mutation; E1 local
        // exit persists but Unknown domain keeps exact closing slot; E2 full
        // evidence atomically retires old then ordinary terminal claim settles
        // Done, no execution/Environment. E3 later cadence retries same old
        // binding, never new Worker/source coordinates. No successor is claimed
        // before maintenance. The original physical and logical lease differ.
        // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
        // | O0 | no | yes | yes | yes | yes | E0 |
        // | O1 | yes | yes | live | yes | yes | E0 |
        // | O2 | yes | yes | yes | absent | yes | E0 |
        // O2e absent role + canonical Ended + empty slot => original fenced
        // terminal claim/settle only (the existing empty legacy case below).
        // Missing role is not a fake provider or an independent exit receipt;
        // concurrent hidden occupancy still makes the Store claim fail closed.
        // | O3 | yes | yes | yes | yes | unknown then complete | E1 then E2+E3 |
        // | O4 | yes | yes | yes | yes | complete | E2 |
        // O5 occupied execution cannot relinquish/supersede: original Leased
        // identity/occupancy survive the new Pending successor. Only canonical
        // Ended+independent complete old evidence can retire/settle the old Run;
        // successor stays Pending, never executes or creates an Environment.
        // O6 cancelled+expired occupied DeadLetter => close original occupancy
        // only, retain sealed delivery, zero Done/requeue. Superseded+occupied
        // is not public-law reachable and belongs to scanner read-fault coverage.
        use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};
        use awaken_dispatch_contract::{
            DispatchAdministration as _, DispatchEnqueue as _, DispatchMaintenance as _,
            WorkerDirectory as _,
        };
        // Keep the large owned test workflow at the same boxed async boundary
        // used by other Host fixture tests; default thread stack stays unchanged.
        Box::pin(async {
            for rule in [
                "nonterminal",
                "live",
                "unbound-source",
                "unknown",
                "complete",
                "supersession-rejected",
                "dead-letter",
            ] {
                let storage = tempfile::tempdir().unwrap();
                let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
                let store = Arc::new(
                    awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                        .await
                        .unwrap(),
                );
                let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
                // Registered placement must come from actual Registry state,
                // not an owner string. Publish the existing protocol manifest,
                // heartbeat Ready, then let claim_run_compatible freeze the
                // exact WorkerAssignment before binding the physical attempt.
                let mut manifest = awaken_dispatch_contract::WorkerManifest::default();
                manifest.runtime_protocol.min = 1;
                manifest.runtime_protocol.max =
                    awaken_dispatch_contract::CURRENT_RUNTIME_PROTOCOL_VERSION;
                let registry_ttl = if rule == "live" { u64::MAX } else { 1 };
                let identity = directory
                    .register(
                        awaken_dispatch_contract::WorkerRegistration {
                            worker_id: "orphan-original".into(),
                            incarnation_id: "old-boot".into(),
                            manifest,
                            create_ledger: None,
                        },
                        0,
                        registry_ttl,
                    )
                    .await
                    .unwrap()
                    .snapshot
                    .identity;
                assert_eq!(
                    directory
                        .heartbeat(
                            &identity,
                            awaken_dispatch_contract::WorkerHeartbeat {
                                sequence: 1,
                                ready: true,
                                in_flight: 0,
                                warm_environment_shapes: Default::default(),
                                credential_observations: Default::default(),
                                acp_capability_observations: Default::default(),
                            },
                            0,
                            registry_ttl
                        )
                        .await
                        .unwrap(),
                    awaken_dispatch_contract::RegistryMutation::Applied
                );
                let worker = directory
                    .incarnation(&identity)
                    .await
                    .unwrap()
                    .unwrap()
                    .snapshot;
                let run_id = RunId("orphan-run".into());
                store
                    .enqueue(
                        awaken_dispatch_contract::RunDispatch::new(test_activation(
                            "orphan-thread",
                            &run_id.0,
                        ))
                        .with_placement(
                            awaken_dispatch_contract::PlacementRequirements::remote_required(),
                        ),
                    )
                    .await
                    .unwrap();
                let claimed = store
                    .claim_run_compatible(&run_id, &worker, 1_000)
                    .await
                    .unwrap()
                    .expect("actual registered placement");
                assert_eq!(
                    claimed.assignment.as_ref().unwrap().identity,
                    identity,
                    "C2 exact registered assignment"
                );
                let old = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
                let binding = awaken_dispatch_contract::dispatch::AttemptExecutionBinding::new(
                    identity,
                    "fixture-process",
                    "original-execution",
                )
                .unwrap();
                assert_eq!(
                    store.begin_attempt(&old, Some(&binding)).await.unwrap(),
                    awaken_dispatch_contract::AttemptAdmission::Applied
                );
                let original = store
                    .load_physical_attempt(&old.run_id)
                    .await
                    .unwrap()
                    .unwrap();
                if ["supersession-rejected", "dead-letter"].contains(&rule) {
                    if rule == "supersession-rejected" {
                        assert_eq!(
                            store.relinquish_claim(&old).await.unwrap(),
                            awaken_dispatch_contract::SettleOutcome::Fenced,
                            "O5 original executing claim cannot become Pending"
                        );
                        store
                            .enqueue_with(
                                awaken_dispatch_contract::RunDispatch::new(test_activation(
                                    "orphan-thread",
                                    "later-delivery",
                                )),
                                awaken_dispatch_contract::SubmitOptions {
                                    supersede: true,
                                    ..Default::default()
                                },
                            )
                            .await
                            .unwrap();
                        assert_eq!(
                            store.load_physical_attempt(&old.run_id).await.unwrap(),
                            Some(original.clone()),
                            "O5 supersede cannot rewrite original physical owner"
                        );
                        let row = store.dispatch_summary(&old.run_id).await.unwrap().unwrap();
                        assert_eq!(
                            row.state,
                            awaken_dispatch_contract::DispatchState::Leased,
                            "O5 old delivery is not Superseded"
                        );
                        assert!(!row.cancellation_requested, "O5 no cancellation invented");
                        assert_eq!(
                            store
                                .dispatch_summary(&RunId("later-delivery".into()))
                                .await
                                .unwrap()
                                .unwrap()
                                .state,
                            awaken_dispatch_contract::DispatchState::Pending,
                            "O5 successor exists only as Pending work"
                        );
                    } else {
                        store.cancel(&old.run_id).await.unwrap();
                        let quarantine = store
                            .claim_run(&old.run_id, "quarantine-owner", 1, &Default::default())
                            .await
                            .unwrap()
                            .unwrap();
                        clock.set(quarantine.lease.expires_ms() + 1);
                        assert_eq!(
                            store
                                .quarantine_retry_exhausted(0, quarantine.lease.expires_ms() + 1)
                                .await
                                .unwrap(),
                            1
                        );
                    }
                }
                clock.set(claimed.lease.expires_ms() + 1);
                let mut dependencies =
                    crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub").await;
                dependencies.clock = clock.clone();
                let mut dependencies = dependencies
                    .with_store_dir(storage.path())
                    .await
                    .with_ephemeral_dispatch(store.clone());
                dependencies.execution.ingress = crate::RunIngress::Durable;
                dependencies.execution.local = crate::LocalExecution::RemoteWorkersOnly;
                let host = dependencies.build().unwrap();
                if rule != "nonterminal" {
                    let commit = host.authority.open_commit("orphan-thread").await.unwrap();
                    commit_run(
                        &commit,
                        &ThreadId("orphan-thread".into()),
                        RunDisposition::ended(old.run_id.clone(), EndCause::NaturalEnd),
                        Vec::new(),
                        Vec::new(),
                    )
                    .await
                    .unwrap();
                }
                let source = Arc::new(IdleEvidence {
                    released: Default::default(),
                    fail_first: std::sync::atomic::AtomicBool::new(false),
                    stops: Default::default(),
                    attempts: Default::default(),
                    complete: std::sync::atomic::AtomicBool::new(rule != "unknown"),
                });
                if rule != "unbound-source" {
                    host.bind_worker_evidence_reconciliation(
                        directory,
                        Arc::new(awaken_dispatch::WorkerEvidenceReleaseAdapter::new(
                            source.clone(),
                        )),
                    )
                    .unwrap();
                }
                let resolver =
                    crate::host::session_worker_resolver(&host).expect("Host dispatch resolver");
                let completed = resolver.reconcile_committed_terminals(8).await.unwrap();
                if ["nonterminal", "live", "unbound-source"].contains(&rule) {
                    assert!(completed.is_empty(), "{rule}");
                    assert_eq!(
                        store.load_physical_attempt(&old.run_id).await.unwrap(),
                        Some(original),
                        "{rule}"
                    );
                    assert!(source.stops.lock().unwrap().is_empty(), "{rule}");
                    assert!(source.attempts.lock().unwrap().is_empty(), "{rule}");
                } else {
                    if rule == "unknown" {
                        assert!(completed.is_empty(), "O3");
                        let closing = store
                            .load_physical_attempt(&old.run_id)
                            .await
                            .unwrap()
                            .unwrap();
                        assert!(closing.closing, "O3");
                        assert_eq!(closing.claim, old, "O3 original identity");
                        assert!(
                            store
                                .attempt_closure_records_after(
                                    awaken_dispatch_contract::DispatchCursor(0),
                                    10,
                                    Some(&old.run_id)
                                )
                                .await
                                .unwrap()
                                .events
                                .is_empty(),
                            "O3 no receipt"
                        );
                        source.complete.store(true, Ordering::SeqCst);
                        assert_eq!(
                            resolver
                                .reconcile_committed_terminals(8)
                                .await
                                .unwrap()
                                .len(),
                            1,
                            "O3 retry"
                        );
                        assert_eq!(source.attempts.lock().unwrap().len(), 2, "O3 same domain");
                    } else if rule == "dead-letter" {
                        assert!(
                            completed.is_empty(),
                            "O6 closure alone cannot settle sealed delivery"
                        );
                    } else {
                        assert_eq!(completed.len(), 1, "O4");
                    }
                    assert!(
                        store
                            .load_physical_attempt(&old.run_id)
                            .await
                            .unwrap()
                            .is_none(),
                        "O3/O4/O5"
                    );
                    assert_eq!(
                        *source.stops.lock().unwrap(),
                        [binding],
                        "one original stop"
                    );
                    if rule == "dead-letter" {
                        let row = store.dispatch_summary(&old.run_id).await.unwrap().unwrap();
                        assert_eq!(
                            row.state,
                            awaken_dispatch_contract::DispatchState::DeadLetter,
                            "O6 original sealed delivery state"
                        );
                        assert!(!row.physical_attempt_active, "O6 physical closure only");
                        assert!(
                            store
                                .completion_events_after(0, 10)
                                .await
                                .unwrap()
                                .is_empty(),
                            "O6 no Done"
                        );
                    } else {
                        let rows = store.list_dispatches().await.unwrap();
                        if rule == "supersession-rejected" {
                            assert_eq!(rows.len(), 1, "O5 only original terminal Run was settled");
                            assert_eq!(
                                rows[0].run_id,
                                RunId("later-delivery".into()),
                                "O5 exact successor remains"
                            );
                            assert_eq!(
                                rows[0].state,
                                awaken_dispatch_contract::DispatchState::Pending,
                                "O5 no successor execution"
                            );
                            assert!(
                                !rows[0].physical_attempt_active,
                                "O5 no successor physical owner"
                            );
                        } else {
                            assert!(rows.is_empty(), "ordinary Done after closure");
                        }
                        assert_eq!(
                            store.completion_events_after(0, 10).await.unwrap().len(),
                            1,
                            "ordinary settlement"
                        );
                    }
                    let closure = store
                        .attempt_closure_records_after(
                            awaken_dispatch_contract::DispatchCursor(0),
                            10,
                            Some(&old.run_id),
                        )
                        .await
                        .unwrap();
                    assert_eq!(closure.events.len(), 1, "one original fact");
                    assert_eq!(
                        closure.events[0].record.attempt().claim,
                        old,
                        "retained original identity"
                    );
                }
                assert!(
                    host.realization_executor
                        .environments()
                        .session_environment("orphan-thread")
                        .await
                        .expect("installed Sessions")
                        .is_none(),
                    "environment-free {rule}"
                );
            }
        })
        .await;
    }

    /// Cause/effect decision table for Host-wide legacy reconciliation:
    ///
    /// | Rule | Host role | Dispatch | Thread commit | Expected effect |
    /// |------|-----------|----------|---------------|-----------------|
    /// | R0 | local pool | any | any | no duplicate coordinator daemon |
    /// | R1 | coordinator-only | Awaiting | no terminal Run | preserve row and Session state |
    /// | R2 | coordinator-only/no terminal role | Awaiting + empty physical slot | exact Run Ended | environment-free fenced Done |
    /// | R3 | coordinator-only + R2 | exact settlement | exact terminal | completion tombstone, no Sandbox |
    ///
    /// Constraints: each row is read through its own Thread commit boundary;
    /// neither queue metadata nor environment availability can prove outcome.
    /// R2 is O2e above: no injected terminal role permits no stop or proof;
    /// the original atomic claim is still the empty-slot delivery authority.
    #[tokio::test]
    async fn host_reconciles_only_exact_committed_terminals_without_opening_sessions() {
        use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};

        let mut local_pool =
            crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub").await;
        local_pool.execution.ingress = crate::RunIngress::Durable;
        local_pool.execution.local = crate::LocalExecution::in_process_pool();
        let local_pool = (local_pool).build().expect("validated Host dependencies");
        assert!(!local_pool.ensure_terminal_dispatch_reconciliation(), "R0");

        let storage = tempfile::tempdir().expect("storage");
        let now = awaken_time::system_clock().now().as_epoch_ms();
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(now));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("dispatch store"),
        );
        let mut coordinator = crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await
            .with_ephemeral_dispatch(store.clone());
        coordinator.execution.ingress = crate::RunIngress::Durable;
        coordinator.execution.local = crate::LocalExecution::RemoteWorkersOnly;
        let coordinator = (coordinator).build().expect("validated Host dependencies");
        let host = coordinator;

        let control = claim(
            &store,
            "thread-control",
            "run-control",
            "setup",
            now,
            &clock,
        )
        .await;
        store
            .settle(
                control.lease.run_id(),
                control.lease.epoch(),
                awaken_dispatch_contract::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("quiesce control");
        let terminal = claim(
            &store,
            "thread-terminal",
            "run-terminal",
            "setup",
            now,
            &clock,
        )
        .await;
        store
            .settle(
                terminal.lease.run_id(),
                terminal.lease.epoch(),
                awaken_dispatch_contract::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("quiesce terminal candidate");

        let commit = host
            .authority
            .open_commit("thread-terminal")
            .await
            .expect("terminal Thread commit");
        commit_run(
            &commit,
            &ThreadId("thread-terminal".to_string()),
            RunDisposition::ended(RunId("run-terminal".to_string()), EndCause::NaturalEnd),
            Vec::new(),
            Vec::new(),
        )
        .await
        .expect("record exact terminal truth");

        assert!(host.ensure_terminal_dispatch_reconciliation());
        assert!(
            !host.ensure_terminal_dispatch_reconciliation(),
            "one coordinator daemon"
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if store
                    .list_dispatches()
                    .await
                    .expect("reconciliation rows")
                    .len()
                    == 1
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("R2 coordinator daemon reconciles immediately");
        let rows = store.list_dispatches().await.expect("remaining dispatches");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].run_id, RunId("run-control".to_string()), "R1");
        assert!(
            host.realization_executor
                .environments()
                .session_environment("thread-control")
                .await
                .expect("installed Sessions")
                .is_none()
        );
        assert!(
            host.realization_executor
                .environments()
                .session_environment("thread-terminal")
                .await
                .expect("installed Sessions")
                .is_none(),
            "R3"
        );
        let completions = store
            .completion_events_after(0, 10)
            .await
            .expect("completion tombstone");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].run_id, RunId("run-terminal".to_string()));
    }

    #[tokio::test]
    async fn coordinator_terminal_scan_preserves_nonterminal_replacement_authority() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 a Coordinator-only active-active peer scans the
        // shared durable store; C2 a row is quiescent Awaiting without committed
        // Ended truth; C3 another row is Leased by Worker A, its lease expires,
        // and committed truth is still non-terminal; C4 Worker B reclaims C3;
        // C5 Worker A later settles with its old epoch. Effects: E1 the
        // coordinator does not claim or advance either row; E2 B receives the
        // next epoch through the existing Dispatch owner; E3 A is fenced while
        // B remains current. The Awaiting probe checks its exact next epoch, so
        // a claim-and-restore maintenance hot loop cannot hide behind unchanged
        // public status.
        //
        // | Rule | Queue phase | committed Ended | lease | actor | Effect |
        // |---|---|---|---|---|---|
        // | A1 | Awaiting | no | none | coordinator scan | E1 preserve next epoch |
        // | A2 | Leased | no | expired | coordinator scan | E1 leave recovery to Worker |
        // | A3 | Leased | no | expired | Worker B | E2 reclaim epoch + 1 |
        // | A4 | Leased | no | superseded | Worker A | E3 fenced |
        use awaken_dispatch::SettleOutcome;

        let storage = tempfile::tempdir().expect("storage");
        let now = awaken_time::system_clock().now().as_epoch_ms();
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("dispatch store"),
        );
        let mut coordinator = crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await
            .with_ephemeral_dispatch(store.clone());
        coordinator.execution.ingress = crate::RunIngress::Durable;
        coordinator.execution.local = crate::LocalExecution::RemoteWorkersOnly;
        let coordinator = (coordinator).build().expect("validated Host dependencies");
        let host = coordinator;
        let resolver = crate::host::session_worker_resolver(&host).expect("Host dispatch resolver");

        let parked = claim(
            &store,
            "thread-awaiting",
            "run-awaiting",
            "worker-a",
            now,
            &clock,
        )
        .await;
        assert_eq!(
            store
                .settle(
                    parked.lease.run_id(),
                    parked.lease.epoch(),
                    awaken_dispatch_contract::DispatchOutcome::Awaiting,
                    &[],
                )
                .await
                .expect("park Awaiting row"),
            SettleOutcome::Applied
        );
        let leased = claim(
            &store,
            "thread-leased",
            "run-leased",
            "worker-a",
            now,
            &clock,
        )
        .await;
        let recovery_now = leased.lease.expires_ms().saturating_add(1);

        assert!(
            resolver
                .reconcile_committed_terminals(8)
                .await
                .expect("scan non-terminal rows")
                .is_empty(),
            "A1/A2: non-terminal rows are not maintenance work"
        );

        clock.set(recovery_now);
        let awaiting_probe = store
            .claim_for_terminal_recovery(parked.lease.run_id(), "epoch-probe", DEFAULT_LEASE_MS)
            .await
            .expect("probe Awaiting epoch")
            .expect("Awaiting row remains available to its canonical recovery verb");
        assert_eq!(
            awaiting_probe.lease.epoch(),
            parked.lease.epoch() + 1,
            "A1/E1: coordinator scan did not invisibly claim and restore Awaiting"
        );
        store
            .settle(
                awaiting_probe.lease.run_id(),
                awaiting_probe.lease.epoch(),
                awaken_dispatch_contract::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("restore Awaiting probe");

        clock.set(recovery_now);
        let replacement = store
            .claim_run(
                leased.lease.run_id(),
                "worker-b",
                DEFAULT_LEASE_MS,
                &Default::default(),
            )
            .await
            .expect("Worker B reclaim")
            .expect("A3: expired lease remains reclaimable");
        assert_eq!(replacement.lease.owner(), "worker-b", "A3/E2");
        assert_eq!(replacement.lease.epoch(), leased.lease.epoch() + 1, "A3/E2");
        assert_eq!(
            store
                .settle(
                    leased.lease.run_id(),
                    leased.lease.epoch(),
                    awaken_dispatch_contract::DispatchOutcome::Done,
                    &[],
                )
                .await
                .expect("stale Worker A settle"),
            SettleOutcome::Fenced,
            "A4/E3"
        );
        assert!(
            store
                .lock_commit_epoch(&awaken_dispatch_contract::RunClaim::from(
                    &replacement.lease
                ))
                .await
                .expect("current replacement fence read")
                .is_some(),
            "A4/E3: stale settlement cannot disturb Worker B"
        );
    }

    #[tokio::test]
    async fn host_retry_exhaustion_uses_the_environment_free_boundary_worker() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect table: H1 expired+exhausted exact claim -> boundary Worker
        // commits Indeterminate and Done; H2 no Session/Environment exists -> no
        // realization is attempted; H3 current epoch -> one completion tombstone.
        use awaken_dispatch::WorkerResolver;

        let storage = tempfile::tempdir().expect("storage");
        let now = awaken_time::system_clock().now().as_epoch_ms();
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("dispatch store"),
        );
        let mut coordinator = crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await
            .with_ephemeral_dispatch(store.clone());
        coordinator.execution.ingress = crate::RunIngress::Durable;
        coordinator.execution.local = crate::LocalExecution::RemoteWorkersOnly;
        let coordinator = (coordinator).build().expect("validated Host dependencies");
        let host = coordinator;
        let fresh = claim(
            &store,
            "thread-exhausted",
            "run-exhausted",
            "crashed",
            now,
            &clock,
        )
        .await;
        clock.set(fresh.lease.expires_ms() + 1);
        store
            .claim_run(fresh.lease.run_id(), "crashed", 1_000, &Default::default())
            .await
            .expect("recovery claim")
            .expect("expired row recovers");
        clock.set(now + 2_002);
        let claimed = store
            .claim_retry_exhausted("remote-worker", 1_000, 1)
            .await
            .expect("special claim")
            .expect("exhausted row claims");
        let resolver = crate::host::session_worker_resolver(&host).expect("Host dispatch resolver");
        assert_eq!(
            resolver
                .worker_for_retry_exhausted(&claimed)
                .await
                .expect("exact retry boundary Worker")
                .terminalize_retry_exhausted(&claimed)
                .await
                .expect("H1 terminalization"),
            Some((
                RunId("run-exhausted".into()),
                RunState::Ended(EndCause::Indeterminate),
            )),
            "H1"
        );
        assert!(
            host.realization_executor
                .environments()
                .session_environment("thread-exhausted")
                .await
                .expect("installed Sessions")
                .is_none(),
            "H2"
        );
        assert!(store.list_dispatches().await.expect("H1 rows").is_empty());
        assert_eq!(
            store
                .completion_events_after(0, 10)
                .await
                .expect("H3 completions")
                .len(),
            1,
            "H3"
        );
    }

    #[tokio::test]
    async fn terminal_recovery_opens_the_parent_partition_for_a_logical_child() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 queue Thread differs from parent affinity; C2
        // committed Ended truth exists only in the parent physical partition;
        // C3 the terminal-recovery claim is current. Effects: E1 claim supplies
        // trusted parent affinity before commit open; E2 child logical Run is
        // found and settled Done; E3 no child-named Session/environment is
        // created. Rule P1=C1+C2+C3=>E1+E2+E3. An implementation that opens the
        // queue Thread before claiming leaves the row Awaiting on FS/SQLite.
        use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};
        use awaken_dispatch::SessionChildAdmission;

        let storage = tempfile::tempdir().expect("storage");
        let now = awaken_time::system_clock().now().as_epoch_ms();
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("dispatch store"),
        );
        let mut coordinator = crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await
            .with_ephemeral_dispatch(store.clone());
        coordinator.execution.ingress = crate::RunIngress::Durable;
        coordinator.execution.local = crate::LocalExecution::RemoteWorkersOnly;
        let coordinator = (coordinator).build().expect("validated Host dependencies");
        let host = coordinator;
        let parent = ThreadId("parent-session".into());
        let child = ThreadId("logical-child".into());
        let run = RunId("child-terminal".into());
        store
            .enqueue_session_child(
                awaken_dispatch_contract::RunDispatch::new(test_activation(&child.0, &run.0))
                    .for_session(parent.clone()),
                SessionChildAdmission::new(25, Vec::new()),
            )
            .await
            .expect("admit child");
        clock.set(now);
        let claimed = store
            .claim_run(&run, "setup", DEFAULT_LEASE_MS, &Default::default())
            .await
            .expect("claim child")
            .expect("child claim");
        store
            .settle(
                &run,
                claimed.lease.epoch(),
                awaken_dispatch_contract::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("park child");
        let parent_commit = host
            .authority
            .open_commit(&parent.0)
            .await
            .expect("parent physical commit");
        commit_run(
            &parent_commit,
            &child,
            RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
            Vec::new(),
            Vec::new(),
        )
        .await
        .expect("commit logical child in parent partition");

        let resolver = crate::host::session_worker_resolver(&host).expect("Host dispatch resolver");
        let reconciled = resolver
            .reconcile_committed_terminals(8)
            .await
            .expect("reconcile child");
        assert_eq!(reconciled.len(), 1, "P1/E2");
        assert_eq!(reconciled[0].0, run, "P1/E2");
        assert!(store.list_dispatches().await.unwrap().is_empty(), "P1/E2");
        assert!(
            host.realization_executor
                .environments()
                .session_environment(&child.0)
                .await
                .expect("installed Sessions")
                .is_none(),
            "P1/E3"
        );
    }
}
