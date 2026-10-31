// @formal surface obligation=memory.cas_has_one_winner_per_generation
// @formal reason: Decorates each actual memory capability after its canonical committed operation.
//! One Resources-owned write-through decorator. Every view shares this instance and sink.
use awaken_resource_contract::{
    MemErr, Memory, MemoryActor, MemoryContentPurge, MemoryCopyConflictPreservation,
    MemoryCopyConflictQueries, MemoryCopyWriteback, MemoryEntry, MemoryFilesystem,
    MemoryInvalidator, MemoryQueries, MemorySnapshotSource, MemoryVersion, MemoryVersionQueries,
    MemoryVersionRedaction, MemoryWriter,
};
use std::sync::Arc;

pub struct InvalidatingMemoryRepository<T: ?Sized> {
    inner: Arc<T>,
    invalidator: Arc<dyn MemoryInvalidator>,
}
impl<T: ?Sized> InvalidatingMemoryRepository<T> {
    #[must_use]
    pub fn new(inner: Arc<T>, invalidator: Arc<dyn MemoryInvalidator>) -> Self {
        Self { inner, invalidator }
    }
}
impl<T: Send + Sync + ?Sized> awaken_resource_contract::MemoryInvalidationIdentity
    for InvalidatingMemoryRepository<T>
{
    fn invalidator_identity(&self) -> Arc<dyn MemoryInvalidator> {
        self.invalidator.clone()
    }
}

#[async_trait::async_trait]
impl<T: MemoryQueries + ?Sized> MemoryQueries for InvalidatingMemoryRepository<T> {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        MemoryQueries::list(self.inner.as_ref(), store, prefix).await
    }
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        MemoryQueries::get_by_path(self.inner.as_ref(), store, path).await
    }
}

#[async_trait::async_trait]
impl<T: MemorySnapshotSource + ?Sized> MemorySnapshotSource for InvalidatingMemoryRepository<T> {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        MemorySnapshotSource::snapshot_heads(self.inner.as_ref(), store).await
    }
}

#[async_trait::async_trait]
impl<T: MemoryWriter + ?Sized> MemoryWriter for InvalidatingMemoryRepository<T> {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        // Forward the author: this decorator owns cache invalidation, not
        // attribution, and the inner repository is the one that records it.
        let memory =
            MemoryWriter::create_as(self.inner.as_ref(), store, path, content, actor).await?;
        self.invalidator.publish(store, path);
        Ok(memory)
    }
    async fn update_head_as(
        &self,
        store: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        let memory = MemoryWriter::update_head_as(
            self.inner.as_ref(),
            store,
            base,
            content,
            target_path,
            actor,
        )
        .await?;
        // A path-changing aggregate update can displace another head. Broadcast a
        // store-wide invalidation so peers cannot retain either old path; the
        // resource repository remains unaware of cache/topology concerns.
        if target_path.is_some() {
            self.invalidator.publish(store, "/");
        } else {
            self.invalidator.publish(store, &memory.path);
        }
        Ok(memory)
    }
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        MemoryWriter::delete_by_path_as(self.inner.as_ref(), store, path, actor).await?;
        self.invalidator.publish(store, path);
        Ok(())
    }
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        let deleted =
            MemoryWriter::delete_if_match_as(self.inner.as_ref(), store, path, base, actor).await?;
        if deleted {
            self.invalidator.publish(store, path);
        }
        Ok(deleted)
    }
}

#[async_trait::async_trait]
impl<T: MemoryFilesystem + ?Sized> MemoryFilesystem for InvalidatingMemoryRepository<T> {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        MemoryFilesystem::list(self.inner.as_ref(), store, prefix).await
    }
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        MemoryFilesystem::get_by_path(self.inner.as_ref(), store, path).await
    }
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        // Forward the author: this decorator owns cache invalidation, not
        // attribution, and the inner repository is the one that records it.
        let memory =
            MemoryFilesystem::create_as(self.inner.as_ref(), store, path, content, actor).await?;
        self.invalidator.publish(store, path);
        Ok(memory)
    }
    async fn update_head_as(
        &self,
        store: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        let memory = MemoryFilesystem::update_head_as(
            self.inner.as_ref(),
            store,
            base,
            content,
            target_path,
            actor,
        )
        .await?;
        // A path-changing aggregate update can displace another head. Broadcast a
        // store-wide invalidation so peers cannot retain either old path; the
        // resource repository remains unaware of cache/topology concerns.
        if target_path.is_some() {
            self.invalidator.publish(store, "/");
        } else {
            self.invalidator.publish(store, &memory.path);
        }
        Ok(memory)
    }
    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        let memory = MemoryFilesystem::rename(self.inner.as_ref(), store, from, to).await?;
        self.invalidator.publish(store, "/");
        Ok(memory)
    }
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        MemoryFilesystem::delete_by_path_as(self.inner.as_ref(), store, path, actor).await?;
        self.invalidator.publish(store, path);
        Ok(())
    }
}

#[async_trait::async_trait]
impl<T: MemoryCopyWriteback + ?Sized> MemoryCopyWriteback for InvalidatingMemoryRepository<T> {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        // Forward the author: this decorator owns cache invalidation, not
        // attribution, and the inner repository is the one that records it.
        let memory =
            MemoryCopyWriteback::create_as(self.inner.as_ref(), store, path, content, actor)
                .await?;
        self.invalidator.publish(store, path);
        Ok(memory)
    }
    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        let memory = self
            .inner
            .update_materialized(store, path, base, content)
            .await?;
        self.invalidator.publish(store, path);
        Ok(memory)
    }
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        let deleted =
            MemoryCopyWriteback::delete_if_match_as(self.inner.as_ref(), store, path, base, actor)
                .await?;
        if deleted {
            self.invalidator.publish(store, path);
        }
        Ok(deleted)
    }
}

#[async_trait::async_trait]
impl<T: MemoryCopyConflictPreservation + ?Sized> MemoryCopyConflictPreservation
    for InvalidatingMemoryRepository<T>
{
    async fn preserve_copy_conflict(
        &self,
        store: &str,
        candidate: &awaken_resource_contract::MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        // Retaining a losing proposal changes no mounted head or FUSE cache.
        MemoryCopyConflictPreservation::preserve_copy_conflict(
            self.inner.as_ref(),
            store,
            candidate,
        )
        .await
    }
}

#[async_trait::async_trait]
impl<T: MemoryCopyConflictQueries + ?Sized> MemoryCopyConflictQueries
    for InvalidatingMemoryRepository<T>
{
    async fn list_copy_conflicts(
        &self,
        store: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryCopyConflict>, MemErr> {
        MemoryCopyConflictQueries::list_copy_conflicts(self.inner.as_ref(), store).await
    }
}

#[async_trait::async_trait]
impl<T: MemoryVersionQueries + ?Sized> MemoryVersionQueries for InvalidatingMemoryRepository<T> {
    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        MemoryVersionQueries::list_versions(self.inner.as_ref(), store).await
    }
}

#[async_trait::async_trait]
impl<T: MemoryVersionRedaction + ?Sized> MemoryVersionRedaction
    for InvalidatingMemoryRepository<T>
{
    async fn redact_version_as(
        &self,
        store: &str,
        version_id: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        MemoryVersionRedaction::redact_version_as(self.inner.as_ref(), store, version_id, actor)
            .await
    }
}

#[async_trait::async_trait]
impl<T: MemoryContentPurge + ?Sized> MemoryContentPurge for InvalidatingMemoryRepository<T> {
    async fn purge_store(
        &self,
        store: &str,
    ) -> Result<awaken_resource_contract::MemoryPurgeSummary, MemErr> {
        let summary = MemoryContentPurge::purge_store(self.inner.as_ref(), store).await?;
        self.invalidator.publish(store, "/");
        Ok(summary)
    }
}

#[cfg(test)]
mod bus_identity_tests;

#[cfg(test)]
mod notification_tests;

#[cfg(test)]
mod tests {
    use super::InvalidatingMemoryRepository;
    use awaken_memory_store::VolatileMemoryRepository;
    use awaken_resource_contract::LocalInvalidator;
    use awaken_resource_contract::{
        MemErr, MemoryActor, MemoryContentPurge, MemoryCopyWriteback, MemoryFilesystem,
        MemoryInvalidator, MemoryQueries, MemoryVersionQueries, MemoryWriter,
    };
    use std::sync::Arc;
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn decorator_accepts_a_queries_only_backend_without_granting_write_capabilities() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct QueriesOnly(AtomicUsize);

        #[cfg(test)]
        #[async_trait::async_trait]
        impl MemoryQueries for QueriesOnly {
            async fn list(
                &self,
                store: &str,
                prefix: &str,
            ) -> Result<Vec<awaken_resource_contract::MemoryEntry>, MemErr> {
                assert_eq!((store, prefix), ("selected", "/notes"));
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }

            async fn get_by_path(
                &self,
                store: &str,
                path: &str,
            ) -> Result<Option<awaken_resource_contract::Memory>, MemErr> {
                assert_eq!((store, path), ("selected", "/notes/a.md"));
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            }
        }

        let source = Arc::new(QueriesOnly(AtomicUsize::new(0)));
        let bus = Arc::new(LocalInvalidator::new(4));
        let mut events = bus.subscribe();
        let decorated = InvalidatingMemoryRepository::new(source.clone(), bus);
        // The source implements exactly Queries. This positive compile boundary
        // fails if the decorator accidentally adds Writer/Filesystem bounds.
        let queries: &dyn MemoryQueries = &decorated;
        assert!(queries.list("selected", "/notes").await.unwrap().is_empty());
        assert!(
            queries
                .get_by_path("selected", "/notes/a.md")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(source.0.load(Ordering::SeqCst), 2);
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn decorator_accepts_conflict_queries_only_without_preservation_capability() {
        use awaken_resource_contract::{MemoryCopyConflict, MemoryCopyConflictQueries};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ConflictQueriesOnly {
            proposal: MemoryCopyConflict,
            calls: AtomicUsize,
        }

        #[cfg(test)]
        #[async_trait::async_trait]
        impl MemoryCopyConflictQueries for ConflictQueriesOnly {
            async fn list_copy_conflicts(
                &self,
                store: &str,
            ) -> Result<Vec<MemoryCopyConflict>, MemErr> {
                assert_eq!(store, "selected");
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![self.proposal.clone()])
            }
        }

        let candidate = MemoryCopyConflict {
            path: "/losing.md".into(),
            base: None,
            content: Some("retained proposal".into()),
        };
        let source = Arc::new(ConflictQueriesOnly {
            proposal: candidate.clone(),
            calls: AtomicUsize::new(0),
        });
        let bus = Arc::new(LocalInvalidator::new(4));
        let mut events = bus.subscribe();
        let decorated = InvalidatingMemoryRepository::new(source.clone(), bus);
        // No head reads, preservation, Writer or Filesystem implementation can
        // mask an accidentally widened conflict-query decorator bound.
        let queries: &dyn MemoryCopyConflictQueries = &decorated;
        assert_eq!(
            queries.list_copy_conflicts("selected").await.unwrap(),
            vec![candidate]
        );
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn decorator_accepts_preservation_only_backend_and_retains_failure_without_head_events() {
        use awaken_resource_contract::{MemoryCopyConflict, MemoryCopyConflictPreservation};
        use std::sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        };

        struct PreservationOnly {
            observed: Mutex<Vec<(String, MemoryCopyConflict)>>,
            fail: AtomicBool,
        }

        #[cfg(test)]
        #[async_trait::async_trait]
        impl MemoryCopyConflictPreservation for PreservationOnly {
            async fn preserve_copy_conflict(
                &self,
                store: &str,
                candidate: &MemoryCopyConflict,
            ) -> Result<(), MemErr> {
                if self.fail.load(Ordering::SeqCst) {
                    return Err(MemErr::Storage("preservation unavailable".into()));
                }
                self.observed
                    .lock()
                    .unwrap()
                    .push((store.into(), candidate.clone()));
                Ok(())
            }
        }

        let source = Arc::new(PreservationOnly {
            observed: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        });
        let bus = Arc::new(LocalInvalidator::new(4));
        let mut events = bus.subscribe();
        let decorated = InvalidatingMemoryRepository::new(source.clone(), bus);
        // Neither Queries, Writer nor conflict listing exists on this source.
        let preservation: &dyn MemoryCopyConflictPreservation = &decorated;
        let candidate = MemoryCopyConflict {
            path: "/losing.md".into(),
            base: None,
            content: Some("retain before disposal".into()),
        };
        preservation
            .preserve_copy_conflict("selected", &candidate)
            .await
            .unwrap();
        assert_eq!(
            *source.observed.lock().unwrap(),
            vec![("selected".into(), candidate.clone())]
        );
        source.fail.store(true, Ordering::SeqCst);
        assert!(
            matches!(preservation.preserve_copy_conflict("selected", &candidate).await, Err(MemErr::Storage(message)) if message == "preservation unavailable")
        );
        assert_eq!(source.observed.lock().unwrap().len(), 1);
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "retaining a conflict changes no successful head, even on preservation failure"
        );
    }

    #[tokio::test]
    async fn materialized_update_invalidates_only_after_exact_location_admission() {
        // MP-I: exact path => commit plus one path notification; renamed path
        // with identical bytes => conflict plus zero notifications. No timeout
        // stands in for absence: try_recv must return Empty on the live bus.
        let inner = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
        let base = inner.create("s", "/a", "base").await.unwrap();
        let bus = Arc::new(LocalInvalidator::new(16));
        let mut rx = bus.subscribe();
        let fs = InvalidatingMemoryRepository::new(inner.clone(), bus);
        fs.update_materialized("s", "/a", &base.generation(), "edit")
            .await
            .unwrap();
        assert_eq!(rx.try_recv().unwrap(), ("s".into(), "/a".into()));
        inner.rename("s", "/a", "/b").await.unwrap();
        assert!(matches!(
            fs.update_materialized("s", "/a", &base.generation(), "edit")
                .await,
            Err(MemErr::Conflict { .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    /// Attribution cause/effect graph: C1 a caller states an author on each
    /// mutation that records one; C2 the mutation reaches the inner repository
    /// through this cache decorator. Effect: E1 the author reaches the version
    /// history unchanged. Decision rule: A1=C1+C2 => E1 for create, update and
    /// delete alike.
    ///
    /// This decorator forwarded only the conditional delete, so every other
    /// mutation lowered to the unattributed spelling and the author was lost
    /// before the store ever saw it. The five attributed methods are required
    /// now, which is what makes a missing forward a compile error; this test
    /// pins the behaviour that the requirement exists to protect.
    #[tokio::test]
    async fn every_forwarded_mutation_keeps_the_author_the_caller_stated() {
        let inner = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
        let bus = Arc::new(LocalInvalidator::new(16));
        let fs = InvalidatingMemoryRepository::new(inner.clone(), bus);
        let actor = MemoryActor::UserActor {
            user_id: "user-7".into(),
        };

        let created = MemoryWriter::create_as(&fs, "s", "/a.md", "one", Some(&actor))
            .await
            .unwrap();
        let updated = MemoryWriter::update_head_as(
            &fs,
            "s",
            &created.generation(),
            "two",
            None,
            Some(&actor),
        )
        .await
        .unwrap();
        MemoryWriter::delete_by_path_as(&fs, "s", &updated.path, Some(&actor))
            .await
            .unwrap();

        let versions = inner.list_versions("s").await.unwrap();
        assert_eq!(
            versions.len(),
            3,
            "A1 create, update and delete each recorded"
        );
        for version in &versions {
            assert_eq!(
                version.created_by.as_ref(),
                Some(&actor),
                "A1/E1 {:?} lost the author",
                version.operation
            );
        }
    }

    #[tokio::test]
    async fn a_mutation_publishes_an_invalidation_others_receive() {
        let bus = Arc::new(LocalInvalidator::new(16));
        let mut rx = bus.subscribe();
        let fs = InvalidatingMemoryRepository::new(
            Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock())),
            bus.clone(),
        );

        // create → the inner store holds it AND an invalidation is broadcast.
        let m = fs.create("s", "/a.md", "one").await.unwrap();
        assert_eq!(m.content.as_deref(), Some("one"));
        assert_eq!(
            rx.try_recv().expect("committed synchronous invalidation"),
            ("s".to_string(), "/a.md".to_string())
        );

        // update → invalidation for the memory's path.
        let updated = fs.update("s", &m.generation(), "two").await.unwrap();
        assert_eq!(
            rx.try_recv().expect("committed synchronous invalidation"),
            ("s".to_string(), "/a.md".to_string())
        );

        // A combined content/path update may also displace a destination, so it
        // emits the store-root sentinel consumed as a full cache clear.
        fs.update_head("s", &updated.generation(), "three", Some("/b.md"))
            .await
            .unwrap();
        assert_eq!(
            rx.try_recv().expect("committed synchronous invalidation"),
            ("s".to_string(), "/".to_string())
        );

        // rename → one store-wide notification clears source and displaced destination.
        fs.rename("s", "/b.md", "/c.md").await.unwrap();
        assert_eq!(
            rx.try_recv().expect("committed synchronous invalidation"),
            ("s".to_string(), "/".to_string())
        );

        // delete → invalidation for the removed path.
        fs.delete_by_path("s", "/c.md").await.unwrap();
        assert_eq!(
            rx.try_recv().expect("committed synchronous invalidation"),
            ("s".to_string(), "/c.md".to_string())
        );
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "one successful write emits exactly once"
        );
        fs.purge_store("s").await.unwrap();
        assert_eq!(rx.try_recv().unwrap(), ("s".into(), "/".into()));
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "whole-store purge emits exactly one store-wide invalidation"
        );
    }

    #[tokio::test]
    async fn reads_pass_through_without_invalidating() {
        let bus = Arc::new(LocalInvalidator::new(16));
        let mut rx = bus.subscribe();
        let fs = InvalidatingMemoryRepository::new(
            Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock())),
            bus.clone(),
        );
        fs.create("s", "/a.md", "x").await.unwrap();
        let _ = rx.try_recv().expect("committed synchronous invalidation"); // drain the create

        // a read publishes nothing.
        assert!(
            MemoryQueries::get_by_path(&fs, "s", "/a.md")
                .await
                .unwrap()
                .is_some()
        );
        assert!(MemoryQueries::list(&fs, "s", "/").await.unwrap().len() == 1);
        assert!(rx.try_recv().is_err(), "reads do not invalidate");
    }

    #[tokio::test]
    async fn a_failed_mutation_publishes_no_invalidation() {
        // The invalidation rides AFTER the inner write's `?`, so a mutation that the
        // store rejects broadcasts nothing — other hosts must not drop a cache entry
        // for a change that never landed.
        let bus = Arc::new(LocalInvalidator::new(16));
        let mut rx = bus.subscribe();
        let fs = InvalidatingMemoryRepository::new(
            Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock())),
            bus.clone(),
        );

        let m = fs.create("s", "/a.md", "one").await.unwrap();
        assert_eq!(
            rx.try_recv().expect("committed synchronous invalidation"),
            ("s".to_string(), "/a.md".to_string()),
            "the successful create did publish"
        );

        // A duplicate create (PathConflict) fails → no publish.
        assert!(fs.create("s", "/a.md", "dup").await.is_err());
        // A CAS update on a stale generation (Conflict) fails → no publish.
        let head = m.generation();
        let stale = awaken_resource_contract::MemoryGeneration::new(
            head.memory_id(),
            head.version(),
            "deadbeef",
        );
        assert!(fs.update("s", &stale, "two").await.is_err());
        assert!(
            !MemoryWriter::delete_if_match_as(&fs, "s", "/absent.md", &head, None,)
                .await
                .unwrap(),
            "an absent exact delete is a successful no-op"
        );
        // A delete of an id-less path still succeeds (idempotent) — but a create/update
        // that errored above left the bus silent.
        assert!(
            rx.try_recv().is_err(),
            "a rejected mutation broadcasts nothing"
        );
    }

    #[tokio::test]
    async fn controlled_memory_bindings_retain_one_backend_and_original_bus() {
        let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
        let bus = Arc::new(LocalInvalidator::new(8));
        let decorated = Arc::new(InvalidatingMemoryRepository::new(backend, bus.clone()));
        let execution = awaken_resource_contract::MemoryExecutionContent::new(decorated.clone());
        let mount = awaken_resource_contract::MemoryMountSource::new(decorated.clone());
        let management = awaken_resource_contract::MemoryManagementContent::new(decorated.clone());
        let expected = Arc::as_ptr(&decorated) as *const ();
        assert_eq!(Arc::as_ptr(&execution.queries()) as *const (), expected);
        assert_eq!(Arc::as_ptr(&execution.snapshots()) as *const (), expected);
        assert_eq!(Arc::as_ptr(&execution.writer()) as *const (), expected);
        assert_eq!(
            Arc::as_ptr(&execution.copy_writeback()) as *const (),
            expected
        );
        assert_eq!(Arc::as_ptr(&mount.queries()) as *const (), expected);
        assert_eq!(Arc::as_ptr(&mount.snapshots()) as *const (), expected);
        assert_eq!(Arc::as_ptr(&mount.writeback()) as *const (), expected);
        assert_eq!(
            Arc::as_ptr(&mount.conflict_preservation()) as *const (),
            expected
        );
        assert_eq!(Arc::as_ptr(&mount.filesystem()) as *const (), expected);
        assert_eq!(Arc::as_ptr(&management.snapshots()) as *const (), expected);
        assert_eq!(Arc::as_ptr(&management.writer()) as *const (), expected);
        assert_eq!(Arc::as_ptr(&management.versions()) as *const (), expected);
        assert_eq!(Arc::as_ptr(&management.redaction()) as *const (), expected);
        assert_eq!(
            Arc::as_ptr(&management.conflict_queries()) as *const (),
            expected
        );
        let original: Arc<dyn MemoryInvalidator> = bus.clone();
        let second: Arc<dyn MemoryInvalidator> = Arc::new(LocalInvalidator::new(8));
        assert!(mount.uses_invalidator(&original));
        assert!(!mount.uses_invalidator(&second));
        let mut events = bus.subscribe();
        let created = execution
            .writer()
            .create("s", "/note.md", "same owner")
            .await
            .unwrap();
        assert_eq!(
            mount.queries().get_by_path("s", "/note.md").await.unwrap(),
            Some(created)
        );
        assert_eq!(events.try_recv().unwrap(), ("s".into(), "/note.md".into()));
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }
}
