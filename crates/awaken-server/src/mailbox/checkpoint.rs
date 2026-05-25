use awaken_contract::contract::commit_coordinator::{CheckpointCommitPlan, CommitError};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::RunRecord;

use super::{Mailbox, MailboxError};

impl Mailbox {
    #[cfg(test)]
    pub(super) async fn commit_run_checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), MailboxError> {
        // ADR-0038 D7: the mailbox's coordinator and run_store are by
        // construction the same logical handle — no `Arc::ptr_eq` check is
        // needed, and there is no parallel coordinator-discovery code path
        // for tests.
        let plan = CheckpointCommitPlan::checkpoint_only(
            thread_id.to_string(),
            messages.to_vec(),
            run.clone(),
        );
        self.coordinator
            .commit_checkpoint(plan)
            .await
            .map_err(|error| MailboxError::Internal(format!("commit checkpoint: {error}")))?;
        Ok(())
    }

    /// Commit `delta` as a version-guarded committed append plus the run record
    /// in one transaction (ADR-0042 A). `expected_version` is the committed
    /// message count the caller observed. Returns `Ok(true)` on commit and
    /// `Ok(false)` on a stale-version conflict — the caller reloads, re-merges
    /// its delta, recomputes the run input range, and retries. Any other commit
    /// failure is surfaced as an error.
    pub(super) async fn commit_run_append(
        &self,
        thread_id: &str,
        delta: &[Message],
        expected_version: Option<u64>,
        run: &RunRecord,
    ) -> Result<bool, MailboxError> {
        let plan = CheckpointCommitPlan::append(
            thread_id.to_string(),
            delta.to_vec(),
            expected_version,
            run.clone(),
        );
        match self.coordinator.commit_checkpoint(plan).await {
            Ok(_) => Ok(true),
            Err(CommitError::MessageVersionConflict { .. }) => Ok(false),
            Err(error) => Err(MailboxError::Internal(format!(
                "commit checkpoint: {error}"
            ))),
        }
    }

    pub(super) async fn refresh_worker_checkpoint_cache(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) {
        let workers = self.workers.read().await;
        if let Some(worker) = workers.get(thread_id) {
            let mut worker = worker.lock();
            if let Some(ref mut ctx) = worker.thread_ctx {
                ctx.apply_checkpoint(messages, run);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use awaken_contract::contract::commit_coordinator::{
        CheckpointCommitOutcome, CommitCoordinator, CommitError, TransactionScopeId,
    };
    use awaken_contract::contract::event_sink::EventSink;
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::storage::{RunStore, ThreadRunStore, ThreadStore};
    use awaken_runtime::RunActivation;
    use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
    use awaken_stores::{InMemoryMailboxStore, InMemoryStore};

    use super::*;
    use crate::mailbox::{Mailbox, MailboxConfig, RunDispatchExecutor};

    struct CountingCoordinator {
        store: Arc<InMemoryStore>,
        commits: AtomicUsize,
    }

    #[async_trait]
    impl CommitCoordinator for CountingCoordinator {
        fn scope(&self) -> TransactionScopeId {
            TransactionScopeId::new("mailbox-test").expect("scope id")
        }

        fn thread_run_store(&self) -> Arc<dyn ThreadRunStore> {
            Arc::clone(&self.store) as Arc<dyn ThreadRunStore>
        }

        async fn commit_checkpoint(
            &self,
            plan: CheckpointCommitPlan,
        ) -> Result<CheckpointCommitOutcome, CommitError> {
            self.commits.fetch_add(1, Ordering::SeqCst);
            #[allow(deprecated)]
            self.store
                .checkpoint(&plan.thread_id, &plan.messages, &plan.run)
                .await?;
            Ok(CheckpointCommitOutcome::default())
        }
    }

    struct CoordinatedExecutor {
        coordinator: Arc<dyn CommitCoordinator>,
    }

    #[async_trait]
    impl RunDispatchExecutor for CoordinatedExecutor {
        async fn run(
            &self,
            _request: RunActivation,
            _sink: Arc<dyn EventSink>,
        ) -> Result<AgentRunResult, AgentLoopError> {
            unreachable!("checkpoint helper test does not execute runs")
        }

        fn cancel(&self, _id: &str) -> bool {
            false
        }

        async fn cancel_and_wait_by_thread(&self, _thread_id: &str) -> bool {
            false
        }

        fn send_decision(
            &self,
            _id: &str,
            _tool_call_id: String,
            _resume: awaken_contract::contract::suspension::ToolCallResume,
        ) -> bool {
            false
        }

        fn commit_coordinator(&self) -> Option<Arc<dyn CommitCoordinator>> {
            Some(Arc::clone(&self.coordinator))
        }
    }

    #[tokio::test]
    async fn mailbox_checkpoint_uses_matching_commit_coordinator() {
        let store = Arc::new(InMemoryStore::new());
        let coordinator = Arc::new(CountingCoordinator {
            store: Arc::clone(&store),
            commits: AtomicUsize::new(0),
        });
        let coordinator_dyn = coordinator.clone() as Arc<dyn CommitCoordinator>;
        let executor = Arc::new(CoordinatedExecutor {
            coordinator: coordinator_dyn,
        });
        let mailbox = Mailbox::new(
            executor,
            Arc::new(InMemoryMailboxStore::new()),
            store.clone() as Arc<dyn ThreadRunStore>,
            "consumer".to_string(),
            MailboxConfig::default(),
        );
        let run = RunRecord {
            run_id: "run-1".to_string(),
            thread_id: "thread-1".to_string(),
            agent_id: "agent-1".to_string(),
            status: RunStatus::Created,
            ..RunRecord::default()
        };

        mailbox
            .commit_run_checkpoint("thread-1", &[Message::user("hi")], &run)
            .await
            .expect("checkpoint through coordinator");

        assert_eq!(coordinator.commits.load(Ordering::SeqCst), 1);
        assert!(store.load_run("run-1").await.unwrap().is_some());
    }

    // ─────────────────────────────────────────────────────────────────
    // MailboxRunStoreCoordinator error-propagation coverage.
    //
    // When the runtime executor exposes no `CommitCoordinator`, `Mailbox`
    // constructs an implicit `MailboxRunStoreCoordinator` wrapping the
    // supplied `run_store`. That implicit coordinator's error must be
    // wrapped as `MailboxError::Internal("commit checkpoint: ...")` —
    // not silently absorbed and not mis-classified.
    // ─────────────────────────────────────────────────────────────────

    /// Executor that exposes NO commit coordinator. Forces `Mailbox::new`
    /// to fall back to its implicit `MailboxRunStoreCoordinator`.
    struct ExecutorWithoutCoordinator;

    #[async_trait]
    impl RunDispatchExecutor for ExecutorWithoutCoordinator {
        async fn run(
            &self,
            _request: RunActivation,
            _sink: Arc<dyn EventSink>,
        ) -> Result<AgentRunResult, AgentLoopError> {
            unreachable!("checkpoint helper test does not execute runs")
        }
        fn cancel(&self, _id: &str) -> bool {
            false
        }
        async fn cancel_and_wait_by_thread(&self, _thread_id: &str) -> bool {
            false
        }
        fn send_decision(
            &self,
            _id: &str,
            _tool_call_id: String,
            _resume: awaken_contract::contract::suspension::ToolCallResume,
        ) -> bool {
            false
        }
        // Intentionally NOT overriding `commit_coordinator()` — returns
        // `None` via the trait default.
    }

    /// `ThreadRunStore` wrapper whose `checkpoint()` always errors. All
    /// other trait methods delegate to a real `InMemoryStore` so the
    /// implementation can keep up with any contract trait method that
    /// `ThreadRunStore` accumulates over time.
    struct FailingThreadRunStore {
        inner: Arc<InMemoryStore>,
    }

    #[async_trait]
    impl awaken_contract::contract::storage::ThreadStore for FailingThreadRunStore {
        async fn save_thread(
            &self,
            thread: &awaken_contract::thread::Thread,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            self.inner.save_thread(thread).await
        }
        async fn load_thread(
            &self,
            id: &str,
        ) -> Result<
            Option<awaken_contract::thread::Thread>,
            awaken_contract::contract::storage::StorageError,
        > {
            self.inner.load_thread(id).await
        }
        async fn delete_thread(
            &self,
            id: &str,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            self.inner.delete_thread(id).await
        }
        async fn list_threads(
            &self,
            offset: usize,
            limit: usize,
        ) -> Result<Vec<String>, awaken_contract::contract::storage::StorageError> {
            self.inner.list_threads(offset, limit).await
        }
        async fn save_messages(
            &self,
            thread_id: &str,
            messages: &[Message],
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            self.inner.save_messages(thread_id, messages).await
        }
        async fn load_messages(
            &self,
            thread_id: &str,
        ) -> Result<Option<Vec<Message>>, awaken_contract::contract::storage::StorageError>
        {
            self.inner.load_messages(thread_id).await
        }
        async fn delete_messages(
            &self,
            thread_id: &str,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            self.inner.delete_messages(thread_id).await
        }
        async fn update_thread_metadata(
            &self,
            thread_id: &str,
            metadata: awaken_contract::thread::ThreadMetadata,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            self.inner.update_thread_metadata(thread_id, metadata).await
        }
    }

    #[async_trait]
    impl RunStore for FailingThreadRunStore {
        async fn create_run(
            &self,
            run: &RunRecord,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            self.inner.create_run(run).await
        }
        async fn load_run(
            &self,
            id: &str,
        ) -> Result<Option<RunRecord>, awaken_contract::contract::storage::StorageError> {
            self.inner.load_run(id).await
        }
        async fn latest_run(
            &self,
            thread_id: &str,
        ) -> Result<Option<RunRecord>, awaken_contract::contract::storage::StorageError> {
            self.inner.latest_run(thread_id).await
        }
        async fn list_runs(
            &self,
            query: &awaken_contract::contract::storage::RunQuery,
        ) -> Result<
            awaken_contract::contract::storage::RunPage,
            awaken_contract::contract::storage::StorageError,
        > {
            self.inner.list_runs(query).await
        }
    }

    #[async_trait]
    impl ThreadRunStore for FailingThreadRunStore {
        async fn checkpoint(
            &self,
            _thread_id: &str,
            _messages: &[Message],
            _run: &RunRecord,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            Err(
                awaken_contract::contract::storage::StorageError::Validation(
                    "simulated FailingThreadRunStore::checkpoint failure".into(),
                ),
            )
        }
    }

    /// When the implicit `MailboxRunStoreCoordinator` propagates an
    /// underlying `ThreadRunStore::checkpoint` failure, the mailbox must
    /// surface it as `MailboxError::Internal("commit checkpoint: …")`.
    #[tokio::test]
    async fn mailbox_run_store_coordinator_wraps_checkpoint_errors() {
        // Executor with no coordinator → Mailbox builds the implicit
        // `MailboxRunStoreCoordinator` over the supplied run_store.
        let failing_store: Arc<dyn ThreadRunStore> = Arc::new(FailingThreadRunStore {
            inner: Arc::new(InMemoryStore::new()),
        });
        let mailbox = Mailbox::new(
            Arc::new(ExecutorWithoutCoordinator),
            Arc::new(InMemoryMailboxStore::new()),
            failing_store,
            "implicit-coord-test".to_string(),
            MailboxConfig::default(),
        );

        let run = RunRecord {
            run_id: "run-fail".to_string(),
            thread_id: "thread-fail".to_string(),
            agent_id: "agent-x".to_string(),
            status: RunStatus::Created,
            ..RunRecord::default()
        };

        let err = mailbox
            .commit_run_checkpoint("thread-fail", &[Message::user("hi")], &run)
            .await
            .expect_err("FailingThreadRunStore must propagate an error");

        match err {
            crate::mailbox::MailboxError::Internal(msg) => {
                assert!(
                    msg.starts_with("commit checkpoint: "),
                    "error must be wrapped with the 'commit checkpoint: ' prefix, got: {msg}"
                );
                assert!(
                    msg.contains("simulated FailingThreadRunStore::checkpoint failure"),
                    "wrapped message must include the underlying store error, got: {msg}"
                );
            }
            other => panic!("expected MailboxError::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mailbox_empty_append_updates_run_without_rewriting_messages() {
        let store = Arc::new(InMemoryStore::new());
        let mailbox = Mailbox::new(
            Arc::new(ExecutorWithoutCoordinator),
            Arc::new(InMemoryMailboxStore::new()),
            store.clone() as Arc<dyn ThreadRunStore>,
            "empty-append-test".to_string(),
            MailboxConfig::default(),
        );
        let initial_run = RunRecord {
            run_id: "run-1".to_string(),
            thread_id: "thread-1".to_string(),
            agent_id: "agent".to_string(),
            status: RunStatus::Created,
            ..RunRecord::default()
        };
        let messages = vec![
            Message::user("first").with_id("msg-1".to_string()),
            Message::user("queued").with_id("msg-2".to_string()),
        ];
        store
            .checkpoint_append("thread-1", &messages, Some(0), &initial_run)
            .await
            .expect("seed committed messages");

        let updated_run = RunRecord {
            status: RunStatus::Done,
            ..initial_run
        };
        let committed = mailbox
            .commit_run_append("thread-1", &[], Some(2), &updated_run)
            .await
            .expect("empty append commits");

        assert!(committed);
        let loaded_messages = store
            .load_messages("thread-1")
            .await
            .expect("load messages")
            .expect("messages exist");
        let ids: Vec<_> = loaded_messages
            .iter()
            .map(|message| message.id.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["msg-1", "msg-2"]);
        let loaded_run = store
            .load_run("run-1")
            .await
            .expect("load run")
            .expect("run exists");
        assert_eq!(loaded_run.status, RunStatus::Done);
    }

    // ─────────────────────────────────────────────────────────────────
    // IntoDispatchExecutor coverage — every constructor input shape must
    // drive a successful checkpoint end-to-end. We exercise both:
    //   * `Arc<R: RunDispatchExecutor + 'static>` (the concrete form)
    //   * `Arc<dyn RunDispatchExecutor>`       (the trait-object form)
    // ─────────────────────────────────────────────────────────────────

    async fn commit_through_mailbox(mailbox: Mailbox) {
        let run = RunRecord {
            run_id: "into-dyn-run".to_string(),
            thread_id: "into-dyn-thread".to_string(),
            agent_id: "agent".to_string(),
            status: RunStatus::Created,
            ..RunRecord::default()
        };
        mailbox
            .commit_run_checkpoint("into-dyn-thread", &[Message::user("hi")], &run)
            .await
            .expect("checkpoint via dyn-erased executor must succeed");
    }

    fn make_coordinated_setup() -> (Arc<CountingCoordinator>, Arc<InMemoryStore>) {
        let store = Arc::new(InMemoryStore::new());
        let coordinator = Arc::new(CountingCoordinator {
            store: Arc::clone(&store),
            commits: AtomicUsize::new(0),
        });
        (coordinator, store)
    }

    /// Pass a concrete `Arc<CoordinatedExecutor>` (R: RunDispatchExecutor)
    /// — the original branch of `IntoDispatchExecutor`.
    #[tokio::test]
    async fn into_dispatch_executor_accepts_concrete_arc() {
        let (coordinator, store) = make_coordinated_setup();
        let coordinator_dyn: Arc<dyn CommitCoordinator> = coordinator.clone();
        let executor = Arc::new(CoordinatedExecutor {
            coordinator: coordinator_dyn,
        });
        let mailbox = Mailbox::new(
            executor, // <-- Arc<CoordinatedExecutor>
            Arc::new(InMemoryMailboxStore::new()),
            store.clone() as Arc<dyn ThreadRunStore>,
            "concrete-arc".to_string(),
            MailboxConfig::default(),
        );
        commit_through_mailbox(mailbox).await;
        assert_eq!(coordinator.commits.load(Ordering::SeqCst), 1);
    }

    /// Pass a pre-erased `Arc<dyn RunDispatchExecutor>` — the second
    /// branch of `IntoDispatchExecutor`. This is the path that
    /// `public_api_compat_extended` smokes against but does not drive a
    /// real commit through.
    #[tokio::test]
    async fn into_dispatch_executor_accepts_pre_erased_arc_and_runs_dispatch() {
        let (coordinator, store) = make_coordinated_setup();
        let coordinator_dyn: Arc<dyn CommitCoordinator> = coordinator.clone();
        let concrete = Arc::new(CoordinatedExecutor {
            coordinator: coordinator_dyn,
        });
        // Type-erase BEFORE handing off to Mailbox::new — this is the
        // path that was previously served by `Mailbox::new_with_executor`.
        let erased: Arc<dyn RunDispatchExecutor> = concrete;
        let mailbox = Mailbox::new(
            erased, // <-- Arc<dyn RunDispatchExecutor>
            Arc::new(InMemoryMailboxStore::new()),
            store.clone() as Arc<dyn ThreadRunStore>,
            "pre-erased-arc".to_string(),
            MailboxConfig::default(),
        );
        commit_through_mailbox(mailbox).await;
        assert_eq!(
            coordinator.commits.load(Ordering::SeqCst),
            1,
            "the erased-arc constructor path must still drive a real commit"
        );
        // FileStore wasn't involved, but the InMemoryStore must reflect
        // the commit.
        assert!(store.load_run("into-dyn-run").await.unwrap().is_some());
    }
}
