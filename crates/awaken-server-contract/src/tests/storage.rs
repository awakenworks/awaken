use super::*;

#[derive(Default)]
struct MockThreadRunStore {
    threads: RwLock<HashMap<String, Thread>>,
    messages: RwLock<HashMap<String, Vec<Message>>>,
    runs: RwLock<HashMap<String, RunRecord>>,
}

#[async_trait]
impl ThreadStore for MockThreadRunStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        Ok(self
            .threads
            .read()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .get(thread_id)
            .cloned())
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.threads
            .write()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .insert(thread.id.clone(), thread.clone());
        Ok(())
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        self.threads
            .write()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .remove(thread_id);
        Ok(())
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        let mut ids: Vec<_> = self
            .threads
            .read()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .keys()
            .cloned()
            .collect();
        ids.sort();
        Ok(ids.into_iter().skip(offset).take(limit).collect())
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        Ok(self
            .messages
            .read()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .get(thread_id)
            .cloned())
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        self.messages
            .write()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .insert(thread_id.to_string(), messages.to_vec());
        Ok(())
    }

    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        self.messages
            .write()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .remove(thread_id);
        Ok(())
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: ThreadMetadata,
    ) -> Result<(), StorageError> {
        let mut guard = self
            .threads
            .write()
            .map_err(|error| StorageError::Io(error.to_string()))?;
        let thread = guard
            .get_mut(id)
            .ok_or_else(|| StorageError::NotFound(id.to_string()))?;
        thread.metadata = metadata;
        Ok(())
    }
}

#[async_trait]
impl RunStore for MockThreadRunStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        self.runs
            .write()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .insert(record.run_id.clone(), record.clone());
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        Ok(self
            .runs
            .read()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .get(run_id)
            .cloned())
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        Ok(self
            .runs
            .read()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .values()
            .filter(|record| record.thread_id == thread_id)
            .max_by_key(|record| record.updated_at)
            .cloned())
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        let mut items: Vec<_> = self
            .runs
            .read()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .values()
            .filter(|record| {
                query
                    .thread_id
                    .as_deref()
                    .is_none_or(|id| record.thread_id == id)
            })
            .filter(|record| query.status.is_none_or(|status| record.status == status))
            .filter(|record| query.matches_id_prefix(&record.thread_id))
            .cloned()
            .collect();
        items.sort_by_key(|record| record.created_at);
        let total = items.len();
        let start = query.offset.min(total);
        let items: Vec<_> = items.into_iter().skip(start).take(query.limit).collect();
        let has_more = start + items.len() < total;
        Ok(RunPage {
            items,
            total,
            has_more,
        })
    }
}

#[async_trait]
impl ThreadRunStore for MockThreadRunStore {
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        if self.load_thread(thread_id).await?.is_none() {
            self.save_thread(&Thread::with_id(thread_id)).await?;
        }
        self.save_messages(thread_id, messages).await?;
        self.create_run(run).await
    }
}

fn make_run(run_id: &str, thread_id: &str, updated_at: u64) -> RunRecord {
    RunRecord {
        run_id: run_id.to_owned(),
        thread_id: thread_id.to_owned(),
        agent_id: "agent-1".to_owned(),
        status: RunStatus::Running,
        created_at: updated_at,
        updated_at,
        ..Default::default()
    }
}

#[tokio::test]
async fn scoped_thread_run_store_isolates_thread_and_run_ids() {
    let inner =
        std::sync::Arc::new(MockThreadRunStore::default()) as std::sync::Arc<dyn ThreadRunStore>;
    let scope_a = ScopedThreadRunStore::new(inner.clone(), ScopeId::new("scope-a").unwrap());
    let scope_b = ScopedThreadRunStore::new(inner.clone(), ScopeId::new("scope-b").unwrap());

    scope_a
        .save_thread(&Thread::with_id("thread-1").with_parent_thread_id("parent"))
        .await
        .unwrap();
    scope_b
        .save_thread(&Thread::with_id("thread-1"))
        .await
        .unwrap();
    scope_a
        .create_run(&make_run("run-1", "thread-1", 100))
        .await
        .unwrap();
    scope_b
        .create_run(&make_run("run-1", "thread-1", 200))
        .await
        .unwrap();

    assert_eq!(
        scope_a
            .load_thread("thread-1")
            .await
            .unwrap()
            .unwrap()
            .parent_thread_id
            .as_deref(),
        Some("parent")
    );
    assert_eq!(
        scope_a.load_run("run-1").await.unwrap().unwrap().updated_at,
        100
    );
    assert_eq!(
        scope_b.load_run("run-1").await.unwrap().unwrap().updated_at,
        200
    );
    assert!(inner.load_thread("thread-1").await.unwrap().is_none());
    assert!(inner.load_run("run-1").await.unwrap().is_none());
}

#[tokio::test]
async fn scoped_list_runs_isolates_and_paginates_across_scopes() {
    let inner =
        std::sync::Arc::new(MockThreadRunStore::default()) as std::sync::Arc<dyn ThreadRunStore>;
    let scope_a = ScopedThreadRunStore::new(inner.clone(), ScopeId::new("scope-a").unwrap());
    let scope_b = ScopedThreadRunStore::new(inner.clone(), ScopeId::new("scope-b").unwrap());

    // 5 runs in scope-a and 3 in scope-b, all sharing one backend.
    for i in 0..5u64 {
        scope_a
            .create_run(&make_run(&format!("a-run-{i}"), "ta", 100 + i))
            .await
            .unwrap();
    }
    for i in 0..3u64 {
        scope_b
            .create_run(&make_run(&format!("b-run-{i}"), "tb", 200 + i))
            .await
            .unwrap();
    }

    let q = |offset, limit, thread_id: Option<&str>| RunQuery {
        offset,
        limit,
        thread_id: thread_id.map(str::to_owned),
        status: None,
        id_prefix: None,
    };

    // Cross-scope list (no thread_id): scope-a sees only its own 5 runs.
    let page = scope_a.list_runs(&q(0, 10, None)).await.unwrap();
    assert_eq!(
        page.total, 5,
        "total counts only in-scope runs, not scope-b's"
    );
    assert_eq!(page.items.len(), 5);
    assert!(page.items.iter().all(|r| r.run_id.starts_with("a-run-")));
    assert!(!page.has_more);

    // Pagination within scope: a window of 2 at offset 2 still totals 5.
    let page = scope_a.list_runs(&q(2, 2, None)).await.unwrap();
    assert_eq!(page.total, 5);
    assert_eq!(page.items.len(), 2);
    assert!(page.has_more);
    assert!(page.items.iter().all(|r| r.run_id.starts_with("a-run-")));

    // scope-b is isolated to its 3 runs.
    let page_b = scope_b.list_runs(&q(0, 10, None)).await.unwrap();
    assert_eq!(page_b.total, 3);
    assert!(page_b.items.iter().all(|r| r.run_id.starts_with("b-run-")));

    // Single-thread pushdown path stays scoped and bounded.
    let page_t = scope_a.list_runs(&q(0, 10, Some("ta"))).await.unwrap();
    assert_eq!(page_t.total, 5);
    assert!(page_t.items.iter().all(|r| r.thread_id == "ta"));
}
