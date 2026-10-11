// @formal evidence worker.drain_and_replacement_races_preserve_registry_authority
// @formal evidence worker.incarnation_generation_fences_stale_processes
//!
//! The laws live in `awaken_store_conformance::worker_directory`; this file only
//! supplies the backends.

use awaken_store_conformance::worker_directory::*;
use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerCurrentSource, WorkerDirectory,
    WorkerHeartbeat, WorkerIncarnationSource, WorkerObservationSource, WorkerState,
};
#[cfg(feature = "test-support")]
use awaken_worker_registry::MemoryWorkerDirectory;
use awaken_worker_registry::{PostgresWorkerDirectory, SqliteWorkerDirectory};
use std::sync::Arc;

struct CurrentOnlyObservation {
    record: Option<RegisteredWorker>,
    unavailable: bool,
    reads: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl WorkerObservationSource for CurrentOnlyObservation {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        panic!("an exact-current fallback must never scan the list")
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerCurrentSource for CurrentOnlyObservation {
    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.unavailable {
            return Err(RegistryError::Persistence(
                "fixture current read unavailable".into(),
            ));
        }
        Ok(self
            .record
            .clone()
            .filter(|record| record.snapshot.identity.worker_id == worker_id))
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for CurrentOnlyObservation {
    async fn incarnation(
        &self,
        identity: &awaken_worker_contract::WorkerIdentity,
    ) -> Result<
        Option<awaken_worker_contract::RegisteredWorker>,
        awaken_worker_contract::RegistryError,
    > {
        awaken_worker_contract::read_current_incarnation(self, identity).await
    }
}

struct ListOnlyObservation {
    record: RegisteredWorker,
    reads: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl WorkerObservationSource for ListOnlyObservation {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(vec![self.record.clone()])
    }
}

#[tokio::test]
async fn observation_current_fallback_keeps_exact_identity_absence_and_error() {
    // R4: current exists/exact -> same record; absent or any full-identity axis
    // differs -> None; current refusal -> the same error. Each row performs
    // exactly one point read and no list, preserving the former Directory
    // fallback rather than manufacturing historical query capability.
    let temporary = tempfile::tempdir().unwrap();
    let directory = SqliteWorkerDirectory::open(temporary.path().join("current.sqlite"))
        .await
        .unwrap();
    let record = directory
        .register(registration("read-current", "boot"), 100, 50)
        .await
        .unwrap();
    for rule in [
        "exact",
        "worker",
        "generation",
        "boot",
        "absent",
        "unavailable",
    ] {
        let source = CurrentOnlyObservation {
            record: (rule != "absent").then(|| record.clone()),
            unavailable: rule == "unavailable",
            reads: Default::default(),
        };
        let mut identity = record.snapshot.identity.clone();
        match rule {
            "worker" => identity.worker_id.push_str("-other"),
            "generation" => identity.generation += 1,
            "boot" => identity.incarnation_id.push_str("-other"),
            _ => {}
        }
        let result = source.incarnation(&identity).await;
        if rule == "unavailable" {
            assert!(
                matches!(result, Err(RegistryError::Persistence(ref error)) if error == "fixture current read unavailable"),
                "R4/{rule}"
            );
        } else {
            assert_eq!(
                result.unwrap(),
                (rule == "exact").then(|| record.clone()),
                "R4/{rule}"
            );
        }
        assert_eq!(source.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn list_only_observation_never_infers_current_or_historical_point_reads() {
    // R5: even a matching roster record grants no point-query capability.
    // The WorkerObservationSource compile-fail examples own refusal of current
    // and incarnation calls; this positive case proves the same list-only type
    // can return the exact record through its sole supported operation. R12
    // retains the separate compile-fail mutation boundary.
    let temporary = tempfile::tempdir().unwrap();
    let directory = SqliteWorkerDirectory::open(temporary.path().join("list.sqlite"))
        .await
        .unwrap();
    let record = directory
        .register(registration("read-list", "boot"), 100, 50)
        .await
        .unwrap();
    let source = Arc::new(ListOnlyObservation {
        record: record.clone(),
        reads: Default::default(),
    });
    let observations: Arc<dyn WorkerObservationSource> = source.clone();
    assert_eq!(
        observations.list().await.unwrap(),
        vec![record],
        "R5 list is supported"
    );
    assert_eq!(source.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
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
    assert_create_ledger_anchor(directory.clone(), directory.clone(), "memory").await;
    let _ = assert_process_exit_receipt(directory.clone(), "memory").await;
    assert_registration_history(directory.clone(), directory.clone(), "memory").await;
    assert_expiry_race(directory.clone(), directory, "memory").await;
}

async fn assert_incarnation_exclusion(
    first: Arc<dyn WorkerDirectory>,
    second: Arc<dyn WorkerDirectory>,
    backend: &str,
) {
    // Causes C1 exact current incarnation; C2 admission guard is held; C3 close,
    // expiry or replacement races the final Dispatch admission. R1 C1+C2 ->
    // closure cannot commit until guard release. R2 release -> exact closure
    // commits; a new guard observes Dead and cannot mint execution authority.
    // Independent SQLite connections/PG pools prove database exclusion rather
    // than a test-side mutex. Stale identity returns no guard and no mutation.
    let id = format!("guard-{backend}-{}", std::process::id());
    let identity = first
        .register(registration(&id, "guarded"), 100, 50)
        .await
        .unwrap()
        .snapshot
        .identity;
    let held = first.lock_incarnation(&identity).await.unwrap().unwrap();
    assert_eq!(held.snapshot().identity, identity);
    let next = second.clone();
    let expected = identity.clone();
    let task = tokio::spawn(async move { next.deregister(&expected).await.unwrap() });
    let mut task = Box::pin(task);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut task)
            .await
            .is_err(),
        "R1 closed transition remains excluded"
    );
    held.release().await.unwrap();
    assert_eq!(task.await.unwrap(), RegistryMutation::Applied, "R2");
    let closed = first.lock_incarnation(&identity).await.unwrap().unwrap();
    assert_eq!(
        closed.snapshot().state,
        WorkerState::Dead,
        "R2 exact closed truth"
    );
    closed.release().await.unwrap();
    let mut stale = identity;
    stale.generation += 1;
    assert!(
        first.lock_incarnation(&stale).await.unwrap().is_none(),
        "R3 stale identity"
    );
}

#[tokio::test]
#[cfg(feature = "test-support")]
async fn memory_incarnation_exclusion_serializes_closure() {
    let directory: Arc<dyn WorkerDirectory> = Arc::new(MemoryWorkerDirectory::new());
    assert_incarnation_exclusion(directory.clone(), directory, "memory").await;
}

#[tokio::test]
async fn sqlite_incarnation_exclusion_serializes_independent_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("guards.sqlite");
    assert_incarnation_exclusion(
        Arc::new(SqliteWorkerDirectory::open(&path).await.unwrap()),
        Arc::new(SqliteWorkerDirectory::open(&path).await.unwrap()),
        "sqlite",
    )
    .await;
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
    let store = Arc::new(SqliteWorkerDirectory::open(path).await.unwrap());
    assert_registry(store.clone(), "dwr-sqlite").await;
    assert_create_ledger_anchor(
        store.clone(),
        Arc::new(SqliteWorkerDirectory::open(path).await.unwrap()),
        "sqlite",
    )
    .await;
    let process_identity = assert_process_exit_receipt(store, "sqlite").await;
    let reopened = SqliteWorkerDirectory::open(path).await.unwrap();
    let record: RegisteredWorker = reopened
        .current("dwr-sqlite")
        .await
        .unwrap()
        .expect("tombstone survives restart");
    assert_eq!(record.snapshot.identity.generation, 2);
    assert_eq!(record.snapshot.state, WorkerState::Dead);
    assert_eq!(
        reopened
            .incarnation(&process_identity)
            .await
            .unwrap()
            .unwrap()
            .process_exit
            .unwrap()
            .evidence_ref(),
        "resource-version-7",
        "P6 receipt survives SQLite reopen"
    );
    let reopened = Arc::new(reopened);
    assert_registration_history(
        reopened.clone(),
        Arc::new(SqliteWorkerDirectory::open(path).await.unwrap()),
        "sqlite",
    )
    .await;
    drop(reopened);
    let reopened = SqliteWorkerDirectory::open(path).await.unwrap();
    assert!(
        matches!(
            reopened
                .register(
                    registration(
                        &format!("history-sqlite-{}-starting-false", std::process::id()),
                        "A"
                    ),
                    500,
                    50
                )
                .await,
            Err(RegistryError::HistoricalIncarnation { .. })
        ),
        "H9 retained history survives close/reopen"
    );
    assert_expiry_race(
        Arc::new(reopened),
        Arc::new(SqliteWorkerDirectory::open(path).await.unwrap()),
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
    let url = match std::env::var("AWAKEN_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("AWAKEN_TEST_DATABASE_URL not set; skipping Postgres registry conformance");
            return;
        }
        Err(error) => panic!("invalid PostgreSQL test configuration: {error}"),
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
    sqlx::query("DELETE FROM worker_registry_worker WHERE worker_id IN ('ledger-postgres', 'ledger-legacy-postgres', 'ledger-race-postgres')")
        .execute(&store.pool())
        .await
        .unwrap();
    assert_registry(store.clone(), "dwr-postgres").await;
    assert_create_ledger_anchor(
        store.clone(),
        Arc::new(
            PostgresWorkerDirectory::connect_existing(&url, 5)
                .await
                .unwrap(),
        ),
        "postgres",
    )
    .await;
    sqlx::query("DELETE FROM worker_registry_worker WHERE worker_id IN ('ledger-postgres', 'ledger-legacy-postgres', 'ledger-race-postgres')")
        .execute(&store.pool())
        .await
        .unwrap();
    let process_identity = assert_process_exit_receipt(store.clone(), "postgres").await;
    assert_eq!(
        PostgresWorkerDirectory::connect_existing(&url, 5)
            .await
            .unwrap()
            .incarnation(&process_identity)
            .await
            .unwrap()
            .unwrap()
            .process_exit
            .unwrap()
            .evidence_ref(),
        "resource-version-7",
        "P6 receipt survives PostgreSQL reconnect"
    );
    assert_registration_history(
        store.clone(),
        Arc::new(
            PostgresWorkerDirectory::connect_existing(&url, 5)
                .await
                .unwrap(),
        ),
        "postgres",
    )
    .await;
    assert_expiry_race(
        store.clone(),
        Arc::new(
            PostgresWorkerDirectory::connect_existing(&url, 5)
                .await
                .unwrap(),
        ),
        "postgres",
    )
    .await;
    assert_incarnation_exclusion(
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

async fn assert_registration_history(
    first: Arc<dyn WorkerDirectory>,
    second: Arc<dyn WorkerDirectory>,
    backend: &str,
) {
    // Cause/effect graph: C1 proposal is current B / historical A / fresh C;
    // C2 current B is Starting/Ready/Draining/Quiesced/Dead; C3 clock before/at
    // expiry; C4 proposal manifest unchanged/changed; C5 independent connections
    // race registration; C6 another Worker id shares an incarnation string.
    // Ordered decision rules: H1 current+same manifest -> exact replay, no lease
    // extension, even closed/expired; H2 current+changed manifest -> reject;
    // H3 historical+either manifest -> HistoricalIncarnation, never a fresh gen;
    // H4 fresh+(closed OR expired) -> next gen; H5 fresh+live -> SlotOccupied.
    // H6 list/current show only max generation, including closed current rows;
    // H7 old heartbeat/drain/expiry cannot mutate retained history or successor;
    // H8 race historical A vs fresh C -> only C can win; H9 durable reopen keeps
    // H3 (SQLite caller closes both connections); H10 history is Worker-scoped.
    // Full C1 x C2 x C3 table, C4 on both retry classes, and C5 on replaceable
    // rows. H11 missing-history upgrade is owned by schema migration tests.
    for state in ["starting", "ready", "draining", "quiesced", "dead"] {
        for expired in [false, true] {
            let id = format!("history-{backend}-{}-{state}-{expired}", std::process::id());
            let a = first
                .register(registration(&id, "A"), 100, 50)
                .await
                .unwrap();
            let b = first
                .register(registration(&id, "B"), 150, 50)
                .await
                .unwrap();
            let identity = b.snapshot.identity.clone();
            let beat = WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            };
            if state == "ready" {
                assert_eq!(
                    first
                        .heartbeat(&identity, beat.clone(), 150, 50)
                        .await
                        .unwrap(),
                    RegistryMutation::Applied
                );
            }
            if matches!(state, "draining" | "quiesced") {
                assert_eq!(
                    first.begin_drain(&identity, 190).await.unwrap(),
                    RegistryMutation::Applied
                );
            }
            if state == "quiesced" {
                assert_eq!(
                    first.mark_quiesced(&identity).await.unwrap(),
                    RegistryMutation::Applied
                );
            }
            if state == "dead" {
                assert_eq!(
                    first.deregister(&identity).await.unwrap(),
                    RegistryMutation::Applied
                );
            }
            let before = first.current(&id).await.unwrap().unwrap();
            let now = if expired { 200 } else { 160 };
            assert_eq!(
                second
                    .register(registration(&id, "B"), now, 500)
                    .await
                    .unwrap(),
                before,
                "H1"
            );
            let mut changed = registration(&id, "B");
            changed.manifest.build_digest = "different".into();
            assert!(
                matches!(
                    second.register(changed, now, 500).await,
                    Err(RegistryError::ManifestChanged)
                ),
                "H2"
            );
            for changed in [false, true] {
                let mut replay = registration(&id, "A");
                if changed {
                    replay.manifest.build_digest = "different".into();
                }
                assert!(
                    matches!(
                        second.register(replay, now, 500).await,
                        Err(RegistryError::HistoricalIncarnation { .. })
                    ),
                    "H3 {id} changed={changed}"
                );
                assert_eq!(
                    first.current(&id).await.unwrap(),
                    Some(before.clone()),
                    "H3 no write"
                );
            }
            let replaceable = expired || matches!(state, "quiesced" | "dead");
            let (fresh, replay) = tokio::join!(
                first.register(registration(&id, "C"), now, 50),
                second.register(registration(&id, "A"), now, 50),
            );
            assert!(
                matches!(replay, Err(RegistryError::HistoricalIncarnation { .. })),
                "H8"
            );
            let expected = if replaceable {
                let c = fresh.unwrap();
                assert_eq!(c.snapshot.identity.generation, 3, "H4");
                assert_eq!(c.snapshot.identity.incarnation_id, "C", "H4");
                c
            } else {
                assert!(
                    matches!(fresh, Err(RegistryError::SlotOccupied { .. })),
                    "H5"
                );
                before
            };
            assert_eq!(
                first.current(&id).await.unwrap(),
                Some(expected.clone()),
                "H6 current"
            );
            let listed: Vec<_> = second
                .list()
                .await
                .unwrap()
                .into_iter()
                .filter(|record| record.snapshot.identity.worker_id == id)
                .collect();
            assert_eq!(listed, vec![expected.clone()], "H6 list");
            assert_eq!(
                first
                    .heartbeat(&a.snapshot.identity, beat, now, 500)
                    .await
                    .unwrap(),
                RegistryMutation::StaleIncarnation,
                "H7 heartbeat"
            );
            assert_eq!(
                first.begin_drain(&a.snapshot.identity, now).await.unwrap(),
                RegistryMutation::StaleIncarnation,
                "H7 drain"
            );
            assert_eq!(
                second
                    .expire_incarnation(&a.snapshot.identity, now)
                    .await
                    .unwrap(),
                RegistryMutation::StaleIncarnation,
                "H7 expiry"
            );
            assert_eq!(
                second.current(&id).await.unwrap(),
                Some(expected),
                "H7 no write"
            );
            let peer = format!("{id}-peer");
            assert_eq!(
                second
                    .register(registration(&peer, "A"), now, 50)
                    .await
                    .unwrap()
                    .snapshot
                    .identity
                    .generation,
                1,
                "H10"
            );
        }
    }
}
