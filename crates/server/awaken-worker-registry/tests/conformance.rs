use std::sync::Arc;

use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerManifest, WorkerRegistration, WorkerState,
};
#[cfg(feature = "test-support")]
use awaken_worker_registry::MemoryWorkerDirectory;
use awaken_worker_registry::{PostgresWorkerDirectory, SqliteWorkerDirectory};

fn registration(worker: &str, incarnation: &str) -> WorkerRegistration {
    WorkerRegistration {
        worker_id: worker.to_string(),
        incarnation_id: incarnation.to_string(),
        manifest: WorkerManifest {
            build_digest: "sha256:image-a".to_string(),
            ..WorkerManifest::default()
        },
    }
}

async fn assert_registry(directory: Arc<dyn WorkerDirectory>, worker_id: &str) {
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

async fn assert_exact_expiry(directory: Arc<dyn WorkerDirectory>, prefix: &str) {
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

#[tokio::test]
#[cfg(feature = "test-support")]
async fn memory_registry_conforms() {
    // Cause/effect decision table:
    // R1 product/default feature set -> MemoryWorkerDirectory is not exported;
    // R2 explicit test-support -> the volatile adapter remains available and
    // must obey the same transition kernel as durable adapters. Compile-time
    // fitness owns R1; this conformance case owns R2.
    let directory = Arc::new(MemoryWorkerDirectory::new());
    assert_registry(directory.clone(), "dwr-memory").await;
    assert_expiry_race(directory.clone(), directory, "memory").await;
}

#[tokio::test]
async fn sqlite_registry_conforms_and_reopens() {
    // Cause/effect graph: durable path -> transition sequence -> process closes
    // -> same path reopens -> generation/tombstone remain authoritative.
    // Decision table: S1 writable path + valid ledger -> mutations persist;
    // S2 reopen same path -> generation/state survive; S3 volatile constructor
    // is unavailable to product builds (compile-time fitness coverage).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workers.sqlite");
    let path = path.to_str().unwrap();
    let store = Arc::new(SqliteWorkerDirectory::open(path).unwrap());
    assert_registry(store, "dwr-sqlite").await;
    let reopened = SqliteWorkerDirectory::open(path).unwrap();
    let record: RegisteredWorker = reopened
        .current("dwr-sqlite")
        .await
        .unwrap()
        .expect("tombstone survives restart");
    assert_eq!(record.snapshot.identity.generation, 2);
    assert_eq!(record.snapshot.state, WorkerState::Dead);
    assert_expiry_race(
        Arc::new(reopened),
        Arc::new(SqliteWorkerDirectory::open(path).unwrap()),
        "sqlite",
    )
    .await;
}

#[tokio::test]
async fn postgres_registry_conforms_when_configured() {
    // Cause/effect decision table for schema access:
    // R1 migration connect succeeds -> the registry bundle is recorded.
    // R2 R1 ledger + connect_existing -> startup verification succeeds and the
    // canonical transition kernel remains usable.
    // R3 missing/drifted ledger -> connect_existing fails closed (covered by the
    // shared scoped-migration verify suite used by this adapter).
    let Ok(url) = std::env::var("AWAKEN_TEST_POSTGRES_URL") else {
        eprintln!("AWAKEN_TEST_POSTGRES_URL not set; skipping Postgres registry conformance");
        return;
    };
    PostgresWorkerDirectory::connect(&url, 5).await.unwrap();
    let store = Arc::new(
        PostgresWorkerDirectory::connect_existing(&url, 5)
            .await
            .unwrap(),
    );
    sqlx::query("DELETE FROM worker_registry_worker WHERE worker_id = 'dwr-postgres'")
        .execute(&store.pool())
        .await
        .unwrap();
    assert_registry(store.clone(), "dwr-postgres").await;
    assert_expiry_race(
        store,
        Arc::new(
            PostgresWorkerDirectory::connect_existing(&url, 5)
                .await
                .unwrap(),
        ),
        "postgres",
    )
    .await;
}

async fn assert_expiry_race(
    first: Arc<dyn WorkerDirectory>,
    second: Arc<dyn WorkerDirectory>,
    backend: &str,
) {
    // R9 uses independent SQLite connections / PostgreSQL pools, not one local
    // Mutex standing in for database serialization. All three requests name
    // the original identity. The old-time heartbeat races exact expiry and
    // replacement: renewed old owner OR fresh successor, never resurrection.
    // Repetition stresses adapter scheduling; Loom above exhausts mutex orders.
    for round in 0..16 {
        let id = format!("expiry-race-{backend}-{}-{round}", std::process::id());
        let old = first
            .register(registration(&id, "old"), 100, 50)
            .await
            .unwrap()
            .snapshot
            .identity;
        let heartbeat = WorkerHeartbeat {
            sequence: 1,
            ready: true,
            in_flight: 2,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
        };
        let (renewed, expired, replacement) = tokio::join!(
            first.heartbeat(&old, heartbeat, 120, 100),
            second.expire_incarnation(&old, 150),
            first.register(registration(&id, "new"), 151, 100),
        );
        let renewed = renewed.unwrap();
        let expired = expired.unwrap();
        let current = second.current(&id).await.unwrap().unwrap();
        if current.snapshot.identity == old {
            assert_eq!(current.snapshot.expires_at_ms, 220);
            assert_eq!(current.snapshot.state, WorkerState::Ready);
            assert_eq!(renewed, RegistryMutation::Applied);
            assert_eq!(expired, RegistryMutation::InvalidTransition);
            assert!(matches!(
                replacement,
                Err(RegistryError::SlotOccupied { .. })
            ));
        } else {
            assert_eq!(current, replacement.unwrap());
            assert_eq!(current.snapshot.identity.generation, 2);
            assert_eq!(current.heartbeat_sequence, 0);
            assert_eq!(current.snapshot.state, WorkerState::Starting);
            assert!(matches!(
                renewed,
                RegistryMutation::InvalidTransition | RegistryMutation::StaleIncarnation
            ));
            assert!(matches!(
                expired,
                RegistryMutation::Applied | RegistryMutation::StaleIncarnation
            ));
        }
    }
}
