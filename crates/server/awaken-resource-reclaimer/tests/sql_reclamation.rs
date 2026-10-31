//! T26-C02: a claimed reclamation whose owner process dies inside the physical
//! cleanup keeps a recoverable durable responsibility; a new owner on an
//! independent SQL handle takes over after lease expiry, a protected object is
//! never purged on retry, the dead owner can no longer commit, and success is
//! judged by the physical effect, never by an empty receipt.
//!
//! The crash is a real child process (`CrashProcess`) killed after its claim
//! and reclamation fence are durable and its physical purge has started.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_resource_contract::{
    ResourceKind, ResourcePhysicalReclaimer, ResourcePurgeError, ResourcePurgeEvidence,
    ResourcePurgeGuard, ResourcePurgeIntent, ResourcePurgeReceipt, ResourcePurgeRepository,
    ResourcePurgeStatus, ResourceReference, ResourceReferenceKind, ResourceTarget,
};
use awaken_resource_reclaimer::{ReconcileSummary, ResourceReclaimer};

const CHILD_ROOT: &str = "AWAKEN_RECLAIM_CRASH_ROOT";
const CHILD_NONCE: &str = "AWAKEN_RECLAIM_CRASH_NONCE";
const LEASE_MS: u64 = 1_000;
/// Worker A claims at t=10, so its lease expires at t=1010.
const CLAIMED_AT: u64 = 10;

#[derive(Clone, Copy)]
enum Backend {
    Sqlite,
    Postgres,
}

async fn open(backend: Backend, root: &Path) -> Arc<dyn ResourcePurgeRepository> {
    match backend {
        Backend::Sqlite => Arc::new(
            awaken_resource_store::SqliteResourceStore::open(root.join("resources.db"))
                .await
                .expect("open the file SQLite lifecycle store"),
        ),
        Backend::Postgres => {
            // One isolated schema per scenario root, shared by parent and child;
            // every call opens a new, independent pool.
            let pool = awaken_reliability_testkit::postgres_crash_pool(&schema_identity(root))
                .await
                .expect("the PostgreSQL row runs only on the selected crash backend");
            let store = awaken_resource_store::PostgresResourceStore::with_pool(pool);
            store
                .ensure_schema()
                .await
                .expect("PostgreSQL lifecycle schema");
            Arc::new(store)
        }
    }
}

fn schema_identity(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

fn target(nonce: &str) -> ResourceTarget {
    ResourceTarget::new("ws-reclaim", ResourceKind::File, format!("blob-{nonce}"))
}

fn intent(nonce: &str) -> ResourcePurgeIntent {
    ResourcePurgeIntent::new(
        format!("purge-{nonce}"),
        format!("file:ws-reclaim:blob-{nonce}"),
        target(nonce),
        None,
        CLAIMED_AT,
        CLAIMED_AT,
    )
    .unwrap()
}

fn blob(root: &Path, nonce: &str) -> PathBuf {
    root.join(format!("blob-{nonce}"))
}

fn scratch_root(label: &str) -> (PathBuf, String) {
    let nonce = format!(
        "{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let root = std::env::temp_dir().join(format!("awaken-reclaim-{label}-{nonce}"));
    std::fs::create_dir_all(&root).unwrap();
    (root, nonce)
}

/// Worker A's physical adapter: it publishes the crash marker once cleanup
/// has begun and never returns, so the parent kills it mid-cleanup.
struct DyingPhysical {
    marker: PathBuf,
}

#[async_trait]
impl ResourcePhysicalReclaimer for DyingPhysical {
    async fn purge(
        &self,
        _target: &ResourceTarget,
        _config_version: Option<u64>,
    ) -> Result<ResourcePurgeEvidence, ResourcePurgeError> {
        std::fs::write(&self.marker, b"physical cleanup started").unwrap();
        // awaken-allow(wait): the crash child parks inside physical cleanup until the parent kills it; it never resumes.
        std::future::pending::<()>().await;
        unreachable!("the crash child is killed while cleanup is in progress")
    }
}

/// Worker B's physical adapter: deletes the blob file and reports exactly
/// whether it removed it.
struct FileBlobPhysical {
    blob: PathBuf,
    calls: AtomicUsize,
}

#[async_trait]
impl ResourcePhysicalReclaimer for FileBlobPhysical {
    async fn purge(
        &self,
        _target: &ResourceTarget,
        _config_version: Option<u64>,
    ) -> Result<ResourcePurgeEvidence, ResourcePurgeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ResourcePurgeEvidence::File {
            blob_deleted: std::fs::remove_file(&self.blob).is_ok(),
        })
    }
}

/// Negative-control mutant: reports an empty `blob_deleted: false` receipt
/// without touching the object.
struct EmptyReceiptPhysical(AtomicUsize);

#[async_trait]
impl ResourcePhysicalReclaimer for EmptyReceiptPhysical {
    async fn purge(
        &self,
        _target: &ResourceTarget,
        _config_version: Option<u64>,
    ) -> Result<ResourcePurgeEvidence, ResourcePurgeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ResourcePurgeEvidence::File {
            blob_deleted: false,
        })
    }
}

/// An independently owned protection (a live Session binding).
struct Protected;

#[async_trait]
impl ResourcePurgeGuard for Protected {
    async fn blockers(
        &self,
        _target: &ResourceTarget,
        _config_version: Option<u64>,
        _now_unix_ms: u64,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        Ok(vec![ResourceReference {
            kind: ResourceReferenceKind::SessionBinding,
            reference_id: "session-live".into(),
        }])
    }
}

/// The oracle check: cleanup is signed off only when the object is gone.
fn cleanup_effect_observed(blob: &Path) -> bool {
    !blob.exists()
}

/// Child half: worker A enqueues, claims, acquires the fence and dies inside
/// the physical purge.
async fn crash_child(backend: Backend, root: &Path, nonce: &str) {
    let store = open(backend, root).await;
    let worker_a = ResourceReclaimer::new(
        "worker-a",
        LEASE_MS,
        store,
        Arc::new(DyingPhysical {
            marker: root.join("marker"),
        }),
    )
    .unwrap();
    worker_a.enqueue(intent(nonce)).await.unwrap();
    let _ = worker_a.reconcile(CLAIMED_AT, 1).await;
    unreachable!("worker A never finishes reconciliation");
}

/// Parent half: the decision table after the crash, on a fresh handle/pool.
///
/// | Rule | now | guard | effect |
/// |---|---|---|---|
/// | K1 | before A's lease expiry | none | claim retained, zero purges |
/// | K2 | after expiry | protected | deferred, zero purges, object kept |
/// | K3 | after expiry | none | one purge, object gone, one receipt |
/// | K4 | A's late completion/save | - | rejected, receipt unchanged |
/// | K5 | repeat | none | stable outcome, still one purge |
/// | NC | mutant empty receipt | none | Completed status but object kept: detected |
async fn recover_after_crash(backend: Backend, root: &Path, nonce: &str) {
    let store = open(backend, root).await;
    let intent_id = format!("purge-{nonce}");
    let stale = store
        .get(&intent_id)
        .await
        .unwrap()
        .expect("durable intent");
    assert_eq!(
        stale.status,
        ResourcePurgeStatus::Claimed,
        "the crash left a claim"
    );
    assert_eq!(stale.claim_owner.as_deref(), Some("worker-a"));
    assert!(
        blob(root, nonce).exists(),
        "A died before deleting the object"
    );

    let physical = Arc::new(FileBlobPhysical {
        blob: blob(root, nonce),
        calls: AtomicUsize::new(0),
    });
    let worker_b =
        || ResourceReclaimer::new("worker-b", LEASE_MS, store.clone(), physical.clone()).unwrap();

    assert_eq!(
        worker_b().reconcile(CLAIMED_AT + 500, 10).await.unwrap(),
        ReconcileSummary::default(),
        "K1 a live lease is not taken over"
    );
    assert_eq!(physical.calls.load(Ordering::SeqCst), 0, "K1");
    assert_eq!(
        store.get(&intent_id).await.unwrap().unwrap().status,
        ResourcePurgeStatus::Claimed,
        "K1 responsibility retained"
    );

    let protected = worker_b().with_guard(Arc::new(Protected));
    assert_eq!(
        protected.reconcile(2_000, 10).await.unwrap(),
        ReconcileSummary {
            deferred: 1,
            ..Default::default()
        },
        "K2"
    );
    assert_eq!(physical.calls.load(Ordering::SeqCst), 0, "K2 zero purges");
    assert!(
        blob(root, nonce).exists(),
        "K2 the protected object survives"
    );
    let deferred = store.get(&intent_id).await.unwrap().unwrap();
    assert_eq!(deferred.status, ResourcePurgeStatus::Pending, "K2");
    assert_eq!(deferred.blockers.len(), 1, "K2");

    assert_eq!(
        worker_b().reconcile(3_000, 10).await.unwrap(),
        ReconcileSummary {
            completed: 1,
            ..Default::default()
        },
        "K3"
    );
    assert_eq!(
        physical.calls.load(Ordering::SeqCst),
        1,
        "K3 exactly one purge"
    );
    assert!(cleanup_effect_observed(&blob(root, nonce)), "K3");
    let completed = store.get(&intent_id).await.unwrap().unwrap();
    assert_eq!(completed.status, ResourcePurgeStatus::Completed, "K3");
    assert!(
        matches!(
            completed.receipt,
            Some(ResourcePurgeReceipt {
                evidence: ResourcePurgeEvidence::File { blob_deleted: true },
                ..
            })
        ),
        "K3 one receipt for the observed deletion"
    );

    let mut late = stale.clone();
    assert!(
        late.complete(
            "worker-a",
            stale.claim_generation,
            3_100,
            ResourcePurgeReceipt {
                purged_at_unix_ms: 3_100,
                evidence: ResourcePurgeEvidence::File { blob_deleted: true },
            },
        )
        .is_err(),
        "K4 the expired owner cannot complete"
    );
    assert!(
        matches!(
            store.save(stale.revision, stale.clone()).await,
            Err(ResourcePurgeError::RevisionConflict(_))
        ),
        "K4 the stale snapshot cannot be saved"
    );
    let after_late = store.get(&intent_id).await.unwrap().unwrap();
    assert_eq!(
        after_late.revision, completed.revision,
        "K4 nothing was written"
    );
    assert_eq!(after_late.status, ResourcePurgeStatus::Completed, "K4");

    assert_eq!(
        worker_b().reconcile(4_000, 10).await.unwrap(),
        ReconcileSummary::default(),
        "K5 stable outcome"
    );
    assert_eq!(physical.calls.load(Ordering::SeqCst), 1, "K5");

    // Negative control: same store and check, mutant physical.
    let mutant_nonce = format!("{nonce}-nc");
    std::fs::write(blob(root, &mutant_nonce), b"bytes").unwrap();
    let mutant_physical = Arc::new(EmptyReceiptPhysical(AtomicUsize::new(0)));
    let mutant =
        ResourceReclaimer::new("worker-b", LEASE_MS, store.clone(), mutant_physical.clone())
            .unwrap();
    mutant.enqueue(intent(&mutant_nonce)).await.unwrap();
    assert_eq!(mutant.reconcile(CLAIMED_AT, 10).await.unwrap().completed, 1);
    assert_eq!(mutant_physical.0.load(Ordering::SeqCst), 1);
    assert!(
        !cleanup_effect_observed(&blob(root, &mutant_nonce)),
        "NC an empty receipt is not cleanup: the effect check rejects it"
    );
}

async fn crash_then_recover(backend: Backend, test_name: &str, label: &str) {
    if let (Ok(root), Ok(nonce)) = (std::env::var(CHILD_ROOT), std::env::var(CHILD_NONCE)) {
        crash_child(backend, Path::new(&root), &nonce).await;
        return;
    }
    let (root, nonce) = scratch_root(label);
    if matches!(backend, Backend::Postgres)
        && awaken_reliability_testkit::postgres_crash_pool(&schema_identity(&root))
            .await
            .is_none()
    {
        std::fs::remove_dir_all(&root).ok();
        return;
    }
    std::fs::write(blob(&root, &nonce), b"bytes").unwrap();
    let status = awaken_reliability_testkit::CrashProcess::new(test_name, root.join("marker"))
        .env(CHILD_ROOT, &root)
        .env(CHILD_NONCE, &nonce)
        .timeout(std::time::Duration::from_secs(60))
        .run()
        .expect("worker A reaches physical cleanup and is killed there");
    assert!(!status.success(), "worker A was terminated");
    recover_after_crash(backend, &root, &nonce).await;
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_crashed_cleanup_is_taken_over_once_on_a_second_handle() {
    crash_then_recover(
        Backend::Sqlite,
        "sqlite_crashed_cleanup_is_taken_over_once_on_a_second_handle",
        "sqlite",
    )
    .await;
}

/// Runs when the PostgreSQL crash backend is selected (`AWAKEN_CRASH_BACKEND`).
#[tokio::test(flavor = "multi_thread")]
async fn postgres_crashed_cleanup_is_taken_over_once_on_a_second_pool() {
    crash_then_recover(
        Backend::Postgres,
        "postgres_crashed_cleanup_is_taken_over_once_on_a_second_pool",
        "postgres",
    )
    .await;
}
