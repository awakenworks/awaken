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

#[derive(Default)]
struct SqlCommitGate {
    armed: std::sync::atomic::AtomicBool,
    reached: tokio::sync::Notify,
    released: std::sync::Mutex<bool>,
    resume: std::sync::Condvar,
}

impl SqlCommitGate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.resume.notify_all();
    }
}

// A failed assertion must not strand spawn_blocking during runtime shutdown.
struct ReleaseSqlGate(Arc<SqlCommitGate>);
impl Drop for ReleaseSqlGate {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_commit_continues_after_caller_abort_and_retry_converges() {
    // L1-L8: operation={create,update,delete,retain-conflict} ×
    // retry={before,after late commit}. Retention changes only its candidate row.
    // Unlike the acknowledgement wrapper below, an AFTER SQL trigger pauses
    // INSIDE the actual production spawn_blocking transaction. Abort only the
    // awaiting teardown; SQL continues and must commit exactly once. An independent
    // reader observes the old committed heads while SQL is paused. Re-drive from
    // the same guard AND reopened original evidence preserves exact history/IDs.
    for operation in ["INSERT", "UPDATE", "DELETE", "CONFLICT"] {
        for retry_before_commit in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let database = root.path().join("memory.sqlite");
            let conn = rusqlite::Connection::open(&database).unwrap();
            let gate = Arc::new(SqlCommitGate::default());
            let _release_on_failure = ReleaseSqlGate(gate.clone());
            conn.create_scalar_function(
                "pause_memory_commit",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                {
                    let gate = gate.clone();
                    move |_| {
                        if gate.armed.swap(false, Ordering::SeqCst) {
                            gate.reached.notify_one();
                            let mut released = gate.released.lock().unwrap();
                            while !*released {
                                released = gate.resume.wait(released).unwrap();
                            }
                        }
                        Ok(0_i64)
                    }
                },
            )
            .unwrap();
            let repository = Arc::new(SqliteMemoryRepository::over(conn));
            repository.ensure_schema().unwrap();
            if operation != "INSERT" {
                repository.create("s", "/note.md", "base").await.unwrap();
            }
            let observer = SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap();
            let schema = rusqlite::Connection::open(&database).unwrap();
            let (sql_operation, table) = if operation == "CONFLICT" {
                ("INSERT", "memory_store_copy_conflicts")
            } else {
                (operation, "memory_store_memories")
            };
            schema
                .execute_batch(&format!(
                    "CREATE TRIGGER pause_write AFTER {sql_operation} ON {table}
                 BEGIN SELECT pause_memory_commit(); END;"
                ))
                .unwrap();
            let mounter = Arc::new(MemoryStoreMounter::copy_only(repository.clone()));
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
            if operation == "CONFLICT" {
                let base = &evidence.heads[0];
                repository
                    .update("s", &base.id, "winner", &base.content_sha256)
                    .await
                    .unwrap();
            }
            let initial_heads = repository.snapshot_heads("s").await.unwrap();
            let initial_history = repository.list_versions("s").await.unwrap();
            let files = if operation == "DELETE" {
                std::fs::remove_file(projection.join("note.md")).unwrap();
                vec![]
            } else {
                std::fs::write(projection.join("note.md"), "edit").unwrap();
                vec![("note.md".to_owned(), b"edit".to_vec())]
            };
            gate.armed.store(true, Ordering::SeqCst);
            let pending = tokio::spawn({
                let mount = mount.clone();
                let mounter = mounter.clone();
                let evidence = evidence.clone();
                let files = files.clone();
                async move {
                    if operation == "CONFLICT" {
                        mounter
                            .reconcile_recovered_copy(
                                "s",
                                &evidence,
                                &files,
                                MountAccess::ReadWrite,
                            )
                            .await
                    } else {
                        mount.teardown().await
                    }
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), gate.reached.notified())
                .await
                .expect("SQL transaction reached the pre-commit trigger");
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
            assert_eq!(observer.snapshot_heads("s").await.unwrap(), initial_heads);
            assert_eq!(observer.list_versions("s").await.unwrap(), initial_history);
            assert!(observer.list_copy_conflicts("s").await.unwrap().is_empty());
            let mut retry = if operation == "CONFLICT" {
                mounter.reconcile_recovered_copy("s", &evidence, &files, MountAccess::ReadWrite)
            } else {
                mount.teardown()
            };
            if retry_before_commit {
                // Poll once to place the real retry behind the in-flight connection
                // permit; no timing sleep or assumed task scheduling.
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(retry.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
            }
            gate.release();
            if !retry_before_commit {
                // Same connection's serialized read is a late-commit completion barrier.
                assert_eq!(
                    repository.list_versions("s").await.unwrap().len(),
                    initial_history.len() + usize::from(operation != "CONFLICT")
                );
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), retry)
                .await
                .unwrap()
                .unwrap();
            if operation != "CONFLICT" {
                mount.teardown().await.unwrap();
            }
            let heads = repository.snapshot_heads("s").await.unwrap();
            let history = repository.list_versions("s").await.unwrap();
            assert_eq!(
                history.len(),
                initial_history.len() + usize::from(operation != "CONFLICT")
            );
            let retained = repository.list_copy_conflicts("s").await.unwrap();
            assert_eq!(retained.len(), usize::from(operation == "CONFLICT"));
            assert_eq!(
                heads.first().and_then(|h| h.content.as_deref()),
                if operation == "DELETE" {
                    None
                } else if operation == "CONFLICT" {
                    Some("winner")
                } else {
                    Some("edit")
                }
            );
            schema.execute_batch("DROP TRIGGER pause_write").unwrap();
            drop(mount);
            drop(mounter);
            drop(repository);
            let reopened =
                Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
            MemoryStoreMounter::copy_only(reopened.clone())
                .reconcile_recovered_copy("s", &evidence, &files, MountAccess::ReadWrite)
                .await
                .unwrap();
            assert_eq!(reopened.snapshot_heads("s").await.unwrap(), heads);
            assert_eq!(reopened.list_versions("s").await.unwrap(), history);
            assert_eq!(reopened.list_copy_conflicts("s").await.unwrap(), retained);
        }
    }
}

#[test]
fn recovery_has_no_temp_directory_dependency_even_when_cancelled() {
    // T1-T3 temp location is a FILE (unusable for staging); success, abort before
    // commit and abort after commit must work without touching it. Environment
    // mutation is child-only, so other tests cannot race a global TMPDIR change.
    let root = tempfile::tempdir().unwrap();
    let sentinel = root.path().join("not-a-directory");
    std::fs::write(&sentinel, b"sentinel").unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "recovery_without_temp_child",
            "--ignored",
            "--nocapture",
        ])
        .env("AWAKEN_RECOVERY_TEMP_TEST", "1")
        .env("TMPDIR", &sentinel)
        .env("TMP", &sentinel)
        .env("TEMP", &sentinel)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"sentinel");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test]
#[ignore = "child participant invoked by recovery_has_no_temp_directory_dependency_even_when_cancelled"]
async fn recovery_without_temp_child() {
    assert_eq!(std::env::var("AWAKEN_RECOVERY_TEMP_TEST").unwrap(), "1");
    for after in [false, true] {
        let repository = Arc::new(SqliteMemoryRepository::open_in_memory().unwrap());
        let gate = Arc::new(Gate {
            after,
            error: false,
            ordinal: 0,
            calls: AtomicUsize::new(0),
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        let mounter = Arc::new(MemoryStoreMounter::copy_only(Arc::new(FaultRepository {
            inner: repository.clone(),
            gate: gate.clone(),
        })));
        let evidence =
            MemoryMaterializationEvidence::new("s", "/workspace/memory", vec![]).unwrap();
        let files = vec![("note.md".to_owned(), b"private content".to_vec())];
        let pending = tokio::spawn({
            let mounter = mounter.clone();
            let evidence = evidence.clone();
            let files = files.clone();
            async move {
                mounter
                    .reconcile_recovered_copy("s", &evidence, &files, MountAccess::ReadWrite)
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.reached.notified())
            .await
            .unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        for _ in 0..2 {
            mounter
                .reconcile_recovered_copy("s", &evidence, &files, MountAccess::ReadWrite)
                .await
                .unwrap();
        }
        assert_eq!(repository.list_versions("s").await.unwrap().len(), 1);
        assert_eq!(
            repository
                .get_by_path("s", "/note.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("private content")
        );
    }
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
    async fn preserve_copy_conflict(
        &self,
        s: &str,
        candidate: &awaken_resource_contract::MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        self.mutate(self.inner.preserve_copy_conflict(s, candidate))
            .await
    }
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
    async fn update_materialized(
        &self,
        s: &str,
        path: &str,
        id: &str,
        content: &str,
        base: &str,
    ) -> Result<Memory, MemErr> {
        self.mutate(self.inner.update_materialized(s, path, id, content, base))
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
async fn interrupted_conflict_retention_retries_without_losing_the_proposal() {
    // RC-F: operation={create,update,delete} x fault={before/after durable save}
    // x caller={error,cancel}. Before save => no receipt; after save => one
    // durable proposal despite absent acknowledgement. Both keep source bytes
    // and the winner's complete heads/history. Reopen + two concurrent terminal
    // retries => exactly one candidate, unchanged original CAS base, no pending
    // conflict. This wraps real SQLite, not a synthetic successful repository.
    for operation in ["create", "update", "delete"] {
        for after in [false, true] {
            for error in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let database = root.path().join("memory.sqlite");
                let repository =
                    Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
                if operation != "create" {
                    repository.create("s", "/note.md", "base").await.unwrap();
                }
                let projection = root.path().join("projection");
                let mount = MemoryStoreMounter::copy_only(repository.clone())
                    .mount("s", &projection, MountAccess::ReadWrite)
                    .await
                    .unwrap();
                let evidence = MemoryMaterializationEvidence::new(
                    "s",
                    "/memory",
                    mount.materialization_heads().unwrap(),
                )
                .unwrap();
                if operation == "create" {
                    repository.create("s", "/note.md", "winner").await.unwrap();
                } else {
                    let base = &evidence.heads[0];
                    repository
                        .update("s", &base.id, "winner", &base.content_sha256)
                        .await
                        .unwrap();
                }
                let files = if operation == "delete" {
                    std::fs::remove_file(projection.join("note.md")).unwrap();
                    vec![]
                } else {
                    std::fs::write(projection.join("note.md"), b"loser").unwrap();
                    vec![("note.md".into(), b"loser".to_vec())]
                };
                let heads = repository.snapshot_heads("s").await.unwrap();
                let history = repository.list_versions("s").await.unwrap();
                let gate = Arc::new(Gate {
                    after,
                    error,
                    ordinal: 1,
                    calls: AtomicUsize::new(0),
                    reached: tokio::sync::Notify::new(),
                    release: tokio::sync::Semaphore::new(0),
                });
                let mounter = MemoryStoreMounter::copy_only(Arc::new(FaultRepository {
                    inner: repository.clone(),
                    gate: gate.clone(),
                }));
                let pending = tokio::spawn({
                    let evidence = evidence.clone();
                    let files = files.clone();
                    async move {
                        mounter
                            .reconcile_recovered_copy(
                                "s",
                                &evidence,
                                &files,
                                MountAccess::ReadWrite,
                            )
                            .await
                    }
                });
                tokio::time::timeout(std::time::Duration::from_secs(5), gate.reached.notified())
                    .await
                    .unwrap();
                assert_eq!(
                    repository.list_copy_conflicts("s").await.unwrap().len(),
                    usize::from(after)
                );
                if error {
                    gate.release.add_permits(1);
                    assert!(pending.await.unwrap().is_err());
                } else {
                    pending.abort();
                    assert!(pending.await.unwrap_err().is_cancelled());
                }
                assert!(
                    projection.is_dir(),
                    "failure cannot authorize source deletion"
                );
                if operation != "delete" {
                    assert_eq!(std::fs::read(projection.join("note.md")).unwrap(), b"loser");
                }
                assert_eq!(repository.snapshot_heads("s").await.unwrap(), heads);
                assert_eq!(repository.list_versions("s").await.unwrap(), history);
                drop(mount);
                drop(repository);
                let reopened =
                    Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
                let mounter = MemoryStoreMounter::copy_only(reopened.clone());
                let (a, b) = tokio::join!(
                    mounter.reconcile_recovered_copy(
                        "s",
                        &evidence,
                        &files,
                        MountAccess::ReadWrite
                    ),
                    mounter.reconcile_recovered_copy(
                        "s",
                        &evidence,
                        &files,
                        MountAccess::ReadWrite
                    ),
                );
                a.unwrap();
                b.unwrap();
                let retained = reopened.list_copy_conflicts("s").await.unwrap();
                assert_eq!(retained.len(), 1);
                assert_eq!(
                    retained[0].content.as_deref(),
                    if operation == "delete" {
                        None
                    } else {
                        Some("loser")
                    }
                );
                assert_eq!(
                    retained[0].base.as_ref().map(|b| b.id.as_str()),
                    evidence.heads.first().map(|h| h.id.as_str())
                );
                assert_eq!(reopened.snapshot_heads("s").await.unwrap(), heads);
                assert_eq!(reopened.list_versions("s").await.unwrap(), history);
            }
        }
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
