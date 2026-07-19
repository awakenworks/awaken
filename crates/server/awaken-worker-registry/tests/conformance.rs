use std::sync::Arc;

use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerManifest, WorkerRegistration, WorkerState,
};
use awaken_worker_registry::{
    MemoryWorkerDirectory, PostgresWorkerDirectory, SqliteWorkerDirectory,
};

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
                },
                170,
                50,
            )
            .await
            .unwrap(),
        RegistryMutation::StaleIncarnation
    );

    let expired = directory.expire(211).await.unwrap();
    assert_eq!(expired, vec![second.snapshot.identity.clone()]);
    let tombstone = directory.current(worker_id).await.unwrap().unwrap();
    assert_eq!(tombstone.snapshot.state, WorkerState::Dead);
    assert_eq!(directory.list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn memory_registry_conforms() {
    assert_registry(Arc::new(MemoryWorkerDirectory::new()), "dwr-memory").await;
}

#[tokio::test]
async fn sqlite_registry_conforms_and_reopens() {
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
}

#[tokio::test]
async fn postgres_registry_conforms_when_configured() {
    let Ok(url) = std::env::var("AWAKEN_TEST_POSTGRES_URL") else {
        eprintln!("AWAKEN_TEST_POSTGRES_URL not set; skipping Postgres registry conformance");
        return;
    };
    let store = Arc::new(PostgresWorkerDirectory::connect(&url).await.unwrap());
    sqlx::query("DELETE FROM worker_registry_worker WHERE worker_id = 'dwr-postgres'")
        .execute(&store.pool())
        .await
        .unwrap();
    assert_registry(store, "dwr-postgres").await;
}
