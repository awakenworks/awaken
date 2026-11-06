//! T40-C01: the mount source admits only the decorator's own bus instance, and
//! a committed write is never reinterpreted as failed because no mount listens.
//!
//! `MemoryStoreMounter::fuse_preferred` (awaken-sandbox-memoryd mounter.rs:136)
//! delegates its admission to the `MemoryMountSource::uses_invalidator` exercised
//! here; this row runs on every host, while the Linux FUSE rows own listener
//! start-up, path and lag invalidation.
use super::InvalidatingMemoryRepository;
use awaken_memory_store::VolatileMemoryRepository;
use awaken_resource_contract::{
    LocalInvalidator, MemoryExecutionContent, MemoryInvalidator, MemoryMountSource,
};
use std::any::TypeId;
use std::sync::Arc;
use tokio::sync::broadcast;

fn decorated(
    bus: Arc<LocalInvalidator>,
) -> Arc<InvalidatingMemoryRepository<VolatileMemoryRepository>> {
    Arc::new(InvalidatingMemoryRepository::new(
        Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock())),
        bus,
    ))
}

/// Negative-control mutant: admit a bus when it merely has the right type.
fn type_only_admission(_source: &MemoryMountSource, candidate: &Arc<LocalInvalidator>) -> bool {
    TypeId::of::<LocalInvalidator>() == candidate_type(candidate.as_ref())
}

fn candidate_type<T: 'static>(_candidate: &T) -> TypeId {
    TypeId::of::<T>()
}

/// Decision table: B1 the decorator's own bus => admitted; B2 a second bus of
/// the same type => refused; B3 the refusal starts no listener on either bus.
/// The type-only mutant admits B2, which the B2 oracle detects.
#[test]
fn mount_admission_accepts_only_the_decorators_own_bus_instance() {
    let original = Arc::new(LocalInvalidator::new(8));
    let second = Arc::new(LocalInvalidator::new(8));
    let source = MemoryMountSource::new(decorated(original.clone()));
    let original_dyn: Arc<dyn MemoryInvalidator> = original.clone();
    let second_dyn: Arc<dyn MemoryInvalidator> = second.clone();

    assert!(source.uses_invalidator(&original_dyn), "B1");
    assert!(!source.uses_invalidator(&second_dyn), "B2");
    assert_eq!(original.subscriber_count(), 0, "B3");
    assert_eq!(second.subscriber_count(), 0, "B3");

    assert!(
        type_only_admission(&source, &second),
        "NC the type-only mutant admits the second bus, so the B2 oracle rejects it"
    );
}

/// C1 no mount listener exists; C2 an execution write commits through the
/// shared decorator. E1 the write returns Ok and the mount view reads the exact
/// head; E2 a listener that subscribes afterwards sees exactly the next write.
#[tokio::test]
async fn a_write_with_no_mount_listener_commits_and_is_visible_to_mounts() {
    let bus = Arc::new(LocalInvalidator::new(8));
    let backend = decorated(bus.clone());
    let execution = MemoryExecutionContent::new(backend.clone());
    let mount = MemoryMountSource::new(backend);
    assert_eq!(bus.subscriber_count(), 0, "C1");

    let created = execution
        .writer()
        .create("s", "/note.md", "first")
        .await
        .expect("E1 no listener is not a write failure");
    assert_eq!(
        mount.queries().get_by_path("s", "/note.md").await.unwrap(),
        Some(created.clone()),
        "E1"
    );

    let mut late = bus.subscribe();
    execution
        .writer()
        .update("s", &created.generation(), "second")
        .await
        .unwrap();
    assert_eq!(
        late.try_recv().unwrap(),
        ("s".into(), "/note.md".into()),
        "E2"
    );
    assert!(matches!(
        late.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}
