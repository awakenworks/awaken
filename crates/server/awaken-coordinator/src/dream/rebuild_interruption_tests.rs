//! T23-C03: Dream private snapshot rebuild interrupted mid-clone or during
//! cleanup, then restarted through the production `prepare`/`cleanup`.
//!
//! | Rule | Fault | Effect |
//! |---|---|---|
//! | B1 | writer fails on the 2nd snapshot head (mid-clone) | prepare fails; caller output (UpdateExisting) and source unchanged |
//! | B2 | restart with the real writer | snapshot equals exactly one complete source snapshot (no duplicate/partial heads) |
//! | B3 | B1 observed by a reader | no half snapshot is visible as an Active catalog entry |
//! | B4 | snapshot purge fails during cleanup | cleanup reports the failure / leaves no private content |
use super::*;
use awaken_resource_contract::{MemErr, MemoryActor, MemoryGeneration, MemoryPurgeSummary};
use std::sync::atomic::{AtomicUsize, Ordering};

struct FailNthSnapshotWrite {
    original: Arc<dyn MemoryWriter>,
    store: String,
    fail_at: usize,
    seen: AtomicUsize,
    fault: Fault,
}

/// What the `fail_at`-th snapshot head write does.
enum Fault {
    /// A storage failure in the middle of the clone.
    Interrupt,
    /// T38-C03 controlled quota: the Memory authority reports `AtCapacity`.
    AtCapacity,
    /// Crash-child mode: publish this marker and never return, so the parent
    /// kills the process in the middle of the clone.
    Hang(std::path::PathBuf),
}

#[async_trait::async_trait]
impl MemoryWriter for FailNthSnapshotWrite {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        if store == self.store && self.seen.fetch_add(1, Ordering::SeqCst) == self.fail_at {
            return Err(match &self.fault {
                Fault::Interrupt => MemErr::Storage("snapshot clone interrupted".into()),
                Fault::AtCapacity => MemErr::AtCapacity,
                Fault::Hang(marker) => {
                    std::fs::write(marker, b"mid-clone").unwrap();
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            });
        }
        self.original.create_as(store, path, content, actor).await
    }
    async fn update_head_as(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        self.original
            .update_head_as(store, base, content, target_path, actor)
            .await
    }
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        self.original.delete_by_path_as(store, path, actor).await
    }
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        self.original
            .delete_if_match_as(store, path, base, actor)
            .await
    }
}

struct AlwaysFailPurge {
    original: Arc<dyn MemoryContentPurge>,
    store: String,
}

#[async_trait::async_trait]
impl MemoryContentPurge for AlwaysFailPurge {
    async fn purge_store(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr> {
        if store == self.store {
            return Err(MemErr::Storage("snapshot cleanup blocked".into()));
        }
        self.original.purge_store(store).await
    }
}

struct Fixture {
    resources: awaken_resource_application::ResourcesApplication,
    agent: BuiltInDreamAgent,
    request: DreamRequest,
    snapshot: String,
    result: String,
}

fn heads(memories: &[Memory]) -> Vec<(String, Option<String>)> {
    memories
        .iter()
        .map(|memory| (memory.path.clone(), memory.content.clone()))
        .collect()
}

async fn fixture(job: &str) -> Fixture {
    let resources = awaken_resource_persistence::ephemeral(
        awaken_time::system_clock(),
        Arc::new(awaken_resource_contract::LocalInvalidator::default()),
    )
    .await
    .unwrap();
    fixture_over(resources, job, true).await
}

/// Durable Resources rooted at `root` (registry, Memory and Files in SQLite
/// files): every reopen is a new application over the same files.
async fn open_durable(root: &std::path::Path) -> awaken_resource_application::ResourcesApplication {
    awaken_resource_persistence::open_embedded(
        root,
        awaken_time::system_clock(),
        Arc::new(awaken_resource_contract::LocalInvalidator::default()),
    )
    .await
    .unwrap()
}

/// Build the production Dream agent over `resources`; `seed` creates the
/// caller source and protected output stores first.
async fn fixture_over(
    resources: awaken_resource_application::ResourcesApplication,
    job: &str,
    seed: bool,
) -> Fixture {
    let authorities = resources.authorities();
    let assembled = crate::test_session_fixture::session_read_fixture(
        authorities.resource_registry(),
        awaken_time::system_clock(),
    )
    .await;
    let result = format!("caller-output-{job}");
    if seed {
        seed_caller_stores(&resources, &result).await;
    }
    let agent = BuiltInDreamAgent::new(
        assembled.sessions.clone(),
        &authorities,
        authorities.resource_registry(),
        resources.memory_stores(),
        resources.files(),
    );
    let request = DreamRequest {
        job_id: job.into(),
        workspace_id: "default".into(),
        source_memory_store_id: "dream-source".into(),
        output_behavior: DreamOutputBehavior::UpdateExisting {
            memory_store_id: result.clone(),
        },
        session_ids: Vec::new(),
        model: awaken_session_dream_contract::DreamModelConfig {
            id: "stub".into(),
            speed: None,
        },
        request_guidance: None,
        agent_id: "dream".into(),
    };
    Fixture {
        resources,
        agent,
        request,
        snapshot: format!("mem_snapshot_{job}"),
        result,
    }
}

async fn seed_caller_stores(
    resources: &awaken_resource_application::ResourcesApplication,
    result: &str,
) {
    let authorities = resources.authorities();
    for (store, name) in [("dream-source", "caller source"), (result, "out")] {
        resources
            .memory_stores()
            .create(CreateMemoryStoreCommand {
                workspace_id: "default".into(),
                id: Some(store.into()),
                name: name.into(),
                description: String::new(),
                metadata: BTreeMap::new(),
                initial_state: ResourceState::Active,
                retention_policy: Default::default(),
            })
            .await
            .unwrap();
    }
    for path in ["/a.md", "/b.md", "/c.md"] {
        authorities
            .memory_writer()
            .create("dream-source", path, &format!("source {path}"))
            .await
            .unwrap();
    }
    authorities
        .memory_writer()
        .create(result, "/protected.md", "protected output")
        .await
        .unwrap();
}

async fn interrupted_clone(fixture: &mut Fixture) {
    let authorities = fixture.resources.authorities();
    fixture.agent.writer = Arc::new(FailNthSnapshotWrite {
        original: authorities.memory_writer(),
        store: fixture.snapshot.clone(),
        fail_at: 1,
        seen: AtomicUsize::new(0),
        fault: Fault::Interrupt,
    });
    let error = fixture.agent.prepare(&fixture.request).await.unwrap_err();
    assert!(
        error.message.contains("snapshot clone interrupted"),
        "B1 {}",
        error.message
    );
}

#[tokio::test]
async fn interrupted_snapshot_clone_rebuilds_one_complete_snapshot_and_keeps_protected_output() {
    let mut fixture = fixture("rebuild-b1").await;
    let authorities = fixture.resources.authorities();
    let source = authorities
        .memory_snapshot_source()
        .snapshot_heads("dream-source")
        .await
        .unwrap();
    let protected = authorities
        .memory_snapshot_source()
        .snapshot_heads(&fixture.result)
        .await
        .unwrap();
    interrupted_clone(&mut fixture).await;
    assert_eq!(
        authorities
            .memory_snapshot_source()
            .snapshot_heads(&fixture.result)
            .await
            .unwrap(),
        protected,
        "B1 protected output unchanged"
    );
    assert_eq!(
        heads(
            &authorities
                .memory_snapshot_source()
                .snapshot_heads("dream-source")
                .await
                .unwrap()
        ),
        heads(&source),
        "B1 source unchanged"
    );

    // B2 restart with the real writer. Session admission after the clone is
    // not under test here; only the rebuilt private snapshot is.
    fixture.agent.writer = authorities.memory_writer();
    let _ = fixture.agent.prepare(&fixture.request).await;
    assert_eq!(
        heads(
            &authorities
                .memory_snapshot_source()
                .snapshot_heads(&fixture.snapshot)
                .await
                .unwrap()
        ),
        heads(&source),
        "B2 exactly one complete snapshot"
    );
    assert_eq!(
        authorities
            .memory_versions()
            .list_versions(&fixture.snapshot)
            .await
            .unwrap()
            .len(),
        source.len(),
        "B2 no leftover history from the interrupted clone"
    );
    assert_eq!(
        authorities
            .memory_snapshot_source()
            .snapshot_heads(&fixture.result)
            .await
            .unwrap(),
        protected,
        "B2 protected output unchanged"
    );
}

#[tokio::test]
async fn interrupted_snapshot_clone_never_exposes_a_half_snapshot() {
    let mut fixture = fixture("rebuild-b3").await;
    interrupted_clone(&mut fixture).await;
    let entry = fixture
        .resources
        .memory_stores()
        .get("default", &fixture.snapshot)
        .await
        .unwrap();
    let visible_heads = fixture
        .resources
        .authorities()
        .memory_snapshot_source()
        .snapshot_heads(&fixture.snapshot)
        .await
        .unwrap();
    assert!(
        entry.is_none_or(|entry| entry.state != ResourceState::Active) || visible_heads.len() == 3,
        "B3 an Active snapshot entry must name one complete snapshot, saw {} heads",
        visible_heads.len()
    );
}

#[tokio::test]
async fn failed_snapshot_cleanup_is_reported_or_leaves_no_private_content() {
    let mut fixture = fixture("cleanup-b4").await;
    let authorities = fixture.resources.authorities();
    authorities
        .memory_writer()
        .create(&fixture.snapshot, "/private.md", "private snapshot")
        .await
        .unwrap();
    fixture.agent.content_purge = Arc::new(AlwaysFailPurge {
        original: authorities.memory_content_purge(),
        store: fixture.snapshot.clone(),
    });
    let cleanup = fixture.agent.cleanup(&fixture.request, None).await;
    let remaining = authorities
        .memory_snapshot_source()
        .snapshot_heads(&fixture.snapshot)
        .await
        .unwrap();
    assert!(
        cleanup.is_err() || remaining.is_empty(),
        "B4 cleanup returned Ok while {} private heads remain",
        remaining.len()
    );
}

const T23_CHILD_ROOT: &str = "AWAKEN_T23_DREAM_CHILD_ROOT";
const T23_CHILD_MARKER: &str = "AWAKEN_T23_DREAM_CHILD_MARKER";

/// B1/B2 against DURABLE state: the interrupted clone runs in a child process
/// that is killed in the middle of the clone (after one of three snapshot
/// heads is durable); a NEW Resources application and a NEW Dream agent over
/// the same SQLite files then rebuild. Nothing is shared in memory between the
/// crashed attempt and the rebuild, which is what the negative control asks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crashed_snapshot_clone_rebuilds_from_durable_state_in_a_new_application() {
    const JOB: &str = "rebuild-durable";
    if let Some(root) = std::env::var_os(T23_CHILD_ROOT) {
        let marker = std::env::var_os(T23_CHILD_MARKER).unwrap();
        let mut fixture =
            fixture_over(open_durable(std::path::Path::new(&root)).await, JOB, false).await;
        let authorities = fixture.resources.authorities();
        fixture.agent.writer = Arc::new(FailNthSnapshotWrite {
            original: authorities.memory_writer(),
            store: fixture.snapshot.clone(),
            fail_at: 1,
            seen: AtomicUsize::new(0),
            fault: Fault::Hang(marker.into()),
        });
        let _ = fixture.agent.prepare(&fixture.request).await;
        panic!("the parent must kill the child in the middle of the clone");
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("resources");
    let result = format!("caller-output-{JOB}");
    let (source, protected) = {
        let resources = open_durable(&root).await;
        seed_caller_stores(&resources, &result).await;
        let snapshots = resources.authorities().memory_snapshot_source();
        (
            snapshots.snapshot_heads("dream-source").await.unwrap(),
            snapshots.snapshot_heads(&result).await.unwrap(),
        )
    };
    let marker = dir.path().join("mid-clone.marker");
    let status = awaken_reliability_testkit::CrashProcess::new(
        "dream::rebuild_interruption_tests::crashed_snapshot_clone_rebuilds_from_durable_state_in_a_new_application",
        &marker,
    )
    .env(T23_CHILD_ROOT, &root)
    .env(T23_CHILD_MARKER, &marker)
    .timeout(std::time::Duration::from_secs(120))
    .run()
    .expect("child reaches the middle of the clone and is killed");
    assert!(!status.success());

    let fixture = fixture_over(open_durable(&root).await, JOB, false).await;
    let authorities = fixture.resources.authorities();
    let snapshots = authorities.memory_snapshot_source();
    assert_eq!(
        snapshots
            .snapshot_heads(&fixture.snapshot)
            .await
            .unwrap()
            .len(),
        1,
        "the kill landed mid-clone: one of three snapshot heads is durable"
    );
    assert_eq!(
        snapshots.snapshot_heads(&fixture.result).await.unwrap(),
        protected,
        "B1 protected output survives the crash"
    );
    assert_eq!(
        snapshots.snapshot_heads("dream-source").await.unwrap(),
        source,
        "B1 source survives the crash"
    );

    let _ = fixture.agent.prepare(&fixture.request).await;
    assert_eq!(
        heads(&snapshots.snapshot_heads(&fixture.snapshot).await.unwrap()),
        heads(&source),
        "B2 exactly one complete snapshot after restart"
    );
    assert_eq!(
        authorities
            .memory_versions()
            .list_versions(&fixture.snapshot)
            .await
            .unwrap()
            .len(),
        source.len(),
        "B2 no leftover history from the crashed clone"
    );
    assert_eq!(
        snapshots.snapshot_heads(&fixture.result).await.unwrap(),
        protected,
        "B2 protected output unchanged by the rebuild"
    );
    let snapshot = fixture.snapshot.clone();
    drop(snapshots);
    drop(authorities);
    drop(fixture);

    let reopened = open_durable(&root).await;
    assert_eq!(
        heads(
            &reopened
                .authorities()
                .memory_snapshot_source()
                .snapshot_heads(&snapshot)
                .await
                .unwrap()
        ),
        heads(&source),
        "B2 the rebuilt snapshot is durable"
    );
}

/// T38-C03 prepare path: the Memory authority hits its controlled quota
/// (`AtCapacity`) in the middle of the private snapshot clone.
async fn capacity_exhausted_prepare(root: &std::path::Path, job: &str) -> DreamFailure {
    let resources = open_durable(root).await;
    seed_caller_stores(&resources, &format!("caller-output-{job}")).await;
    let mut fixture = fixture_over(resources, job, false).await;
    let authorities = fixture.resources.authorities();
    fixture.agent.writer = Arc::new(FailNthSnapshotWrite {
        original: authorities.memory_writer(),
        store: fixture.snapshot.clone(),
        fail_at: 1,
        seen: AtomicUsize::new(0),
        fault: Fault::AtCapacity,
    });
    fixture.agent.prepare(&fixture.request).await.unwrap_err()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capacity_exhausted_snapshot_clone_keeps_protected_output_and_retry_rebuilds_once() {
    const JOB: &str = "capacity-prepare";
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("resources");
    let failure = capacity_exhausted_prepare(&root, JOB).await;
    assert!(
        failure.message.contains(&MemErr::AtCapacity.to_string()),
        "X6 the capacity cause surfaces (no panic, no silent success): {failure:?}"
    );

    // Capacity restored: a new application over the same files retries the
    // ORIGINAL operation (same job id, same request).
    let fixture = fixture_over(open_durable(&root).await, JOB, false).await;
    let authorities = fixture.resources.authorities();
    let snapshots = authorities.memory_snapshot_source();
    let source = snapshots.snapshot_heads("dream-source").await.unwrap();
    let protected = snapshots.snapshot_heads(&fixture.result).await.unwrap();
    assert_eq!(
        heads(&protected),
        vec![("/protected.md".into(), Some("protected output".into()))],
        "X6 protected output kept"
    );
    let _ = fixture.agent.prepare(&fixture.request).await;
    assert_eq!(
        heads(&snapshots.snapshot_heads(&fixture.snapshot).await.unwrap()),
        heads(&source),
        "X7 retry of the original operation yields one complete snapshot"
    );
    assert_eq!(
        authorities
            .memory_versions()
            .list_versions(&fixture.snapshot)
            .await
            .unwrap()
            .len(),
        source.len(),
        "X7 exactly once: no history left by the capacity-failed attempt"
    );
    assert_eq!(
        snapshots.snapshot_heads(&fixture.result).await.unwrap(),
        protected,
        "X6 protected output unchanged by the retry"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capacity_exhausted_snapshot_clone_has_a_distinct_failure_kind() {
    let dir = tempfile::tempdir().unwrap();
    let failure = capacity_exhausted_prepare(&dir.path().join("resources"), "capacity-kind").await;
    assert_ne!(
        failure.kind, "internal_error",
        "X8 capacity exhaustion must not share the generic internal failure kind: {failure:?}"
    );
}
