//! T26-C01: the production lifecycle guard composed with the production
//! reclaimer and SQLite lifecycle/registry store. A failed or unknown
//! dependency observation must never become permission to purge.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_resource_contract::{
    AcquireResourceReclamationOutcome, ChangeMemoryStoreState, CreateMemoryStoreCommand,
    ResourceAdministration, ResourceKind, ResourcePhysicalReclaimer, ResourcePurgeError,
    ResourcePurgeEvidence, ResourcePurgeGuard, ResourcePurgeIntent, ResourcePurgeRepository,
    ResourcePurgeStatus, ResourceReclamationFence, ResourceReference, ResourceState,
    ResourceTarget,
};
use awaken_resource_reclaimer::{ReconcileSummary, ResourceReclaimer};

use super::ResourceLifecycleGuard;

#[derive(Default)]
struct CountingPhysical(AtomicUsize);

#[async_trait]
impl ResourcePhysicalReclaimer for CountingPhysical {
    async fn purge(
        &self,
        _target: &ResourceTarget,
        _config_version: Option<u64>,
    ) -> Result<ResourcePurgeEvidence, ResourcePurgeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ResourcePurgeEvidence::MemoryStore {
            heads_deleted: 0,
            versions_deleted: 0,
            copy_conflicts_deleted: 0,
        })
    }
}

/// Negative-control mutant: reads a failed dependency observation as "zero references".
struct ZeroOnError(Arc<dyn ResourcePurgeGuard>);

#[async_trait]
impl ResourcePurgeGuard for ZeroOnError {
    async fn blockers(
        &self,
        target: &ResourceTarget,
        config_version: Option<u64>,
        now_unix_ms: u64,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        Ok(self
            .0
            .blockers(target, config_version, now_unix_ms)
            .await
            .unwrap_or_default())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Arc<awaken_resource_store::SqliteResourceStore>,
    registry: Arc<crate::RegistryApplication>,
    guard: Arc<dyn ResourcePurgeGuard>,
}

async fn open_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resources.sqlite");
    let store = Arc::new(
        awaken_resource_store::SqliteResourceStore::open(&path)
            .await
            .unwrap(),
    );
    let registry = Arc::new(crate::RegistryApplication::new(
        store.clone(),
        awaken_time::system_clock(),
    ));
    Fixture {
        _dir: dir,
        path,
        store,
        guard: Arc::new(ResourceLifecycleGuard::new(registry.clone())),
        registry,
    }
}

fn target() -> ResourceTarget {
    ResourceTarget::new("ws-a", ResourceKind::MemoryStore, "mem-1")
}

fn intent(config_version: Option<u64>) -> ResourcePurgeIntent {
    ResourcePurgeIntent::new(
        "purge-mem-1",
        "memory:ws-a:mem-1",
        target(),
        config_version,
        10,
        10,
    )
    .unwrap()
}

/// Make the registry dependency unreadable with a real storage fault.
async fn break_registry(path: &std::path::Path) {
    let sql = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(path))
        .await
        .unwrap();
    sqlx::raw_sql("DROP TABLE resource_catalog_entry")
        .execute(&sql)
        .await
        .unwrap();
    sql.close().await;
}

async fn reconcile_purge(
    fixture: &Fixture,
    selected: ResourcePurgeIntent,
) -> (ReconcileSummary, usize) {
    let physical = Arc::new(CountingPhysical::default());
    let reclaimer =
        ResourceReclaimer::new("worker-a", 100, fixture.store.clone(), physical.clone())
            .unwrap()
            .with_guard(fixture.guard.clone());
    reclaimer.enqueue(selected).await.unwrap();
    let summary = reclaimer.reconcile(10, 10).await.unwrap();
    (summary, physical.0.load(Ordering::SeqCst))
}

/// Decision table: G1 registry read fails => guard returns an incomplete
/// observation (never `Ok([])`); the reclaimer then performs zero physical
/// purges, keeps the intent Pending and retryable, and leaves no fence.
/// Negative control: the identical pipeline with a `ZeroOnError` guard purges
/// once, which the purge-count oracle detects.
#[tokio::test]
async fn a_failed_registry_observation_blocks_purge_end_to_end() {
    let fixture = open_fixture().await;
    break_registry(&fixture.path).await;

    let observed = fixture.guard.blockers(&target(), Some(1), 10).await;
    assert!(
        matches!(
            observed,
            Err(ResourcePurgeError::IncompleteReferenceObservation { .. }
                | ResourcePurgeError::CorruptData { .. })
        ),
        "G1 a failed dependency read is not an empty blocker set: {observed:?}"
    );

    let (summary, purges) = reconcile_purge(&fixture, intent(Some(1))).await;
    assert_eq!(
        summary,
        ReconcileSummary {
            retryable_failures: 1,
            ..Default::default()
        }
    );
    assert_eq!(purges, 0, "G1 zero physical purges");
    let stored = ResourcePurgeRepository::get(fixture.store.as_ref(), "purge-mem-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, ResourcePurgeStatus::Pending, "G1 retryable");
    assert!(stored.receipt.is_none(), "G1 no cleanup receipt");
    assert!(stored.last_error.is_some(), "G1 the failure is retained");
    assert!(
        matches!(
            ResourceReclamationFence::acquire_reclamation(
                fixture.store.as_ref(),
                "probe",
                &target()
            )
            .await
            .unwrap(),
            AcquireResourceReclamationOutcome::Acquired
        ),
        "G1 no reclamation fence was left behind"
    );

    // Negative control on a second, identically broken store.
    let mutant_fixture = open_fixture().await;
    break_registry(&mutant_fixture.path).await;
    let mutant_physical = Arc::new(CountingPhysical::default());
    let mutant = ResourceReclaimer::new(
        "worker-a",
        100,
        mutant_fixture.store.clone(),
        mutant_physical.clone(),
    )
    .unwrap()
    .with_guard(Arc::new(ZeroOnError(mutant_fixture.guard.clone())));
    mutant.enqueue(intent(Some(1))).await.unwrap();
    assert_eq!(mutant.reconcile(10, 10).await.unwrap().completed, 1);
    assert_eq!(
        mutant_physical.0.load(Ordering::SeqCst),
        1,
        "NC the purge-count oracle detects a guard that reads failure as zero references"
    );
}

#[tokio::test]
async fn an_unknown_memory_store_definition_never_authorizes_purge() {
    // C1 no scoped MemoryStore definition, with a valid purge intent and no
    // indexed references. R1 C1 => incomplete observation, zero physical
    // purges, Pending intent + retained error for the existing retry owner.
    // Absence cannot substitute for the required Deleted/version witness.
    let fixture = open_fixture().await;
    let observed = fixture.guard.blockers(&target(), Some(1), 10).await;
    assert!(
        matches!(
            observed,
            Err(ResourcePurgeError::IncompleteReferenceObservation { .. })
        ),
        "R1 an unknown definition is incomplete lifecycle evidence: {observed:?}"
    );
    let (summary, purges) = reconcile_purge(&fixture, intent(Some(1))).await;
    assert_eq!(summary.retryable_failures, 1, "R1");
    assert_eq!(purges, 0, "R1");
    let stored = ResourcePurgeRepository::get(fixture.store.as_ref(), "purge-mem-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, ResourcePurgeStatus::Pending, "R1");
    assert!(stored.receipt.is_none(), "R1 no cleanup receipt");
    assert!(stored.last_error.is_some(), "R1 retry retains the cause");
}

#[tokio::test]
async fn memory_store_purge_requires_a_deleted_definition_at_the_exact_config() {
    // Causes: C1 same Workspace; C2 current lifecycle; C3 admitted config.
    // | Rule | C1      | C2        | C3             | Effect                  |
    // | L1   | exact   | Active    | exact          | defer, zero purges      |
    // | L2   | exact   | Suspended | exact          | defer, zero purges      |
    // | L3   | exact   | Deleted   | absent/stale   | defer, zero purges      |
    // | L4   | exact   | Deleted   | exact          | complete, one purge     |
    // | L5   | foreign | Deleted   | exact          | retry incomplete, zero |
    // All rows use the same real SQLite registry and production reclaimer;
    // only L4 supplies the complete lifecycle witness that authorizes cleanup.
    for (rule, workspace, state, config_version, allow) in [
        ("L1", "ws-a", ResourceState::Active, Some(1), false),
        ("L2", "ws-a", ResourceState::Suspended, Some(1), false),
        ("L3 absent", "ws-a", ResourceState::Deleted, None, false),
        ("L3 stale", "ws-a", ResourceState::Deleted, Some(2), false),
        ("L4", "ws-a", ResourceState::Deleted, Some(1), true),
        ("L5", "ws-b", ResourceState::Deleted, Some(1), false),
    ] {
        let fixture = open_fixture().await;
        fixture
            .registry
            .register_memory_store(crate::memory_store_registration(
                CreateMemoryStoreCommand {
                    workspace_id: workspace.into(),
                    id: Some("mem-1".into()),
                    name: "purge witness".into(),
                    description: String::new(),
                    metadata: Default::default(),
                    initial_state: ResourceState::Active,
                    retention_policy: Default::default(),
                },
                awaken_time::system_clock().now(),
            ))
            .await
            .unwrap();
        if state != ResourceState::Active {
            fixture
                .registry
                .change_memory_store_state(ChangeMemoryStoreState {
                    workspace_id: workspace.into(),
                    id: "mem-1".into(),
                    state,
                })
                .await
                .unwrap();
        }
        let observed = fixture.guard.blockers(&target(), config_version, 10).await;
        if workspace == "ws-b" {
            assert!(
                matches!(
                    observed,
                    Err(ResourcePurgeError::IncompleteReferenceObservation { .. })
                ),
                "{rule}"
            );
        } else {
            assert_eq!(observed.unwrap().is_empty(), allow, "{rule}");
        }
        // The owned intent must carry the same version as the guard probe.
        let selected = intent(config_version);
        let (summary, purges) = reconcile_purge(&fixture, selected).await;
        assert_eq!(summary.completed, usize::from(allow), "{rule}");
        assert_eq!(purges, usize::from(allow), "{rule}");
        assert_eq!(
            summary.retryable_failures,
            usize::from(workspace == "ws-b"),
            "{rule}"
        );
        assert_eq!(
            summary.deferred,
            usize::from(!allow && workspace == "ws-a"),
            "{rule}"
        );
    }
}
