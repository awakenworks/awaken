//! Controlled faults at the real Repository commit / mounter acknowledgement seam.
use awaken_memory_store::{
    MemErr, Memory, MemoryEntry, MemoryPurgeSummary, MemoryRepository, MemoryVersion,
    SqliteMemoryRepository,
};
use awaken_provisioning_contract::{
    MemoryMaterializationEvidence, MemoryMount, MemoryMounter, MountAccess,
};
use awaken_sandbox_memoryd::MemoryStoreMounter;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Gate {
    after: bool,
    detached: bool,
    completed: tokio::sync::Notify,
    error: bool,
    ordinal: usize,
    calls: AtomicUsize,
    reached: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

struct FaultRepository {
    inner: Arc<SqliteMemoryRepository>,
    gate: Arc<Gate>,
}

impl Gate {
    async fn pause(&self) -> Result<(), MemErr> {
        self.reached.notify_one();
        self.release.acquire().await.unwrap().forget();
        if self.error {
            Err(MemErr::Storage("injected acknowledgement failure".into()))
        } else {
            Ok(())
        }
    }
}

impl FaultRepository {
    async fn mutate<T: Send + 'static>(
        &self,
        operation: impl std::future::Future<Output = Result<T, MemErr>> + Send + 'static,
    ) -> Result<T, MemErr> {
        let selected = self.gate.calls.fetch_add(1, Ordering::SeqCst) == self.gate.ordinal;
        if selected && self.gate.detached {
            let gate = self.gate.clone();
            // Once dispatched, the repository operation has its own lifetime,
            // as with SQLite's queued worker or an already-sent remote request.
            return tokio::spawn(async move {
                gate.pause().await?;
                let result = operation.await;
                gate.completed.notify_one();
                result
            })
            .await
            .unwrap();
        }
        if selected && !self.gate.after {
            self.gate.pause().await?;
        }
        let value = operation.await?;
        if selected && self.gate.after {
            self.gate.pause().await?;
        }
        Ok(value)
    }
}

#[async_trait::async_trait]
impl MemoryRepository for FaultRepository {
    async fn snapshot_heads(&self, s: &str) -> Result<Vec<Memory>, MemErr> {
        self.inner.snapshot_heads(s).await
    }
    async fn list(&self, s: &str, p: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        self.inner.list(s, p).await
    }
    async fn get_by_path(&self, s: &str, p: &str) -> Result<Option<Memory>, MemErr> {
        self.inner.get_by_path(s, p).await
    }
    async fn create(&self, s: &str, p: &str, c: &str) -> Result<Memory, MemErr> {
        let (inner, s, p, c) = (self.inner.clone(), s.to_owned(), p.to_owned(), c.to_owned());
        self.mutate(async move { inner.create(&s, &p, &c).await })
            .await
    }
    async fn update_head(
        &self,
        s: &str,
        id: &str,
        c: &str,
        base: &str,
        target: Option<&str>,
    ) -> Result<Memory, MemErr> {
        let (inner, s, id, c, base, target) = (
            self.inner.clone(),
            s.to_owned(),
            id.to_owned(),
            c.to_owned(),
            base.to_owned(),
            target.map(str::to_owned),
        );
        self.mutate(async move {
            inner
                .update_head(&s, &id, &c, &base, target.as_deref())
                .await
        })
        .await
    }
    async fn rename(&self, s: &str, a: &str, b: &str) -> Result<Memory, MemErr> {
        self.inner.rename(s, a, b).await
    }
    async fn delete_by_path(&self, s: &str, p: &str) -> Result<(), MemErr> {
        self.inner.delete_by_path(s, p).await
    }
    async fn delete_if_match(
        &self,
        s: &str,
        p: &str,
        id: &str,
        base: &str,
    ) -> Result<bool, MemErr> {
        let (inner, s, p, id, base) = (
            self.inner.clone(),
            s.to_owned(),
            p.to_owned(),
            id.to_owned(),
            base.to_owned(),
        );
        self.mutate(async move { inner.delete_if_match(&s, &p, &id, &base).await })
            .await
    }
    async fn list_versions(&self, s: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        self.inner.list_versions(s).await
    }
    async fn redact_version(&self, s: &str, id: &str) -> Result<Option<MemoryVersion>, MemErr> {
        self.inner.redact_version(s, id).await
    }
    async fn purge_store(&self, s: &str) -> Result<MemoryPurgeSummary, MemErr> {
        self.inner.purge_store(s).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_writeback_preserves_commit_truth_and_exact_retry() {
    // Cause/effect decision matrix: operation={create,update,delete}; fault=
    // {before commit,after commit before acknowledgement}; termination=
    // {storage error,abort awaiting mounter task}; prefix={none,one committed file}.
    // F1 before commit => selected head/history unchanged. F2 after commit =>
    // head + history already durable although teardown has not returned.
    // F3 cancelling/rejecting the response cannot undo a completed prefix.
    // F4 retry SAME guard then reopen SQLite + ORIGINAL evidence => exactly
    // one version per actual mutation, no accidental rebase/duplicate history.
    // The gate wraps real SQLite operations, not synthetic success responses.
    for operation in ["create", "update", "delete"] {
        // F5 queued mutation outlives caller cancellation; F6 a newer writer
        // wins before that queued mutation runs => stale CAS must not overwrite
        // it, and both same-guard and recovered-copy retries must conflict.
        for (after, detached, competitor) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, true, true),
        ] {
            for error in [false, true] {
                if detached && error {
                    continue;
                }
                for prefix in [false, true] {
                    let root = tempfile::tempdir().unwrap();
                    let database = root.path().join("memory.sqlite");
                    let repository =
                        Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
                    if operation != "create" {
                        repository.create("s", "/z.md", "base").await.unwrap();
                    }
                    if prefix {
                        repository
                            .create("s", "/a.md", "prefix-base")
                            .await
                            .unwrap();
                    }
                    let initial_history = repository.list_versions("s").await.unwrap().len();
                    let gate = Arc::new(Gate {
                        after,
                        detached,
                        completed: tokio::sync::Notify::new(),
                        error,
                        ordinal: usize::from(prefix),
                        calls: AtomicUsize::new(0),
                        reached: tokio::sync::Notify::new(),
                        release: tokio::sync::Semaphore::new(0),
                    });
                    let mounter = MemoryStoreMounter::copy_only(Arc::new(FaultRepository {
                        inner: repository.clone(),
                        gate: gate.clone(),
                    }));
                    let projection = root.path().join("projection");
                    let mount: Arc<dyn MemoryMount> = Arc::from(
                        mounter
                            .mount("s", &projection, MountAccess::ReadWrite)
                            .await
                            .unwrap(),
                    );
                    let evidence = MemoryMaterializationEvidence::new(
                        "s",
                        "/workspace/memory",
                        mount.materialization_heads().unwrap(),
                    )
                    .unwrap();
                    if operation == "delete" {
                        std::fs::remove_file(projection.join("z.md")).unwrap();
                    } else {
                        std::fs::write(projection.join("z.md"), "edit").unwrap();
                    }
                    if prefix {
                        std::fs::write(projection.join("a.md"), "prefix-edit").unwrap();
                    }
                    let pending = tokio::spawn({
                        let mount = mount.clone();
                        async move { mount.teardown().await }
                    });
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        gate.reached.notified(),
                    )
                    .await
                    .expect("exact commit seam reached");
                    assert_eq!(
                        repository.list_versions("s").await.unwrap().len(),
                        initial_history + usize::from(prefix) + usize::from(after),
                        "F1/F2/F3 {operation}/{after}/{error}/{prefix}"
                    );
                    let head = repository.get_by_path("s", "/z.md").await.unwrap();
                    assert_eq!(
                        head.as_ref().and_then(|m| m.content.as_deref()),
                        match (operation, after) {
                            ("create", false) | ("delete", true) => None,
                            (_, false) => Some("base"),
                            _ => Some("edit"),
                        }
                    );
                    if error {
                        gate.release.add_permits(1);
                        assert!(pending.await.unwrap().is_err());
                    } else {
                        pending.abort();
                        assert!(pending.await.unwrap_err().is_cancelled());
                    }
                    if detached {
                        if competitor {
                            if operation == "create" {
                                repository.create("s", "/z.md", "newer").await.unwrap();
                            } else {
                                let head =
                                    repository.get_by_path("s", "/z.md").await.unwrap().unwrap();
                                repository
                                    .update_head("s", &head.id, "newer", &head.content_sha256, None)
                                    .await
                                    .unwrap();
                            }
                        }
                        gate.release.add_permits(1);
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            gate.completed.notified(),
                        )
                        .await
                        .expect("dispatched mutation finishes after caller cancellation");
                        let head = repository.get_by_path("s", "/z.md").await.unwrap();
                        assert_eq!(
                            head.as_ref().and_then(|m| m.content.as_deref()),
                            if competitor {
                                Some("newer")
                            } else if operation == "delete" {
                                None
                            } else {
                                Some("edit")
                            },
                            "F5/F6"
                        );
                    }
                    for _ in 0..2 {
                        assert_eq!(mount.teardown().await.is_err(), competitor, "F4/F6 retry");
                    }
                    let expected = repository.snapshot_heads("s").await.unwrap();
                    let history = repository.list_versions("s").await.unwrap();
                    assert_eq!(
                        history.len(),
                        initial_history + usize::from(prefix) + 1,
                        "F4 one mutation per path"
                    );
                    drop(mount);
                    drop(mounter);
                    drop(repository);
                    let repository =
                        Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
                    let mut files = vec![];
                    if prefix {
                        files.push(("a.md".into(), b"prefix-edit".to_vec()));
                    }
                    if operation != "delete" {
                        files.push(("z.md".into(), b"edit".to_vec()));
                    }
                    let recovered = MemoryStoreMounter::copy_only(repository.clone());
                    assert_eq!(
                        recovered
                            .reconcile_recovered_copy(
                                "s",
                                &evidence,
                                &files,
                                MountAccess::ReadWrite
                            )
                            .await
                            .is_err(),
                        competitor,
                        "F4/F6 recovered retry"
                    );
                    assert_eq!(repository.snapshot_heads("s").await.unwrap(), expected);
                    assert_eq!(repository.list_versions("s").await.unwrap(), history);
                }
            }
        }
    }
}
