// @formal surface exclude=test_support_crate
// @formal owner crates/platform/dispatch/awaken-worker-registry/tests/conformance.rs
// @formal reason: Trait-generic WorkerDirectory conformance suite (ADR-0081 §7). It is a tooling-only test
// @formal reason: library every backend's test target calls; no product crate links or invokes it, and it
// @formal reason: admits nothing at runtime.
//! Trait-generic conformance suite for the `WorkerDirectory` port (ADR-0081 §7).
//!
//! Each `assert_*` law takes an `Arc<dyn WorkerDirectory>`; a backend passes a
//! fresh directory and, for the reopen laws, a second handle on the same
//! durable state. Registration, heartbeat receipts, process-exit receipts,
//! exact expiry, the expiry race and registration history are the port's
//! contract.

use awaken_worker_contract::{
    RegistryError, RegistryMutation, WorkerCreateLedger, WorkerCurrentSource, WorkerDirectory,
    WorkerHeartbeat, WorkerIncarnationSource, WorkerManifest, WorkerProcessExit,
    WorkerRegistration, WorkerState,
};
use std::sync::Arc;

pub fn registration(worker: &str, incarnation: &str) -> WorkerRegistration {
    WorkerRegistration {
        worker_id: worker.to_string(),
        incarnation_id: incarnation.to_string(),
        manifest: WorkerManifest {
            build_digest: "sha256:image-a".to_string(),
            runtime_protocol: awaken_worker_contract::VersionRange::exact(
                awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
            ),
            ..WorkerManifest::default()
        },
        create_ledger: None,
    }
}

pub async fn assert_create_ledger_anchor(
    first: Arc<dyn WorkerDirectory>,
    second: Arc<dyn WorkerDirectory>,
    backend: &str,
) {
    // Causes: C1 first registration or exact retry; C2 ledger absent, exact or
    // same-name/replaced UID; C3 current or historical incarnation. Effects:
    // E1 first registration atomically freezes the UID, E2 exact replay does
    // not replace it, E3 changed/backfilled UID is rejected, E4 replacement
    // generation cannot erase the old witness. Rules L1=first+exact=>E1/E2,
    // L2=retry+changed/absent=>E3, L3=legacy+backfill=>E3,
    // L4=new generation=>E4, L5=two first registrations with different UIDs
    // => exactly one wins the database transaction. Independent stores must
    // see the same result.
    // R1/R3/R11: mutation setup stays with Directory, while exact historical
    // bytes are read through its actual erased observation role. A successor
    // must not replace the old anchor; legacy absence remains absence.
    let first_observations: Arc<dyn WorkerIncarnationSource> = first.clone();
    let first_current: Arc<dyn WorkerCurrentSource> = first.clone();
    let second_observations: Arc<dyn WorkerIncarnationSource> = second.clone();
    let worker = format!("ledger-{backend}");
    let ledger = |uid| {
        WorkerCreateLedger::new(
            "kubernetes",
            "awaken-control",
            "worker-ledger",
            uid,
            "config-a",
        )
        .unwrap()
    };
    let original = ledger("uid-original");
    let mut first_registration = registration(&worker, "first");
    first_registration.create_ledger = Some(original.clone());
    let old = first
        .register(first_registration.clone(), 100, 50)
        .await
        .unwrap();
    assert_eq!(old.create_ledger, Some(original.clone()), "L1/E1");
    // R2: each full-identity axis is part of the actual backend point read;
    // neither the current logical row nor another boot is a substitute.
    for axis in ["worker", "generation", "boot"] {
        let mut missing = old.snapshot.identity.clone();
        match axis {
            "worker" => missing.worker_id.push_str("-missing"),
            "generation" => missing.generation += 1,
            "boot" => missing.incarnation_id.push_str("-missing"),
            _ => unreachable!(),
        }
        assert!(
            first_observations
                .incarnation(&missing)
                .await
                .unwrap()
                .is_none(),
            "R2/{axis}"
        );
    }
    assert_eq!(
        second
            .register(first_registration.clone(), 101, 50)
            .await
            .unwrap(),
        old,
        "L1/E2 exact response-loss replay"
    );
    let mut changed = first_registration.clone();
    changed.create_ledger = Some(ledger("uid-replaced"));
    assert!(
        matches!(
            second.register(changed, 102, 50).await,
            Err(RegistryError::CreateLedgerChanged)
        ),
        "L2/E3 same-name replacement"
    );
    let mut absent = first_registration;
    absent.create_ledger = None;
    assert!(
        matches!(
            second.register(absent, 103, 50).await,
            Err(RegistryError::CreateLedgerChanged)
        ),
        "L2/E3 missing ledger"
    );
    assert_eq!(
        second_observations
            .incarnation(&old.snapshot.identity)
            .await
            .unwrap()
            .unwrap()
            .create_ledger,
        Some(original.clone()),
        "L2/E1 original UID remains frozen"
    );

    let legacy_worker = format!("ledger-legacy-{backend}");
    let legacy = first
        .register(registration(&legacy_worker, "first"), 100, 50)
        .await
        .unwrap();
    let mut backfill = registration(&legacy_worker, "first");
    backfill.create_ledger = Some(ledger("uid-new"));
    assert!(
        matches!(
            second.register(backfill, 101, 50).await,
            Err(RegistryError::CreateLedgerChanged)
        ),
        "L3/E3 historical unbound execution cannot gain proof"
    );
    assert!(
        second_observations
            .incarnation(&legacy.snapshot.identity)
            .await
            .unwrap()
            .unwrap()
            .create_ledger
            .is_none()
    );

    first.deregister(&old.snapshot.identity).await.unwrap();
    let mut replacement = registration(&worker, "second");
    replacement.create_ledger = Some(ledger("uid-successor"));
    let newer = second.register(replacement, 200, 50).await.unwrap();
    assert_eq!(
        newer.snapshot.identity.generation,
        old.snapshot.identity.generation + 1
    );
    assert_eq!(newer.create_ledger, Some(ledger("uid-successor")), "L4/E4");
    assert_eq!(
        first_observations
            .incarnation(&old.snapshot.identity)
            .await
            .unwrap()
            .unwrap()
            .create_ledger,
        Some(original),
        "L4/E4 old UID remains independently readable"
    );

    let race_worker = format!("ledger-race-{backend}");
    let mut left = registration(&race_worker, "same-boot");
    left.create_ledger = Some(ledger("race-a"));
    let mut right = registration(&race_worker, "same-boot");
    right.create_ledger = Some(ledger("race-b"));
    let (left_result, right_result) = tokio::join!(
        first.register(left, 300, 50),
        second.register(right, 300, 50),
    );
    assert_eq!(
        left_result.is_ok() as u8 + right_result.is_ok() as u8,
        1,
        "L5 one UID wins"
    );
    assert!(
        left_result
            .as_ref()
            .err()
            .or(right_result.as_ref().err())
            .is_some_and(|error| matches!(error, RegistryError::CreateLedgerChanged)),
        "L5 conflicting UID is rejected"
    );
    let winner = first_current.current(&race_worker).await.unwrap().unwrap();
    assert!(
        winner.create_ledger == Some(ledger("race-a"))
            || winner.create_ledger == Some(ledger("race-b")),
        "L5 durable exact winner"
    );
}

pub async fn assert_registry(directory: Arc<dyn WorkerDirectory>, worker_id: &str) {
    let first = directory
        .register(registration(worker_id, "boot-1"), 100, 50)
        .await
        .unwrap();
    assert_eq!(first.snapshot.identity.generation, 1);
    assert_eq!(first.snapshot.state, WorkerState::Starting);

    let idempotent = directory
        .register(registration(worker_id, "boot-1"), 110, 50)
        .await
        .unwrap();
    assert_eq!(
        idempotent, first,
        "register retry must not extend authority"
    );

    let mut changed = registration(worker_id, "boot-1");
    changed.manifest.build_digest = "sha256:changed".to_string();
    assert!(matches!(
        directory.register(changed, 110, 50).await,
        Err(RegistryError::ManifestChanged)
    ));

    assert!(matches!(
        directory
            .register(registration(worker_id, "boot-2"), 120, 50)
            .await,
        Err(RegistryError::SlotOccupied { .. })
    ));

    let identity = first.snapshot.identity.clone();
    assert_eq!(
        directory
            .heartbeat(
                &identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 2,
                    warm_environment_shapes: Default::default(),
                    credential_observations: Default::default(),
                    acp_capability_observations: Default::default(),
                    source_retention: Default::default(),
                },
                120,
                50,
            )
            .await
            .unwrap(),
        RegistryMutation::Applied
    );
    let ready = directory.current(worker_id).await.unwrap().unwrap();
    assert_eq!(ready.snapshot.state, WorkerState::Ready);
    assert_eq!(ready.snapshot.in_flight, 2);
    assert_eq!(ready.snapshot.expires_at_ms, 170);

    assert_eq!(
        directory
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
                130,
                50,
            )
            .await
            .unwrap(),
        RegistryMutation::StaleSequence
    );
    assert_eq!(
        directory.begin_drain(&identity, 200).await.unwrap(),
        RegistryMutation::Applied
    );
    // R1 a draining heartbeat's source-retention report is persisted with
    // the incarnation; R2 the next heartbeat replaces it whole.
    let reported_retention =
        std::collections::BTreeSet::from([awaken_worker_contract::WorkerSourceRetention {
            session_id: "retained-session".into(),
            subject: "subject-digest".into(),
            retained: true,
        }]);
    assert_eq!(
        directory
            .heartbeat(
                &identity,
                WorkerHeartbeat {
                    sequence: 2,
                    ready: true,
                    in_flight: 1,
                    warm_environment_shapes: Default::default(),
                    credential_observations: Default::default(),
                    acp_capability_observations: Default::default(),
                    source_retention: reported_retention.clone(),
                },
                140,
                50,
            )
            .await
            .unwrap(),
        RegistryMutation::Applied
    );
    let draining = directory.current(worker_id).await.unwrap().unwrap();
    assert_eq!(draining.snapshot.state, WorkerState::Draining);
    assert_eq!(draining.snapshot.in_flight, 1);
    assert_eq!(draining.source_retention, reported_retention, "R1");
    assert_eq!(
        directory.mark_quiesced(&identity).await.unwrap(),
        RegistryMutation::InvalidTransition
    );
    directory
        .heartbeat(
            &identity,
            WorkerHeartbeat {
                sequence: 3,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                source_retention: Default::default(),
            },
            150,
            50,
        )
        .await
        .unwrap();
    assert_eq!(
        directory.mark_quiesced(&identity).await.unwrap(),
        RegistryMutation::Applied
    );
    assert!(
        directory
            .current(worker_id)
            .await
            .unwrap()
            .unwrap()
            .source_retention
            .is_empty(),
        "R2 a later heartbeat replaces the report"
    );

    let second = directory
        .register(registration(worker_id, "boot-2"), 160, 50)
        .await
        .unwrap();
    assert_eq!(second.snapshot.identity.generation, 2);
    assert_eq!(
        directory
            .heartbeat(
                &identity,
                WorkerHeartbeat {
                    sequence: 4,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: Default::default(),
                    credential_observations: Default::default(),
                    acp_capability_observations: Default::default(),
                    source_retention: Default::default(),
                },
                170,
                50,
            )
            .await
            .unwrap(),
        RegistryMutation::StaleIncarnation
    );

    assert_eq!(
        directory
            .expire_incarnation(&second.snapshot.identity, 211)
            .await
            .unwrap(),
        RegistryMutation::Applied
    );
    let tombstone = directory.current(worker_id).await.unwrap().unwrap();
    assert_eq!(tombstone.snapshot.state, WorkerState::Dead);
    assert_eq!(
        directory
            .list()
            .await
            .unwrap()
            .iter()
            .filter(|record| record.snapshot.identity.worker_id == worker_id)
            .count(),
        1,
        "one durable record for this logical Worker; unrelated Workers are allowed"
    );
    assert_exact_expiry(directory, worker_id).await;
}

pub async fn assert_process_exit_receipt(
    directory: Arc<dyn WorkerDirectory>,
    prefix: &str,
) -> awaken_worker_contract::WorkerIdentity {
    // Worker-process receipt cause/effect graph: C1 exact historical identity;
    // C2 Worker state open/closed; C3 receipt absent/equal/conflicting; C4 a
    // successor generation may become current. Effects: E1 open/missing facts
    // cannot write; E2 the first closed exact receipt is stored; E3 exact replay
    // stutters; E4 conflict cannot overwrite; E5 replacement keeps the old
    // receipt addressable while the new generation has none.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | P1 | no | any | any | no | E1 NotFound |
    // | P2 | yes | open | absent | no | E1 InvalidTransition |
    // | P3 | yes | closed | absent | no | E2 Applied |
    // | P4 | yes | closed | equal | no | E3 Applied, unchanged |
    // | P5 | yes | closed | conflict | no | E4 InvalidTransition |
    // | P6 | yes | closed | equal | yes | E5 historical read |
    // R1/R2/R3: the read-only role preserves the exact old exit after replacement;
    // it cannot borrow the successor's record or backfill missing evidence.
    let observations: Arc<dyn WorkerIncarnationSource> = directory.clone();
    let worker_id = format!("{prefix}-process-exit-{}", std::process::id());
    let missing = awaken_worker_contract::WorkerIdentity::new("missing", "boot", 1);
    let receipt = WorkerProcessExit::new("cluster", "pod-uid-a", "resource-version-7").unwrap();
    assert_eq!(
        directory
            .record_process_exit(&missing, receipt.clone())
            .await
            .unwrap(),
        RegistryMutation::NotFound,
        "P1"
    );
    let old = directory
        .register(registration(&worker_id, "boot-a"), 0, 10)
        .await
        .unwrap();
    let identity = old.snapshot.identity.clone();
    assert_eq!(
        directory
            .record_process_exit(&identity, receipt.clone())
            .await
            .unwrap(),
        RegistryMutation::InvalidTransition,
        "P2"
    );
    assert_eq!(
        directory.expire_incarnation(&identity, 10).await.unwrap(),
        RegistryMutation::Applied
    );
    assert_eq!(
        directory
            .record_process_exit(&identity, receipt.clone())
            .await
            .unwrap(),
        RegistryMutation::Applied,
        "P3"
    );
    assert_eq!(
        directory
            .record_process_exit(&identity, receipt.clone())
            .await
            .unwrap(),
        RegistryMutation::Applied,
        "P4"
    );
    let conflicting = WorkerProcessExit::new("cluster", "pod-uid-a", "resource-version-8").unwrap();
    assert_eq!(
        directory
            .record_process_exit(&identity, conflicting)
            .await
            .unwrap(),
        RegistryMutation::InvalidTransition,
        "P5"
    );
    let replacement = directory
        .register(registration(&worker_id, "boot-b"), 11, 10)
        .await
        .unwrap();
    assert_eq!(replacement.snapshot.identity.generation, 2);
    assert_eq!(replacement.process_exit, None);
    assert_eq!(
        observations
            .incarnation(&identity)
            .await
            .unwrap()
            .unwrap()
            .process_exit,
        Some(receipt),
        "P6"
    );
    identity
}

pub async fn assert_exact_expiry(directory: Arc<dyn WorkerDirectory>, prefix: &str) {
    // Cause/effect table (all stores execute this same trace): E1 missing ->
    // NotFound; E2 another incarnation/generation -> StaleIncarnation, no write;
    // E3 exact unexpired Starting/Ready/Draining -> InvalidTransition, no write;
    // E4 exact expiry equality/past -> Dead even with in-flight work; E5 exact
    // already Quiesced/Dead -> Applied without rewriting; E6 duplicate response
    // loss, delayed old-time heartbeat/register -> closed record is unchanged;
    // E7 replacement -> old expiry cannot affect the successor; E8 renewal
    // before expiry -> the transaction uses the new deadline, not an old read.
    // This closes Registry authority only. In-flight work need not have stopped,
    // and no rule grants Dispatch finish/commit or proves external termination.
    let base = format!("{prefix}-expiry-{}", std::process::id());
    let missing = awaken_worker_contract::WorkerIdentity::new(&base, "absent", 1);
    assert_eq!(
        directory.expire_incarnation(&missing, 150).await.unwrap(),
        RegistryMutation::NotFound,
        "E1"
    );
    let peer_id = format!("{base}-peer");
    let peer = directory
        .register(registration(&peer_id, "peer"), 0, 1)
        .await
        .unwrap();
    for state in [
        WorkerState::Starting,
        WorkerState::Ready,
        WorkerState::Draining,
        WorkerState::Quiesced,
        WorkerState::Dead,
    ] {
        for now in [149, 150, 151] {
            let id = format!("{base}-{state:?}-{now}");
            let first = directory
                .register(registration(&id, "old"), 100, 50)
                .await
                .unwrap();
            let identity = first.snapshot.identity;
            let heartbeat = |sequence| WorkerHeartbeat {
                sequence,
                ready: true,
                in_flight: if state == WorkerState::Quiesced { 0 } else { 2 },
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                source_retention: Default::default(),
            };
            if state != WorkerState::Starting {
                directory
                    .heartbeat(&identity, heartbeat(1), 100, 50)
                    .await
                    .unwrap();
            }
            if matches!(state, WorkerState::Draining | WorkerState::Quiesced) {
                directory.begin_drain(&identity, 180).await.unwrap();
            }
            if state == WorkerState::Quiesced {
                directory.mark_quiesced(&identity).await.unwrap();
            }
            if state == WorkerState::Dead {
                directory.deregister(&identity).await.unwrap();
            }
            let before = directory.current(&id).await.unwrap().unwrap();
            for change_generation in [false, true] {
                let mut foreign = identity.clone();
                if change_generation {
                    foreign.generation += 1;
                } else {
                    foreign.incarnation_id = "foreign".into();
                }
                assert_eq!(
                    directory.expire_incarnation(&foreign, 151).await.unwrap(),
                    RegistryMutation::StaleIncarnation,
                    "E2"
                );
                assert_eq!(
                    directory.current(&id).await.unwrap(),
                    Some(before.clone()),
                    "E2 no write"
                );
            }
            let closed = matches!(state, WorkerState::Quiesced | WorkerState::Dead);
            let accepted = closed || now >= 150;
            assert_eq!(
                directory.expire_incarnation(&identity, now).await.unwrap(),
                if accepted {
                    RegistryMutation::Applied
                } else {
                    RegistryMutation::InvalidTransition
                },
                "E3..E5"
            );
            let after = directory.current(&id).await.unwrap().unwrap();
            if !accepted || closed {
                assert_eq!(after, before, "E3/E5 no rewrite");
            } else {
                let mut expected = before;
                expected.snapshot.state = WorkerState::Dead;
                assert_eq!(
                    after, expected,
                    "E4 only state changes, not in-flight or evidence"
                );
            }
            if accepted {
                assert_eq!(
                    directory.expire_incarnation(&identity, now).await.unwrap(),
                    RegistryMutation::Applied,
                    "E6 duplicate"
                );
                assert_eq!(
                    directory
                        .heartbeat(&identity, heartbeat(2), 120, 100)
                        .await
                        .unwrap(),
                    RegistryMutation::InvalidTransition,
                    "E6 delayed heartbeat"
                );
                assert_eq!(
                    directory
                        .register(registration(&id, "old"), 120, 100)
                        .await
                        .unwrap(),
                    after,
                    "E6 register retry"
                );
                let replacement = directory
                    .register(registration(&id, "new"), 151, 100)
                    .await
                    .unwrap();
                assert_eq!(
                    directory.expire_incarnation(&identity, 999).await.unwrap(),
                    RegistryMutation::StaleIncarnation,
                    "E7"
                );
                assert_eq!(
                    directory.current(&id).await.unwrap(),
                    Some(replacement),
                    "E7 successor retained"
                );
            } else {
                assert_eq!(
                    directory
                        .heartbeat(&identity, heartbeat(2), 120, 100)
                        .await
                        .unwrap(),
                    RegistryMutation::Applied
                );
                let renewed = directory.current(&id).await.unwrap();
                assert_eq!(
                    directory.expire_incarnation(&identity, 150).await.unwrap(),
                    RegistryMutation::InvalidTransition,
                    "E8"
                );
                assert_eq!(
                    directory.current(&id).await.unwrap(),
                    renewed,
                    "E8 renewed authority retained"
                );
            }
            assert_eq!(
                directory.current(&peer_id).await.unwrap(),
                Some(peer.clone()),
                "exact expiry never sweeps a peer, even an expired one"
            );
        }
    }
}
