use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use crate::{BackgroundTaskEnd, BackgroundTaskId, BackgroundWait, TaskFence};

pub const BACKGROUND_TASK_LEASE: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct BackgroundTaskCompletion {
    pub fence: TaskFence,
    pub end: BackgroundTaskEnd,
}

#[derive(Debug, Clone)]
pub struct BackgroundTaskWaitCandidate {
    pub fence: TaskFence,
    pub wait: BackgroundWait,
    /// True only for a poll watchdog checkpoint that must extend the same
    /// durable attempt lease before resuming observation.
    pub renew_lease: bool,
}

enum SupervisorEntry {
    Active(CancellationToken),
    Waiting(BackgroundTaskWaitCandidate),
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

    /// Atomically replace one active invocation/poll with its durable wait
    /// candidate. The candidate stays in the same one-slot deduplication guard
    /// until a post-commit observer confirms that Thread State owns it.
    pub fn wait(&self, id: BackgroundTaskId, candidate: BackgroundTaskWaitCandidate) {
        let mut entries = self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned");
        if matches!(entries.get(&id), Some(SupervisorEntry::Active(_))) {
            entries.insert(id, SupervisorEntry::Waiting(candidate));
        }
    }

    #[must_use]
    pub fn wait_candidate(&self, id: &BackgroundTaskId) -> Option<BackgroundTaskWaitCandidate> {
        match self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned")
            .get(id)
        {
            Some(SupervisorEntry::Waiting(candidate)) => Some(candidate.clone()),
            Some(SupervisorEntry::Active(_) | SupervisorEntry::Completed(_)) | None => None,
        }
    }

    /// A committed Waiting/Cancelling continuation atomically consumes the
    /// process candidate and becomes the next active poll/cancel owner. If the
    /// fence differs, the candidate is stale and remains observable for the
    /// StepStart reconciler to retire without launching another effect.
    pub fn resume_wait(
        &self,
        id: &BackgroundTaskId,
        fence: &TaskFence,
    ) -> Option<CancellationToken> {
        let mut entries = self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned");
        let matches = matches!(
            entries.get(id),
            Some(SupervisorEntry::Waiting(candidate)) if candidate.fence == *fence
        );
        if !matches {
            return None;
        }
        let token = CancellationToken::new();
        entries.insert(id.clone(), SupervisorEntry::Active(token.clone()));
        Some(token)
    }

    /// Atomically replace one registered launch with its first completion.
    /// An unregistered or already-completed delivery is stale and ignored.
    pub fn complete(&self, id: BackgroundTaskId, completion: BackgroundTaskCompletion) {
        let mut entries = self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned");
        let may_complete = match entries.get(&id) {
            Some(SupervisorEntry::Active(_)) => true,
            Some(SupervisorEntry::Waiting(candidate)) => candidate.fence == completion.fence,
            Some(SupervisorEntry::Completed(_)) | None => false,
        };
        if may_complete {
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
            Some(SupervisorEntry::Active(_) | SupervisorEntry::Waiting(_)) | None => None,
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

    fn wait_candidate(epoch: u64) -> BackgroundTaskWaitCandidate {
        BackgroundTaskWaitCandidate {
            fence: TaskFence {
                worker_id: "worker".into(),
                epoch,
            },
            wait: BackgroundWait::Remote(awaken_runtime_contract::tool::ToolTaskHandle {
                owner: "mcp".into(),
                binding: "mcp-generation".into(),
                task_id: "remote-task".into(),
                poll_interval_ms: Some(50),
            }),
            renew_lease: false,
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

    #[test]
    fn active_wait_commit_ack_and_poll_reuse_one_atomic_slot() {
        // Cause/effect decision table: R1 vacant wait delivery -> inert; R2
        // Active+wait -> one Waiting candidate and no completion; R3 wrong-fence
        // commit ack -> inert; R4 matching committed fence -> the same slot
        // becomes Active and returns one cancellation token; R5 duplicate ack or
        // register -> no second poll. Constraint: Active/Waiting/Completed never
        // coexist in parallel maps for one durable task id.
        let supervisor = BackgroundTaskSupervisor::new("worker");
        let id = id();
        supervisor.wait(id.clone(), wait_candidate(1));
        assert!(supervisor.wait_candidate(&id).is_none(), "R1");
        assert!(supervisor.register(&id).is_some(), "R2 setup");
        supervisor.wait(id.clone(), wait_candidate(1));
        assert!(supervisor.wait_candidate(&id).is_some(), "R2");
        assert!(supervisor.completion(&id).is_none(), "R2");
        assert!(
            supervisor.resume_wait(&id, &completion(2).fence).is_none(),
            "R3"
        );
        assert!(
            supervisor.resume_wait(&id, &completion(1).fence).is_some(),
            "R4"
        );
        assert!(supervisor.is_active(&id), "R4");
        assert!(
            supervisor.resume_wait(&id, &completion(1).fence).is_none(),
            "R5"
        );
        assert!(supervisor.register(&id).is_none(), "R5");
    }

    #[test]
    fn matching_terminal_candidate_dominates_wait_in_the_same_slot() {
        // Cause/effect table: R1 Active->Waiting stores one continuation
        // candidate; R2 matching-fence Terminal races before wait commit and
        // atomically replaces it; R3 delayed Wait/duplicate Terminal stutter.
        // The StepStart observer can therefore never fold Waiting after a known
        // terminal outcome for the same attempt.
        let supervisor = BackgroundTaskSupervisor::new("worker");
        let id = id();
        assert!(supervisor.register(&id).is_some());
        supervisor.wait(id.clone(), wait_candidate(1));
        supervisor.complete(id.clone(), completion(1));
        assert!(supervisor.wait_candidate(&id).is_none(), "R2");
        assert!(supervisor.completion(&id).is_some(), "R2");
        supervisor.wait(id.clone(), wait_candidate(1));
        supervisor.complete(id.clone(), completion(2));
        assert_eq!(supervisor.completion(&id).unwrap().fence.epoch, 1, "R3");
    }
}
