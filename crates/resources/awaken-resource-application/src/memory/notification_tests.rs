//! T22-C03: notification delivery never rewrites a committed Memory fact, and a
//! rejected mutation never announces a head change.
//!
//! Every assertion drives the production [`InvalidatingMemoryRepository`] over
//! the real volatile repository; only the invalidation sink is a collaborator.
use super::InvalidatingMemoryRepository;
use awaken_memory_store::VolatileMemoryRepository;
use awaken_resource_contract::{
    LocalInvalidator, MemErr, Memory, MemoryActor, MemoryContentPurge, MemoryCopyWriteback,
    MemoryFilesystem, MemoryGeneration, MemoryInvalidator, MemoryQueries, MemorySnapshotSource,
    MemoryVersionQueries, MemoryWriter,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecordingInvalidator(Mutex<Vec<(String, String)>>);

impl MemoryInvalidator for RecordingInvalidator {
    fn publish(&self, store: &str, path: &str) {
        self.0.lock().unwrap().push((store.into(), path.into()));
    }
}

impl RecordingInvalidator {
    fn events(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

/// Oracle check shared by the production decorator and the negative control:
/// the reported outcome of a write must agree with what the store committed.
async fn outcome_matches_commit(
    reported_ok: bool,
    inner: &VolatileMemoryRepository,
    store: &str,
    path: &str,
) -> bool {
    let committed = MemoryQueries::get_by_path(inner, store, path)
        .await
        .unwrap()
        .is_some();
    reported_ok == committed
}

fn heads(memories: &[Memory]) -> Vec<(String, Option<String>)> {
    memories
        .iter()
        .map(|memory| (memory.path.clone(), memory.content.clone()))
        .collect()
}

/// Negative-control mutant: a decorator that reports an undeliverable
/// notification as a write failure although the inner write already committed.
struct RollbackOnUndelivered {
    inner: Arc<VolatileMemoryRepository>,
    bus: Arc<LocalInvalidator>,
}

#[async_trait::async_trait]
impl MemoryWriter for RollbackOnUndelivered {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        let memory =
            MemoryWriter::create_as(self.inner.as_ref(), store, path, content, actor).await?;
        if self.bus.subscriber_count() == 0 {
            return Err(MemErr::Storage("notification undelivered".into()));
        }
        Ok(memory)
    }
    async fn update_head_as(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        MemoryWriter::update_head_as(
            self.inner.as_ref(),
            store,
            base,
            content,
            target_path,
            actor,
        )
        .await
    }
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        MemoryWriter::delete_by_path_as(self.inner.as_ref(), store, path, actor).await
    }
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        MemoryWriter::delete_if_match_as(self.inner.as_ref(), store, path, base, actor).await
    }
}

/// Cause/effect: C1 the bus has zero live subscribers (send fails); C2 each
/// mutation kind commits in the inner store. E1 every write reports `Ok`; E2
/// the durable heads and history equal the committed script; E3 a mutant that
/// turns the undeliverable notification into a write failure is detected by
/// the same outcome/commit oracle.
#[tokio::test]
async fn committed_writes_survive_an_undeliverable_notification() {
    let inner = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let bus = Arc::new(LocalInvalidator::new(4));
    assert_eq!(bus.subscriber_count(), 0, "C1 nobody listens");
    let fs = InvalidatingMemoryRepository::new(inner.clone(), bus.clone());

    let created = fs.create("s", "/a.md", "one").await.expect("E1 create");
    assert!(
        outcome_matches_commit(true, &inner, "s", "/a.md").await,
        "E1"
    );
    let updated = fs
        .update("s", &created.generation(), "two")
        .await
        .expect("E1 update");
    let renamed = fs
        .rename_as("s", "/a.md", "/b.md", None)
        .await
        .expect("E1 rename");
    assert_eq!(renamed.id, updated.id, "E2 rename keeps the identity");
    fs.create("s", "/c.md", "gone").await.expect("E1 create c");
    fs.delete_by_path("s", "/c.md").await.expect("E1 delete");

    assert_eq!(
        heads(&inner.snapshot_heads("s").await.unwrap()),
        vec![("/b.md".to_string(), Some("two".to_string()))],
        "E2 the committed heads are exactly the script's outcome"
    );
    let versions = inner.list_versions("s").await.unwrap();
    assert!(
        versions.len() >= 5,
        "E2 every committed mutation kept its history: {}",
        versions.len()
    );

    let summary = fs.purge_store("s").await.expect("E1 purge");
    assert!(summary.heads_deleted >= 1, "E2 purge removed the live head");
    assert!(inner.snapshot_heads("s").await.unwrap().is_empty(), "E2");
    assert_eq!(bus.subscriber_count(), 0, "C1 held for the whole script");

    // E3 negative control: the same oracle detects the rollback-on-undelivered mutant.
    let mutant_inner = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let mutant = RollbackOnUndelivered {
        inner: mutant_inner.clone(),
        bus: bus.clone(),
    };
    let reported = mutant.create("s", "/a.md", "one").await;
    assert!(
        matches!(&reported, Err(MemErr::Storage(message)) if message == "notification undelivered")
    );
    assert!(
        !outcome_matches_commit(reported.is_ok(), &mutant_inner, "s", "/a.md").await,
        "E3 the oracle flags a write reported failed although it committed"
    );
}

/// Decision table over rejected mutations (each row must leave the sink and the
/// heads untouched), then one accepted rename as the positive control that the
/// sink is wired. A publish-before-commit mutant is detected by the same check.
#[tokio::test]
async fn every_rejected_mutation_publishes_nothing() {
    let inner = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let seeded = inner.create("s", "/a.md", "one").await.unwrap();
    let sink = Arc::new(RecordingInvalidator::default());
    let fs = InvalidatingMemoryRepository::new(inner.clone(), sink.clone());
    let stale = MemoryGeneration::new(seeded.generation().memory_id(), 99, "deadbeef");
    let before = heads(&inner.snapshot_heads("s").await.unwrap());

    assert!(matches!(
        MemoryFilesystem::create_as(&fs, "s", "/a.md", "dup", None).await,
        Err(MemErr::PathConflict(_))
    ));
    assert!(matches!(
        MemoryFilesystem::rename_as(&fs, "s", "/missing.md", "/z.md", None).await,
        Err(MemErr::NotFound(_))
    ));
    assert!(
        MemoryWriter::update_head_as(&fs, "s", &stale, "two", Some("/z.md"), None)
            .await
            .is_err(),
        "stale path-changing update"
    );
    assert!(
        MemoryFilesystem::update_head_as(&fs, "s", &stale, "two", None, None)
            .await
            .is_err(),
        "stale filesystem update"
    );
    assert!(
        fs.update_materialized("s", "/a.md", &stale, "two")
            .await
            .is_err(),
        "stale materialized update"
    );
    assert!(
        MemoryCopyWriteback::create_as(&fs, "s", "/a.md", "dup", None)
            .await
            .is_err(),
        "duplicate copy create"
    );
    assert!(sink.events().is_empty(), "rejected writes publish nothing");
    assert_eq!(
        heads(&inner.snapshot_heads("s").await.unwrap()),
        before,
        "rejected writes changed no head"
    );

    MemoryFilesystem::rename_as(&fs, "s", "/a.md", "/b.md", None)
        .await
        .unwrap();
    assert_eq!(
        sink.events(),
        vec![("s".to_string(), "/".to_string())],
        "positive control: one accepted rename publishes one store-wide event"
    );

    // Negative control: a mutant that publishes before awaiting the commit is
    // detected by the same "rejected writes publish nothing" check.
    let mutant_sink = RecordingInvalidator::default();
    let mutant_result =
        publish_before_commit_rename(&mutant_sink, inner.as_ref(), "/missing.md", "/z.md").await;
    assert!(matches!(mutant_result, Err(MemErr::NotFound(_))));
    assert_eq!(
        mutant_sink.events(),
        vec![("s".to_string(), "/".to_string())],
        "the check detects a publish that precedes a rejected commit"
    );
}

/// Negative-control mutant of the decorator's rename: the publish runs before
/// the inner `?`, so a rejected rename still announces a head change.
async fn publish_before_commit_rename(
    sink: &RecordingInvalidator,
    inner: &VolatileMemoryRepository,
    from: &str,
    to: &str,
) -> Result<Memory, MemErr> {
    sink.publish("s", "/");
    MemoryFilesystem::rename_as(inner, "s", from, to, None).await
}
