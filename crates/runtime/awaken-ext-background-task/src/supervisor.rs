use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use crate::{BackgroundTaskEnd, BackgroundTaskId, TaskFence};

pub const BACKGROUND_TASK_LEASE: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct BackgroundTaskCompletion {
    pub fence: TaskFence,
    pub end: BackgroundTaskEnd,
}

/// Process-local execution projection. Durable authority remains Thread State;
/// losing this value on restart is handled by the aggregate lease/recovery law.
pub struct BackgroundTaskSupervisor {
    worker_id: String,
    active: Mutex<BTreeMap<BackgroundTaskId, CancellationToken>>,
    completed: Mutex<BTreeMap<BackgroundTaskId, BackgroundTaskCompletion>>,
}

/// One process identity shared by the Runtime plugin and product observer.
#[must_use]
pub fn process_supervisor() -> Arc<BackgroundTaskSupervisor> {
    static SUPERVISOR: OnceLock<Arc<BackgroundTaskSupervisor>> = OnceLock::new();
    SUPERVISOR
        .get_or_init(|| {
            Arc::new(BackgroundTaskSupervisor::new(
                awaken_runtime_contract::content_fingerprint(&(
                    "background-worker-v1",
                    std::process::id(),
                    BackgroundTaskSupervisor::now_ms(),
                ))
                .expect("background worker identity serializes"),
            ))
        })
        .clone()
}

impl BackgroundTaskSupervisor {
    #[must_use]
    pub fn new(worker_id: impl Into<String>) -> Self {
        Self {
            worker_id: worker_id.into(),
            active: Mutex::new(BTreeMap::new()),
            completed: Mutex::new(BTreeMap::new()),
        }
    }

    #[must_use]
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    #[must_use]
    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .unwrap_or(u64::MAX)
    }

    #[must_use]
    pub fn lease_ms() -> u64 {
        u64::try_from(BACKGROUND_TASK_LEASE.as_millis()).expect("background lease fits u64")
    }

    /// Claim process-local launch ownership once. Duplicate terminal delivery
    /// observes the existing token and causes no second effect.
    pub fn register(&self, id: &BackgroundTaskId) -> Option<CancellationToken> {
        if self
            .completed
            .lock()
            .expect("background completion mutex poisoned")
            .contains_key(id)
        {
            return None;
        }
        let mut active = self
            .active
            .lock()
            .expect("background active mutex poisoned");
        if active.contains_key(id) {
            return None;
        }
        let token = CancellationToken::new();
        active.insert(id.clone(), token.clone());
        Some(token)
    }

    #[must_use]
    pub fn is_active(&self, id: &BackgroundTaskId) -> bool {
        self.active
            .lock()
            .expect("background active mutex poisoned")
            .contains_key(id)
    }

    pub fn cancel(&self, id: &BackgroundTaskId) {
        if let Some(token) = self
            .active
            .lock()
            .expect("background active mutex poisoned")
            .get(id)
        {
            token.cancel();
        }
    }

    pub fn complete(&self, id: BackgroundTaskId, completion: BackgroundTaskCompletion) {
        self.active
            .lock()
            .expect("background active mutex poisoned")
            .remove(&id);
        self.completed
            .lock()
            .expect("background completion mutex poisoned")
            .entry(id)
            .or_insert(completion);
    }

    /// Read a completed projection without releasing its deduplication guard.
    /// The guard is retired only after an observer sees the corresponding
    /// durable terminal state.
    #[must_use]
    pub fn completion(&self, id: &BackgroundTaskId) -> Option<BackgroundTaskCompletion> {
        self.completed
            .lock()
            .expect("background completion mutex poisoned")
            .get(id)
            .cloned()
    }

    /// Release process-local state after durable Thread State is terminal.
    pub fn retire(&self, id: &BackgroundTaskId) {
        self.active
            .lock()
            .expect("background active mutex poisoned")
            .remove(id);
        self.completed
            .lock()
            .expect("background completion mutex poisoned")
            .remove(id);
    }
}
