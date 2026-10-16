//! Controlled faults at the real Repository commit / mounter acknowledgement seam.

mod support;
use awaken_memory_store::{
    MemErr, Memory, MemoryCopyConflictPreservation, MemoryCopyConflictQueries, MemoryCopyWriteback,
    MemoryEntry, MemoryQueries, MemorySnapshotSource, MemoryVersionQueries, MemoryWriter,
    SqliteMemoryRepository,
};
use awaken_provisioning_contract::{
    MemoryMaterializationEvidence, MemoryMount, MemoryMounter, MountAccess,
};
use awaken_sandbox_memoryd::MemoryStoreMounter;
use sqlx::sqlite::SqliteOperation;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::MemoryMountOutcomeTestExt as _;

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

// A failed assertion must not strand the paused SQL write during runtime shutdown.
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
    // Unlike the acknowledgement wrapper below, a SQLite row-change hook pauses
    // INSIDE the actual production write transaction. Abort only the
    // awaiting teardown; SQL continues and must commit exactly once. An independent
    // reader observes the old committed heads while SQL is paused. Re-drive from
    // the same guard AND reopened original evidence preserves exact history/IDs.
    for operation in ["INSERT", "UPDATE", "DELETE", "CONFLICT"] {
        for retry_before_commit in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let database = root.path().join("memory.sqlite");
            let pool = awaken_sqlite_pool::EmbeddedSqlite::file(&database)
                .open()
                .await
                .unwrap();
            let gate = Arc::new(SqlCommitGate::default());
            let _release_on_failure = ReleaseSqlGate(gate.clone());
            let (sql_operation, table) = if operation == "CONFLICT" {
                (SqliteOperation::Insert, "memory_store_copy_conflicts")
            } else {
                (
                    match operation {
                        "INSERT" => SqliteOperation::Insert,
                        "UPDATE" => SqliteOperation::Update,
                        _ => SqliteOperation::Delete,
                    },
                    "memory_store_memories",
                )
            };
            // The row-change hook runs inside the repository's own write
            // statement, before its transaction commits, exactly where an AFTER
            // row trigger would.
            pool.acquire()
                .await
                .unwrap()
                .lock_handle()
                .await
                .unwrap()
                .set_update_hook({
                    let gate = gate.clone();
                    move |change| {
                        if change.operation == sql_operation
                            && change.table == table
                            && gate.armed.swap(false, Ordering::SeqCst)
                        {
                            gate.reached.notify_one();
                            let mut released = gate.released.lock().unwrap();
                            while !*released {
                                released = gate.resume.wait(released).unwrap();
                            }
                        }
                    }
                });
            let repository = Arc::new(SqliteMemoryRepository::over(
                pool,
                awaken_time::system_clock(),
            ));
            repository.ensure_schema().await.unwrap();
            if operation != "INSERT" {
                repository.create("s", "/note.md", "base").await.unwrap();
            }
            let observer = SqliteMemoryRepository::open(
                database.to_str().unwrap(),
                awaken_time::system_clock(),
            )
            .await
            .unwrap();
            let mounter = Arc::new(MemoryStoreMounter::copy_only(repository.clone()));
            let projection = root.path().join("projection");
            let mount: Arc<dyn MemoryMount> = Arc::from(
                mounter
                    .mount("s", &projection, MountAccess::ReadWrite)
                    .await
                    .mounted(),
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
                    .update("s", &base.generation(), "winner")
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
                .unwrap();
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
            let retry_result = tokio::time::timeout(std::time::Duration::from_secs(5), retry)
                .await
                .unwrap();
            assert_eq!(retry_result.is_err(), operation == "UPDATE");
            if operation != "CONFLICT" {
                assert_eq!(mount.teardown().await.is_err(), operation == "UPDATE");
            }
            let heads = repository.snapshot_heads("s").await.unwrap();
            let history = repository.list_versions("s").await.unwrap();
            assert_eq!(
                history.len(),
                initial_history.len() + usize::from(operation != "CONFLICT")
            );
            let retained = repository.list_copy_conflicts("s").await.unwrap();
            assert_eq!(
                retained.len(),
                usize::from(operation == "CONFLICT"),
                "operation={operation} retry_before_commit={retry_before_commit}"
            );
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
            drop(mount);
            drop(mounter);
            drop(repository);
            let reopened = Arc::new(
                SqliteMemoryRepository::open(
                    database.to_str().unwrap(),
                    awaken_time::system_clock(),
                )
                .await
                .unwrap(),
            );
            MemoryStoreMounter::copy_only(reopened.clone())
                .reconcile_recovered_copy("s", &evidence, &files, MountAccess::ReadWrite)
                .await
                .unwrap();
            assert_eq!(reopened.snapshot_heads("s").await.unwrap(), heads);
            assert_eq!(reopened.list_versions("s").await.unwrap(), history);
            let reopened_retained = reopened.list_copy_conflicts("s").await.unwrap();
            assert_eq!(
                reopened_retained.len(),
                usize::from(matches!(operation, "CONFLICT" | "UPDATE")),
                "recovered old generation retains an UPDATE proposal"
            );
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
        let repository = Arc::new(
            SqliteMemoryRepository::open_in_memory(awaken_time::system_clock())
                .await
                .unwrap(),
        );
        let gate = Arc::new(Gate {
            after,
            error: false,
            detached: false,
            completed: tokio::sync::Notify::new(),
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
            awaken_resource_contract::MemoryQueries::get_by_path(
                repository.as_ref(),
                "s",
                "/note.md"
            )
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

impl FaultRepository {
    async fn preserve_copy_conflict(
        &self,
        s: &str,
        candidate: &awaken_resource_contract::MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        let (inner, s, candidate) = (self.inner.clone(), s.to_owned(), candidate.clone());
        self.mutate(async move { inner.preserve_copy_conflict(&s, &candidate).await })
            .await
    }
    async fn snapshot_heads(&self, s: &str) -> Result<Vec<Memory>, MemErr> {
        self.inner.snapshot_heads(s).await
    }
    async fn list(&self, s: &str, p: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        MemoryQueries::list(self.inner.as_ref(), s, p).await
    }
    async fn get_by_path(&self, s: &str, p: &str) -> Result<Option<Memory>, MemErr> {
        MemoryQueries::get_by_path(self.inner.as_ref(), s, p).await
    }
    async fn create_as(
        &self,
        s: &str,
        p: &str,
        c: &str,
        actor: Option<&awaken_resource_contract::MemoryActor>,
    ) -> Result<Memory, MemErr> {
        let (inner, s, p, c, actor) = (
            self.inner.clone(),
            s.to_owned(),
            p.to_owned(),
            c.to_owned(),
            actor.cloned(),
        );
        self.mutate(async move {
            MemoryCopyWriteback::create_as(inner.as_ref(), &s, &p, &c, actor.as_ref()).await
        })
        .await
    }
    async fn update_materialized(
        &self,
        s: &str,
        path: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        let (inner, s, path, base, content) = (
            self.inner.clone(),
            s.to_owned(),
            path.to_owned(),
            base.clone(),
            content.to_owned(),
        );
        self.mutate(async move { inner.update_materialized(&s, &path, &base, &content).await })
            .await
    }
    async fn delete_if_match_as(
        &self,
        s: &str,
        p: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        actor: Option<&awaken_resource_contract::MemoryActor>,
    ) -> Result<bool, MemErr> {
        let (inner, s, p, base, actor) = (
            self.inner.clone(),
            s.to_owned(),
            p.to_owned(),
            base.clone(),
            actor.cloned(),
        );
        self.mutate(async move {
            MemoryCopyWriteback::delete_if_match_as(inner.as_ref(), &s, &p, &base, actor.as_ref())
                .await
        })
        .await
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
                let repository = Arc::new(
                    SqliteMemoryRepository::open(
                        database.to_str().unwrap(),
                        awaken_time::system_clock(),
                    )
                    .await
                    .unwrap(),
                );
                if operation != "create" {
                    repository.create("s", "/note.md", "base").await.unwrap();
                }
                let projection = root.path().join("projection");
                let mount = MemoryStoreMounter::copy_only(repository.clone())
                    .mount("s", &projection, MountAccess::ReadWrite)
                    .await
                    .mounted();
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
                        .update("s", &base.generation(), "winner")
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
                    detached: false,
                    completed: tokio::sync::Notify::new(),
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
                // Inspect the same durable inner backend, not the four-role
                // CopyOnly wrapper; listing is not a mount capability.
                assert_eq!(
                    MemoryCopyConflictQueries::list_copy_conflicts(repository.as_ref(), "s")
                        .await
                        .unwrap()
                        .len(),
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
                let reopened = Arc::new(
                    SqliteMemoryRepository::open(
                        database.to_str().unwrap(),
                        awaken_time::system_clock(),
                    )
                    .await
                    .unwrap(),
                );
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
        // it. Live same-guard retry remains a conflict; terminal recovered-copy
        // retry may finish only after preserving the loser in the Memory
        // aggregate, with unchanged winner/history and idempotent conflict data.
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
                    let repository = Arc::new(
                        SqliteMemoryRepository::open(
                            database.to_str().unwrap(),
                            awaken_time::system_clock(),
                        )
                        .await
                        .unwrap(),
                    );
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
                            .mounted(),
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
                    .unwrap();
                    assert_eq!(
                        repository.list_versions("s").await.unwrap().len(),
                        initial_history + usize::from(prefix) + usize::from(after),
                        "F1/F2/F3 {operation}/{after}/{error}/{prefix}"
                    );
                    let head = awaken_resource_contract::MemoryQueries::get_by_path(
                        repository.as_ref(),
                        "s",
                        "/z.md",
                    )
                    .await
                    .unwrap();
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
                                let head = awaken_resource_contract::MemoryQueries::get_by_path(
                                    repository.as_ref(),
                                    "s",
                                    "/z.md",
                                )
                                .await
                                .unwrap()
                                .unwrap();
                                repository
                                    .update_head("s", &head.generation(), "newer", None)
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
                        let head = awaken_resource_contract::MemoryQueries::get_by_path(
                            repository.as_ref(),
                            "s",
                            "/z.md",
                        )
                        .await
                        .unwrap();
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
                        let stale_update = operation == "update" && (after || detached);
                        assert_eq!(
                            mount.teardown().await.is_err(),
                            competitor || stale_update,
                            "F4/F6 retry operation={operation} after={after} detached={detached} prefix={prefix}"
                        );
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
                    let repository = Arc::new(
                        SqliteMemoryRepository::open(
                            database.to_str().unwrap(),
                            awaken_time::system_clock(),
                        )
                        .await
                        .unwrap(),
                    );
                    let mut files = vec![];
                    if prefix {
                        files.push(("a.md".into(), b"prefix-edit".to_vec()));
                    }
                    if operation != "delete" {
                        files.push(("z.md".into(), b"edit".to_vec()));
                    }
                    let recovered = MemoryStoreMounter::copy_only(repository.clone());
                    for _ in 0..2 {
                        recovered
                            .reconcile_recovered_copy(
                                "s",
                                &evidence,
                                &files,
                                MountAccess::ReadWrite,
                            )
                            .await
                            .expect("F4/F6 terminal retry retains any losing proposal");
                    }
                    let conflicts = repository.list_copy_conflicts("s").await.unwrap();
                    assert_eq!(
                        conflicts.len(),
                        usize::from(prefix) + usize::from(competitor || operation == "update"),
                        "F6 exact conflict retention"
                    );
                    if prefix {
                        assert!(conflicts.iter().any(|conflict| conflict.path == "/a.md"));
                    }
                    if competitor || operation == "update" {
                        let conflict = conflicts
                            .iter()
                            .find(|conflict| conflict.path == "/z.md")
                            .expect("z proposal retained");
                        assert_eq!(
                            conflict.content.as_deref(),
                            (operation != "delete").then_some("edit")
                        );
                        assert_eq!(conflict.base.is_none(), operation == "create");
                    }
                    assert_eq!(repository.snapshot_heads("s").await.unwrap(), expected);
                    assert_eq!(repository.list_versions("s").await.unwrap(), history);
                }
            }
        }
    }
}

#[async_trait::async_trait]
#[cfg(test)]
impl MemoryQueries for FaultRepository {
    async fn list(&self, s: &str, p: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        FaultRepository::list(self, s, p).await
    }
    async fn get_by_path(&self, s: &str, p: &str) -> Result<Option<Memory>, MemErr> {
        FaultRepository::get_by_path(self, s, p).await
    }
}

#[async_trait::async_trait]
#[cfg(test)]
impl MemorySnapshotSource for FaultRepository {
    async fn snapshot_heads(&self, s: &str) -> Result<Vec<Memory>, MemErr> {
        FaultRepository::snapshot_heads(self, s).await
    }
}

#[async_trait::async_trait]
#[cfg(test)]
impl MemoryCopyWriteback for FaultRepository {
    async fn create_as(
        &self,
        s: &str,
        p: &str,
        c: &str,
        actor: Option<&awaken_resource_contract::MemoryActor>,
    ) -> Result<Memory, MemErr> {
        FaultRepository::create_as(self, s, p, c, actor).await
    }
    async fn update_materialized(
        &self,
        s: &str,
        path: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        FaultRepository::update_materialized(self, s, path, base, content).await
    }
    async fn delete_if_match_as(
        &self,
        s: &str,
        p: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        actor: Option<&awaken_resource_contract::MemoryActor>,
    ) -> Result<bool, MemErr> {
        FaultRepository::delete_if_match_as(self, s, p, base, actor).await
    }
}

#[async_trait::async_trait]
#[cfg(test)]
impl MemoryCopyConflictPreservation for FaultRepository {
    async fn preserve_copy_conflict(
        &self,
        s: &str,
        candidate: &awaken_resource_contract::MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        FaultRepository::preserve_copy_conflict(self, s, candidate).await
    }
}
