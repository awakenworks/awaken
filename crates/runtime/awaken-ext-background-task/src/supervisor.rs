use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(all(test, feature = "loom"))]
use loom::sync::Mutex;
#[cfg(not(all(test, feature = "loom")))]
use std::sync::Mutex;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionFoldPlan {
    /// Stage the exact candidate while retaining it for commit-loss retry.
    StageRetainingCandidate,
    /// A strictly newer committed epoch makes the observed candidate stale.
    RetireStale,
    /// No current attempt or an incomparable snapshot cannot authorize either.
    Preserve,
}

/// Sole scalar admission plan from a process completion candidate to the
/// ordinary durable fold. The supervisor applies retirement under its one slot
/// lock; a matching candidate is deliberately read without being consumed.
fn completion_fold_plan(
    candidate: &TaskFence,
    committed: Option<&TaskFence>,
) -> CompletionFoldPlan {
    let Some(committed) = committed else {
        return CompletionFoldPlan::Preserve;
    };
    if candidate == committed {
        CompletionFoldPlan::StageRetainingCandidate
    } else if candidate.epoch < committed.epoch {
        CompletionFoldPlan::RetireStale
    } else {
        CompletionFoldPlan::Preserve
    }
}

enum SupervisorEntry {
    Active {
        fence: TaskFence,
        token: CancellationToken,
    },
    Waiting(BackgroundTaskWaitCandidate),
    Completed(BackgroundTaskCompletion),
}

impl SupervisorEntry {
    /// Both initial launch and committed wait resumption mint exactly this
    /// fence/token pair; there is no unfenced Active construction path.
    fn active(fence: &TaskFence) -> (Self, CancellationToken) {
        let token = CancellationToken::new();
        (
            Self::Active {
                fence: fence.clone(),
                token: token.clone(),
            },
            token,
        )
    }

    fn fence(&self) -> &TaskFence {
        match self {
            Self::Active { fence, .. } => fence,
            Self::Waiting(candidate) => &candidate.fence,
            Self::Completed(completion) => &completion.fence,
        }
    }
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

    /// Claim process-local launch ownership for a committed attempt. Same/older
    /// epochs cannot relaunch; a strictly newer committed epoch supersedes the
    /// old projection, including an Active executor that never returned.
    pub fn register(&self, id: &BackgroundTaskId, fence: &TaskFence) -> Option<CancellationToken> {
        let mut entries = self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned");
        if let Some(entry) = entries.get(id)
            && fence.epoch <= entry.fence().epoch
        {
            return None;
        }
        // Epoch replacement is not durable cancellation intent. In
        // particular, signalling the old token would ask an old remote poll
        // to cancel the shared remote task after the new owner took over.
        let (entry, token) = SupervisorEntry::active(fence);
        entries.insert(id.clone(), entry);
        Some(token)
    }

    #[must_use]
    pub fn is_active(&self, id: &BackgroundTaskId, fence: &TaskFence) -> bool {
        matches!(
            self.entries
                .lock()
                .expect("background supervisor mutex poisoned")
                .get(id),
            Some(SupervisorEntry::Active { fence: active, .. }) if active == fence
        )
    }

    pub fn cancel(&self, id: &BackgroundTaskId, fence: &TaskFence) {
        if let Some(SupervisorEntry::Active {
            fence: active,
            token,
        }) = self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned")
            .get(id)
            && active == fence
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
        if matches!(entries.get(&id), Some(SupervisorEntry::Active { fence, .. }) if *fence == candidate.fence)
        {
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
            Some(SupervisorEntry::Active { .. } | SupervisorEntry::Completed(_)) | None => None,
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
        let (entry, token) = SupervisorEntry::active(fence);
        entries.insert(id.clone(), entry);
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
            Some(entry @ (SupervisorEntry::Active { .. } | SupervisorEntry::Waiting(_))) => {
                entry.fence() == &completion.fence
            }
            Some(SupervisorEntry::Completed(_)) | None => false,
        };
        if may_complete {
            entries.insert(id, SupervisorEntry::Completed(completion));
        }
    }

    /// Read a completed projection without releasing its deduplication guard.
    /// The guard is retired only after an observer sees durable terminal truth
    /// or a strictly newer committed attempt supersedes this candidate.
    #[must_use]
    pub fn completion(&self, id: &BackgroundTaskId) -> Option<BackgroundTaskCompletion> {
        match self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned")
            .get(id)
        {
            Some(SupervisorEntry::Completed(completion)) => Some(completion.clone()),
            Some(SupervisorEntry::Active { .. } | SupervisorEntry::Waiting(_)) | None => None,
        }
    }

    /// Admit one completion candidate against the exact attempt in the caller's
    /// committed snapshot. A matching candidate remains in the slot so a lost
    /// or rejected State commit can stage the same result on the next StepStart.
    /// A strictly older candidate is retired atomically; same/newer incomparable
    /// candidates remain for a snapshot that can establish their authority.
    pub(crate) fn completion_for_fold(
        &self,
        id: &BackgroundTaskId,
        committed: Option<&TaskFence>,
    ) -> Option<BackgroundTaskCompletion> {
        let mut entries = self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned");
        let Some(SupervisorEntry::Completed(completion)) = entries.get(id) else {
            return None;
        };
        match completion_fold_plan(&completion.fence, committed) {
            CompletionFoldPlan::StageRetainingCandidate => Some(completion.clone()),
            CompletionFoldPlan::RetireStale => {
                entries.remove(id);
                None
            }
            CompletionFoldPlan::Preserve => None,
        }
    }

    /// Retire an observed projection only when committed truth has a strictly
    /// newer epoch. An older durable snapshot cannot evict a newer completion;
    /// a replacement between observation and locking must remain untouched.
    pub fn retire(&self, id: &BackgroundTaskId, fence: &TaskFence, committed: &TaskFence) {
        let mut entries = self
            .entries
            .lock()
            .expect("background supervisor mutex poisoned");
        if fence.epoch < committed.epoch
            && entries.get(id).is_some_and(|entry| entry.fence() == fence)
        {
            entries.remove(id);
        }
    }

    /// Release any process projection only after observing durable Ended truth.
    /// Unlike stale-candidate retirement this needs no attempt fence: Ended is
    /// absorbing and has no live attempt. Never call this from a stale callback.
    pub fn retire_terminal(&self, id: &BackgroundTaskId) {
        self.entries
            .lock()
            .expect("background supervisor mutex poisoned")
            .remove(id);
    }
}

#[cfg(test)]
mod fixtures {
    use super::*;

    pub(super) fn id() -> BackgroundTaskId {
        BackgroundTaskId::new("task").expect("fixture id")
    }

    pub(super) fn completion(epoch: u64) -> BackgroundTaskCompletion {
        BackgroundTaskCompletion {
            fence: TaskFence {
                worker_id: "worker".into(),
                epoch,
            },
            end: BackgroundTaskEnd::Cancelled,
        }
    }

    pub(super) fn wait_candidate(epoch: u64) -> BackgroundTaskWaitCandidate {
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
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::{fixtures::*, *};

    #[test]
    fn only_newer_committed_epochs_replace_existing_projection() {
        // Causes: Active/Waiting/Completed; incoming epoch below/equal/above
        // current (including u64::MAX); same/different owner (24 rules).
        // R1 incoming <= current -> slot and token unchanged regardless of owner;
        // R2 incoming > current -> exact new Active fence, candidate discarded
        // only because its epoch is superseded. Both rules preserve the old
        // cancellation token: ownership replacement is not cancellation intent.
        // Constraint: caller supplies a committed claim, never an input proposal.
        for phase in 0..3 {
            for epoch in [1, 2, 3, u64::MAX] {
                for owner in ["worker", "other"] {
                    let supervisor = BackgroundTaskSupervisor::new("worker");
                    let token = supervisor.register(&id(), &completion(2).fence).unwrap();
                    if phase == 1 {
                        supervisor.wait(id(), wait_candidate(2));
                    }
                    if phase == 2 {
                        supervisor.complete(id(), completion(2));
                    }
                    let fence = TaskFence {
                        worker_id: owner.into(),
                        epoch,
                    };
                    assert_eq!(supervisor.register(&id(), &fence).is_some(), epoch > 2);
                    assert!(!token.is_cancelled());
                    let entries = supervisor.entries.lock().unwrap();
                    let entry = entries.get(&id()).unwrap();
                    let current = completion(2).fence;
                    assert_eq!(entry.fence(), if epoch > 2 { &fence } else { &current });
                    if epoch > 2 {
                        assert!(matches!(entry, SupervisorEntry::Active { .. }));
                    } else {
                        assert!(matches!(
                            (phase, entry),
                            (0, SupervisorEntry::Active { .. })
                                | (1, SupervisorEntry::Waiting(_))
                                | (2, SupervisorEntry::Completed(_))
                        ));
                    }
                }
            }
        }
    }

    #[test]
    fn stale_snapshot_cannot_retire_newer_projection() {
        // Causes: three slot phases x committed epoch below/equal/above the
        // observed epoch (nine rules). R1 older/equal snapshot -> preserve even
        // if its owner differs; R2 newer committed epoch -> retire exact observed
        // slot. Incomparable same-epoch owners fail closed, not by deleting one.
        for phase in 0..3 {
            for epoch in [1, 2, 3] {
                let supervisor = BackgroundTaskSupervisor::new("worker");
                supervisor.register(&id(), &completion(2).fence).unwrap();
                if phase == 1 {
                    supervisor.wait(id(), wait_candidate(2));
                }
                if phase == 2 {
                    supervisor.complete(id(), completion(2));
                }
                supervisor.retire(
                    &id(),
                    &completion(2).fence,
                    &TaskFence {
                        worker_id: "other".into(),
                        epoch,
                    },
                );
                assert_eq!(
                    supervisor.entries.lock().unwrap().contains_key(&id()),
                    epoch <= 2
                );
            }
        }
    }

    #[test]
    fn every_projection_mutation_requires_exact_owner_and_epoch() {
        // Causes: slot phase Active/Waiting/Completed, owner equality, epoch
        // equality, and command Wait/Complete/Cancel/Retire (48 rules).
        // Constraint: register and resume are the only Active constructors.
        // R1 either coordinate differs -> full slot stutter, no cancellation;
        // R2 exact Wait -> Waiting only from Active; R3 exact Complete ->
        // Completed from Active/Waiting; R4 exact Cancel -> token only in Active;
        // R5 exact Retire -> vacant. Completed absorbs Wait/Complete/Cancel.
        for phase in 0..3 {
            for same_owner in [false, true] {
                for same_epoch in [false, true] {
                    for command in 0..4 {
                        let supervisor = BackgroundTaskSupervisor::new("worker");
                        let id = id();
                        let current = completion(2).fence;
                        let token = supervisor.register(&id, &current).unwrap();
                        if phase == 1 {
                            supervisor.wait(id.clone(), wait_candidate(2));
                        }
                        if phase == 2 {
                            supervisor.complete(id.clone(), completion(2));
                        }
                        let fence = TaskFence {
                            worker_id: if same_owner { "worker" } else { "other" }.into(),
                            epoch: if same_epoch { 2 } else { 1 },
                        };
                        match command {
                            0 => supervisor.wait(
                                id.clone(),
                                BackgroundTaskWaitCandidate {
                                    fence: fence.clone(),
                                    wait: BackgroundWait::ExternalInput,
                                    renew_lease: true,
                                },
                            ),
                            1 => supervisor.complete(
                                id.clone(),
                                BackgroundTaskCompletion {
                                    fence: fence.clone(),
                                    end: BackgroundTaskEnd::Indeterminate {
                                        message: "candidate".into(),
                                    },
                                },
                            ),
                            2 => supervisor.cancel(&id, &fence),
                            _ => supervisor.retire(&id, &fence, &completion(3).fence),
                        }
                        let exact = same_owner && same_epoch;
                        assert_eq!(token.is_cancelled(), exact && phase == 0 && command == 2);
                        let expected = if !exact {
                            Some(phase)
                        } else {
                            match command {
                                0 if phase == 0 => Some(1),
                                1 => Some(2),
                                3 => None,
                                _ => Some(phase),
                            }
                        };
                        let entries = supervisor.entries.lock().unwrap();
                        let observed = entries.get(&id).map(|entry| {
                            assert_eq!(entry.fence(), &current);
                            match entry {
                                SupervisorEntry::Active { .. } => 0,
                                SupervisorEntry::Waiting(candidate) => {
                                    let accepted = exact && command == 0 && phase == 0;
                                    assert_eq!(candidate.renew_lease, accepted);
                                    assert_eq!(
                                        candidate.wait,
                                        if accepted {
                                            BackgroundWait::ExternalInput
                                        } else {
                                            wait_candidate(2).wait
                                        }
                                    );
                                    1
                                }
                                SupervisorEntry::Completed(completion) => {
                                    assert_eq!(
                                        completion.end,
                                        if exact && command == 1 && phase != 2 {
                                            BackgroundTaskEnd::Indeterminate {
                                                message: "candidate".into(),
                                            }
                                        } else {
                                            BackgroundTaskEnd::Cancelled
                                        }
                                    );
                                    2
                                }
                            }
                        });
                        assert_eq!(
                            observed, expected,
                            "phase={phase} owner={same_owner} epoch={same_epoch} command={command}"
                        );
                    }
                }
            }
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
        assert!(
            supervisor.register(&id, &completion(1).fence).is_some(),
            "C1/E1"
        );
        assert!(supervisor.is_active(&id, &completion(1).fence), "C1/E1");
        supervisor.complete(id.clone(), completion(1));
        assert!(!supervisor.is_active(&id, &completion(1).fence), "C2/E2");
        assert_eq!(supervisor.completion(&id).unwrap().fence.epoch, 1, "C2/E2");
        assert!(
            supervisor.register(&id, &completion(1).fence).is_none(),
            "C3/E3"
        );
        supervisor.complete(id.clone(), completion(2));
        assert_eq!(supervisor.completion(&id).unwrap().fence.epoch, 1, "C3/E3");
        supervisor.retire_terminal(&id);
        assert!(
            supervisor.register(&id, &completion(1).fence).is_some(),
            "C4/E4"
        );
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
        assert!(supervisor.register(&id, &completion(1).fence).is_some());
    }

    #[test]
    fn completion_fold_admission_retains_retry_and_retires_only_strictly_stale() {
        // Cause/effect decision table for the sole completion-fold admission:
        // C1 a Completed slot exists; C2 a committed attempt exists; C3 its
        // owner matches; C4 candidate epoch is below/equal/above committed.
        // R1 C1+C2+C3+equal -> return the exact candidate repeatedly and retain
        // the slot (E1 commit-loss retry); R2 C1+C2+below -> return nothing and
        // retire the exact stale slot (E2 inert); R3 missing attempt,
        // same-epoch different owner, or candidate above -> return nothing and
        // preserve the slot (E3 an older/incomparable snapshot cannot evict).
        // Constraint: durable Ended cleanup remains the separate absorbing-state
        // observation; this admission never claims that a staged command committed.
        for (committed, expected_first, expected_retained) in [
            (Some(("worker", 2)), true, true),
            (Some(("worker", 3)), false, false),
            (Some(("other", 2)), false, true),
            (Some(("worker", 1)), false, true),
            (None, false, true),
        ] {
            let supervisor = BackgroundTaskSupervisor::new("worker");
            let id = id();
            supervisor.register(&id, &completion(2).fence).unwrap();
            supervisor.complete(id.clone(), completion(2));
            let committed = committed.map(|(worker_id, epoch)| TaskFence {
                worker_id: worker_id.into(),
                epoch,
            });
            let first = supervisor.completion_for_fold(&id, committed.as_ref());
            assert_eq!(first.is_some(), expected_first);
            let second = supervisor.completion_for_fold(&id, committed.as_ref());
            assert_eq!(second.is_some(), expected_first, "R1 exact retry");
            assert_eq!(supervisor.completion(&id).is_some(), expected_retained);
        }
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
        assert!(
            supervisor.register(&id, &completion(1).fence).is_some(),
            "R2 setup"
        );
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
        assert!(supervisor.is_active(&id, &completion(1).fence), "R4");
        assert!(
            supervisor.resume_wait(&id, &completion(1).fence).is_none(),
            "R5"
        );
        assert!(
            supervisor.register(&id, &completion(1).fence).is_none(),
            "R5"
        );
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
        assert!(supervisor.register(&id, &completion(1).fence).is_some());
        supervisor.wait(id.clone(), wait_candidate(1));
        supervisor.complete(id.clone(), completion(1));
        assert!(supervisor.wait_candidate(&id).is_none(), "R2");
        assert!(supervisor.completion(&id).is_some(), "R2");
        supervisor.wait(id.clone(), wait_candidate(1));
        supervisor.complete(id.clone(), completion(2));
        assert_eq!(supervisor.completion(&id).unwrap().fence.epoch, 1, "R3");
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::thread;

    use super::{fixtures::*, *};

    #[test]
    fn stale_callbacks_and_retirement_cannot_mutate_replacement() {
        // Causes: old Waiting observation, replacement register, and
        // delayed old complete/wait/cancel/retire under arbitrary lock ordering.
        // R1 before replacement: old callbacks may affect only the old slot;
        // R2 after replacement: all old-fence mutations stutter. Effect: the new
        // Active slot and its uncancelled token survive. Each critical section
        // uses the production method, not a separate model of the map.
        loom::model(|| {
            let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker"));
            supervisor.register(&id(), &completion(1).fence).unwrap();
            supervisor.wait(id(), wait_candidate(1));
            let observed = supervisor.wait_candidate(&id()).unwrap();
            let replacing = supervisor.clone();
            let replacement =
                thread::spawn(move || replacing.register(&id(), &completion(2).fence).unwrap());
            let stale = supervisor.clone();
            let callbacks = thread::spawn(move || {
                stale.complete(id(), completion(1));
                stale.wait(id(), wait_candidate(1));
                stale.cancel(&id(), &observed.fence);
                let _ = stale.completion_for_fold(&id(), Some(&completion(2).fence));
            });
            let token = replacement.join().unwrap();
            callbacks.join().unwrap();
            assert!(supervisor.is_active(&id(), &completion(2).fence));
            assert!(!supervisor.is_active(&id(), &completion(1).fence));
            assert!(!token.is_cancelled());
            assert!(supervisor.completion(&id()).is_none());
            assert!(supervisor.wait_candidate(&id()).is_none());
        });
    }

    #[test]
    fn resumed_poll_retains_its_fence_against_late_callbacks() {
        // R1 exact committed wait ack -> one Active poll, retaining fence;
        // R2 wrong-owner or wrong-epoch callbacks before/after ack -> stutter.
        // Bounds: one slot, two threads, fixed representative identities. This
        // does not prove Tokio token internals or the durable commit precondition.
        loom::model(|| {
            let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker"));
            supervisor.register(&id(), &completion(2).fence).unwrap();
            supervisor.wait(id(), wait_candidate(2));
            let resuming = supervisor.clone();
            let resumed =
                thread::spawn(move || resuming.resume_wait(&id(), &completion(2).fence).unwrap());
            let stale = supervisor.clone();
            let callbacks = thread::spawn(move || {
                let mut wrong_owner = completion(2);
                wrong_owner.fence.worker_id = "other".into();
                stale.complete(id(), wrong_owner);
                stale.complete(id(), completion(1));
                stale.wait(id(), wait_candidate(1));
                stale.cancel(&id(), &completion(1).fence);
            });
            let token = resumed.join().unwrap();
            callbacks.join().unwrap();
            assert!(supervisor.is_active(&id(), &completion(2).fence));
            assert!(!token.is_cancelled());
        });
    }

    #[test]
    fn completion_dominates_wait_and_ack_in_every_interleaving() {
        // R1 matching terminal before wait -> wait/ack stutter;
        // R2 terminal between wait and ack -> ack stutters;
        // R3 terminal after ack -> resumed Active becomes Completed.
        // Effect: one retained terminal candidate, never a reopened poll.
        loom::model(|| {
            let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker"));
            supervisor.register(&id(), &completion(1).fence).unwrap();
            let waiting = supervisor.clone();
            let wait_and_ack = thread::spawn(move || {
                waiting.wait(id(), wait_candidate(1));
                let _ = waiting.resume_wait(&id(), &completion(1).fence);
            });
            let finishing = supervisor.clone();
            let terminal = thread::spawn(move || finishing.complete(id(), completion(1)));
            wait_and_ack.join().unwrap();
            terminal.join().unwrap();
            assert_eq!(
                supervisor.completion(&id()).unwrap().fence,
                completion(1).fence
            );
            assert!(supervisor.wait_candidate(&id()).is_none());
            assert!(supervisor.register(&id(), &completion(1).fence).is_none());
        });
    }

    #[test]
    fn duplicate_commit_ack_has_exactly_one_poll_owner() {
        // Cause: two matching committed wait acknowledgements race. Rule: the
        // first consumes Waiting; the second observes Active and stutters.
        // Effect: exactly one token is minted and its fence remains attached.
        loom::model(|| {
            let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker"));
            supervisor.register(&id(), &completion(1).fence).unwrap();
            supervisor.wait(id(), wait_candidate(1));
            let first = supervisor.clone();
            let first =
                thread::spawn(move || first.resume_wait(&id(), &completion(1).fence).is_some());
            let second = supervisor.clone();
            let second =
                thread::spawn(move || second.resume_wait(&id(), &completion(1).fence).is_some());
            assert_ne!(first.join().unwrap(), second.join().unwrap());
            assert!(supervisor.is_active(&id(), &completion(1).fence));
        });
    }

    #[test]
    fn completion_fold_observation_cannot_consume_newer_registration() {
        // Causes: an exact epoch-1 completion fold races a committed epoch-2
        // registration under the production slot mutex. R1 fold first -> return
        // the retained epoch-1 receipt, then replacement installs epoch 2; R2
        // replacement first -> fold observes no Completed candidate and stutters.
        // Effect: both schedules end with the exact epoch-2 Active slot and its
        // live token. Constraint: whether an already-staged epoch-1 command wins
        // a ThreadCommit race is the durable CAS/async TCB, not this process lock.
        loom::model(|| {
            let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker"));
            supervisor.register(&id(), &completion(1).fence).unwrap();
            supervisor.complete(id(), completion(1));
            let folding = supervisor.clone();
            let folded = thread::spawn(move || {
                folding.completion_for_fold(&id(), Some(&completion(1).fence))
            });
            let replacing = supervisor.clone();
            let replacement =
                thread::spawn(move || replacing.register(&id(), &completion(2).fence).unwrap());
            if let Some(receipt) = folded.join().unwrap() {
                assert_eq!(receipt.fence, completion(1).fence);
            }
            let token = replacement.join().unwrap();
            assert!(supervisor.is_active(&id(), &completion(2).fence));
            assert!(!token.is_cancelled());
        });
    }
}

#[cfg(kani)]
#[kani::proof]
fn process_completion_fold_retries_exact_candidate_and_stale_is_inert() {
    // Cause/effect decision table over the production completion_fold_plan:
    // C1 committed attempt absent/present; C2 owner equal/different; C3 all u64
    // candidate and committed epochs. R1 present+exact owner+equal epoch yields
    // StageRetainingCandidate on every observation; R2 any candidate epoch below
    // committed yields RetireStale; R3 absent, same/newer mismatches yield Preserve.
    // Effects: only R1 can reach BackgroundTask::finish and it does not consume
    // the process candidate, so a rejected/lost durable commit retries identically;
    // R2/R3 authorize no State mutation. The supervisor mutex, StateCommand,
    // ThreadCommit/DB durability, heap allocation and async scheduling are outside
    // this scalar proof and remain covered by native/Loom evidence or the TCB.
    let candidate_epoch: u64 = kani::any();
    let committed_epoch: u64 = kani::any();
    let has_committed_attempt: bool = kani::any();
    let same_owner: bool = kani::any();
    let candidate = TaskFence {
        worker_id: "candidate-worker".into(),
        epoch: candidate_epoch,
    };
    let committed = TaskFence {
        worker_id: if same_owner {
            "candidate-worker"
        } else {
            "other-worker"
        }
        .into(),
        epoch: committed_epoch,
    };
    let plan = completion_fold_plan(&candidate, has_committed_attempt.then_some(&committed));
    let exact = has_committed_attempt && same_owner && candidate_epoch == committed_epoch;
    let strictly_stale = has_committed_attempt && candidate_epoch < committed_epoch;
    assert_eq!(plan == CompletionFoldPlan::StageRetainingCandidate, exact);
    assert_eq!(plan == CompletionFoldPlan::RetireStale, strictly_stale);
    assert_eq!(
        plan == CompletionFoldPlan::Preserve,
        !exact && !strictly_stale
    );
    if exact {
        assert_eq!(completion_fold_plan(&candidate, Some(&committed)), plan);
    }
}
