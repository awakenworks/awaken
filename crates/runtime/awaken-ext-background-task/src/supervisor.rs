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

enum SupervisorEntry {
    Active(CancellationToken),
    Completed(BackgroundTaskCompletion),
}

/// Process-local execution projection. Durable authority remains Thread State;
/// losing this value on restart is handled by the aggregate lease/recovery law.
pub struct BackgroundTaskSupervisor {
    worker_id: String,
    /// One mutually-exclusive process projection per durable task. A single
    /// lock makes Active -> Completed atomic, so duplicate terminal delivery
    /// cannot register between two parallel maps.
    entries: Mutex<BTreeMap<BackgroundTaskId, SupervisorEntry>>,
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
            entries: Mutex::new(BTreeMap::new()),
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
        let mut entries = self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned");
        if entries.contains_key(id) {
            return None;
        }
        let token = CancellationToken::new();
        entries.insert(id.clone(), SupervisorEntry::Active(token.clone()));
        Some(token)
    }

    #[must_use]
    pub fn is_active(&self, id: &BackgroundTaskId) -> bool {
        matches!(
            self.entries
                .lock()
                .expect("background supervisor mutex poisoned")
                .get(id),
            Some(SupervisorEntry::Active(_))
        )
    }

    pub fn cancel(&self, id: &BackgroundTaskId) {
        if let Some(SupervisorEntry::Active(token)) = self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned")
            .get(id)
        {
            token.cancel();
        }
    }

    /// Atomically replace one registered launch with its first completion.
    /// An unregistered or already-completed delivery is stale and ignored.
    pub fn complete(&self, id: BackgroundTaskId, completion: BackgroundTaskCompletion) {
        let mut entries = self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned");
        if matches!(entries.get(&id), Some(SupervisorEntry::Active(_))) {
            entries.insert(id, SupervisorEntry::Completed(completion));
        }
    }

    /// Read a completed projection without releasing its deduplication guard.
    /// The guard is retired only after an observer sees the corresponding
    /// durable terminal state.
    #[must_use]
    pub fn completion(&self, id: &BackgroundTaskId) -> Option<BackgroundTaskCompletion> {
        match self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned")
            .get(id)
        {
            Some(SupervisorEntry::Completed(completion)) => Some(completion.clone()),
            Some(SupervisorEntry::Active(_)) | None => None,
        }
    }

    /// Release process-local state after durable Thread State is terminal.
    pub fn retire(&self, id: &BackgroundTaskId) {
        self.entries
            .lock()
            .expect("background supervisor mutex poisoned")
            .remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> BackgroundTaskId {
        BackgroundTaskId::new("task").expect("fixture id")
    }

    fn completion(epoch: u64) -> BackgroundTaskCompletion {
        BackgroundTaskCompletion {
            fence: TaskFence {
                worker_id: "worker".into(),
                epoch,
            },
            end: BackgroundTaskEnd::Cancelled,
        }
    }

    #[test]
    fn active_to_completed_is_one_atomic_deduplication_slot() {
        // Cause/effect table: C1 vacant+register -> E1 Active; C2 Active+complete
        // -> E2 Completed; C3 Completed+register/complete -> E3 both stutter;
        // C4 retire -> E4 vacant and registerable. Constraint: Active and
        // Completed can never coexist in parallel maps for one task id.
        let supervisor = BackgroundTaskSupervisor::new("worker");
        let id = id();
        assert!(supervisor.register(&id).is_some(), "C1/E1");
        assert!(supervisor.is_active(&id), "C1/E1");
        supervisor.complete(id.clone(), completion(1));
        assert!(!supervisor.is_active(&id), "C2/E2");
        assert_eq!(supervisor.completion(&id).unwrap().fence.epoch, 1, "C2/E2");
        assert!(supervisor.register(&id).is_none(), "C3/E3");
        supervisor.complete(id.clone(), completion(2));
        assert_eq!(supervisor.completion(&id).unwrap().fence.epoch, 1, "C3/E3");
        supervisor.retire(&id);
        assert!(supervisor.register(&id).is_some(), "C4/E4");
    }

    #[test]
    fn unregistered_completion_cannot_create_a_parallel_guard() {
        // Cause: completion arrives without a locally registered launch.
        // Effect: it is ignored and leaves the sole slot vacant. Constraint:
        // only the owner that won register may create completion evidence.
        let supervisor = BackgroundTaskSupervisor::new("worker");
        let id = id();
        supervisor.complete(id.clone(), completion(1));
        assert!(supervisor.completion(&id).is_none());
        assert!(supervisor.register(&id).is_some());
    }
}
