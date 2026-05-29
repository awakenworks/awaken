//! Run management operations.

use awaken_server_contract::contract::lifecycle::RunStatus;
use awaken_server_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, RunStore, StorageError,
};

/// Counters returned by [`runs_summary`]. The dashboard Workload card
/// reads all three in one round-trip instead of fanning three parallel
/// `?status=` queries; cuts polling load 3× and narrows the window for
/// inconsistent combinations during status transitions. The three
/// inner counts are still sequential against the store (no store-side
/// batch count API yet), so the snapshot is best-effort, not strictly
/// atomic.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct RunsSummary {
    pub running: u64,
    pub waiting: u64,
    pub created: u64,
}

/// Count non-terminal runs in each status. See [`RunsSummary`].
pub async fn runs_summary(store: &dyn RunStore) -> Result<RunsSummary, StorageError> {
    async fn n(store: &dyn RunStore, status: RunStatus) -> Result<u64, StorageError> {
        let q = RunQuery {
            offset: 0,
            limit: 1,
            thread_id: None,
            status: Some(status),
        };
        store.list_runs(&q).await.map(|page| page.total as u64)
    }
    Ok(RunsSummary {
        running: n(store, RunStatus::Running).await?,
        waiting: n(store, RunStatus::Waiting).await?,
        created: n(store, RunStatus::Created).await?,
    })
}

/// Axum handler thin wrapper. Lives here (not in `routes.rs`) so the
/// router stays under its file-length budget and the count-storage
/// logic is co-located with the rest of run-service.
pub async fn runs_summary_handler(
    axum::extract::State(st): axum::extract::State<crate::app::RunModuleState>,
) -> Result<axum::Json<serde_json::Value>, crate::error::ApiError> {
    let summary = runs_summary(st.store().as_ref())
        .await
        .map_err(|e| crate::error::ApiError::Internal(e.to_string()))?;
    Ok(axum::Json(
        serde_json::to_value(summary).expect("serialize RunsSummary"),
    ))
}

/// Admin-authenticated `/v1/runs/summary` router fragment. Merged through
/// `AdminRunModule`; this endpoint is operational dashboard data, not part
/// of the public run-control surface.
pub fn summary_routes() -> axum::Router<crate::app::RunModuleState> {
    axum::Router::new().route("/v1/runs/summary", axum::routing::get(runs_summary_handler))
}

/// Get a run by ID.
pub async fn get_run(
    store: &dyn RunStore,
    run_id: &str,
) -> Result<Option<RunRecord>, StorageError> {
    store.load_run(run_id).await
}

/// List runs with filtering and pagination.
pub async fn list_runs(store: &dyn RunStore, query: &RunQuery) -> Result<RunPage, StorageError> {
    store.list_runs(query).await
}

/// Get the latest run for a thread.
pub async fn latest_run(
    store: &dyn RunStore,
    thread_id: &str,
) -> Result<Option<RunRecord>, StorageError> {
    store.latest_run(thread_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::contract::lifecycle::RunStatus;

    /// Simple in-memory run store for testing.
    #[derive(Default)]
    struct MockRunStore {
        runs: std::sync::RwLock<std::collections::HashMap<String, RunRecord>>,
    }

    #[async_trait::async_trait]
    impl RunStore for MockRunStore {
        async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
            let mut guard = self.runs.write().unwrap();
            if guard.contains_key(&record.run_id) {
                return Err(StorageError::AlreadyExists(record.run_id.clone()));
            }
            guard.insert(record.run_id.clone(), record.clone());
            Ok(())
        }

        async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
            Ok(self.runs.read().unwrap().get(run_id).cloned())
        }

        async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
            Ok(self
                .runs
                .read()
                .unwrap()
                .values()
                .filter(|r| r.thread_id == thread_id)
                .max_by_key(|r| r.updated_at)
                .cloned())
        }

        async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
            let guard = self.runs.read().unwrap();
            let mut filtered: Vec<RunRecord> = guard
                .values()
                .filter(|r| query.thread_id.as_deref().is_none_or(|t| r.thread_id == t))
                .filter(|r| query.status.is_none_or(|s| r.status == s))
                .cloned()
                .collect();
            filtered.sort_by_key(|r| r.created_at);
            let total = filtered.len();
            let offset = query.offset.min(total);
            let items: Vec<RunRecord> = filtered
                .into_iter()
                .skip(offset)
                .take(query.limit)
                .collect();
            let has_more = offset + items.len() < total;
            Ok(RunPage {
                items,
                total,
                has_more,
            })
        }
    }

    fn make_run(run_id: &str, thread_id: &str, updated_at: u64) -> RunRecord {
        RunRecord {
            run_id: run_id.to_owned(),
            thread_id: thread_id.to_owned(),
            agent_id: "agent-1".to_owned(),
            parent_run_id: None,
            resolution_id: None,
            activation: None,
            request: None,
            input: None,
            output: None,
            status: RunStatus::Running,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: updated_at,
            started_at: None,
            finished_at: None,
            updated_at,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        }
    }

    #[tokio::test]
    async fn get_run_returns_none_for_missing() {
        let store = MockRunStore::default();
        let result = get_run(&store, "missing").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_run_returns_existing() {
        let store = MockRunStore::default();
        let run = make_run("r1", "t1", 100);
        store.create_run(&run).await.unwrap();
        let loaded = get_run(&store, "r1").await.unwrap().unwrap();
        assert_eq!(loaded.thread_id, "t1");
    }

    #[tokio::test]
    async fn latest_run_returns_most_recent() {
        let store = MockRunStore::default();
        store.create_run(&make_run("r1", "t1", 100)).await.unwrap();
        store.create_run(&make_run("r2", "t1", 200)).await.unwrap();
        let result = latest_run(&store, "t1").await.unwrap().unwrap();
        assert_eq!(result.run_id, "r2");
    }

    #[tokio::test]
    async fn list_runs_filters_by_thread() {
        let store = MockRunStore::default();
        store.create_run(&make_run("r1", "t1", 100)).await.unwrap();
        store.create_run(&make_run("r2", "t2", 200)).await.unwrap();
        let page = list_runs(
            &store,
            &RunQuery {
                thread_id: Some("t1".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].run_id, "r1");
    }

    #[tokio::test]
    async fn list_runs_pagination() {
        let store = MockRunStore::default();
        for i in 0..5 {
            store
                .create_run(&make_run(&format!("r{i}"), "t1", i as u64))
                .await
                .unwrap();
        }
        let page = list_runs(
            &store,
            &RunQuery {
                offset: 2,
                limit: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.has_more);
        assert_eq!(page.total, 5);
    }
}
