//! Fire-and-forget auxiliary runs with a bounded drain.
//!
//! Out-of-band aux agents (memory extraction, dream) run *after* a main Run
//! finishes and must not block it, yet they should still be given a chance to
//! finish before the process exits — otherwise a memory write is lost on
//! shutdown. [`BackgroundRuns`] is that seam: [`spawn`](BackgroundRuns::spawn)
//! detaches a task, [`drain`](BackgroundRuns::drain) awaits the in-flight ones up
//! to a timeout so a well-behaved shutdown flushes them without a hang blocking
//! exit forever.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tracing::Instrument;

/// A registry of detached background tasks that can be drained before shutdown.
#[derive(Default)]
pub struct BackgroundRuns {
    tasks: Mutex<JoinSet<()>>,
    activity: Arc<BackgroundActivity>,
    admissions:
        std::sync::Mutex<HashMap<(String, String), std::sync::Weak<SharedToolExecutionAdmission>>>,
}

#[derive(Default)]
struct BackgroundActivity {
    shared: std::sync::Mutex<HashMap<(String, String), usize>>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct SharedToolExecutionAdmission {
    state: std::sync::Mutex<(u64, BTreeMap<u64, awaken_runtime_contract::ToolConcurrency>)>,
    changed: tokio::sync::Notify,
}

struct SharedToolExecutionPermit {
    admission: Arc<SharedToolExecutionAdmission>,
    id: u64,
}

impl awaken_runtime_contract::ToolExecutionPermit for SharedToolExecutionPermit {}

impl Drop for SharedToolExecutionPermit {
    fn drop(&mut self) {
        self.admission
            .state
            .lock()
            .expect("shared tool admission mutex poisoned")
            .1
            .remove(&self.id);
        self.admission.changed.notify_waiters();
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::ToolExecutionAdmission for SharedToolExecutionAdmission {
    async fn acquire(
        self: Arc<Self>,
        claim: awaken_runtime_contract::ToolConcurrency,
    ) -> Result<
        Box<dyn awaken_runtime_contract::ToolExecutionPermit>,
        awaken_runtime_contract::tool::ToolError,
    > {
        loop {
            let notified = self.changed.notified();
            {
                let mut state = self
                    .state
                    .lock()
                    .expect("shared tool admission mutex poisoned");
                if state
                    .1
                    .values()
                    .all(|active| active.compatible_with(&claim))
                {
                    state.0 = state.0.checked_add(1).ok_or_else(|| {
                        awaken_runtime_contract::tool::ToolError::Execution(
                            "shared tool admission sequence exhausted".into(),
                        )
                    })?;
                    let id = state.0;
                    state.1.insert(id, claim);
                    return Ok(Box::new(SharedToolExecutionPermit {
                        admission: self.clone(),
                        id,
                    }));
                }
            }
            notified.await;
        }
    }
}

/// Releases one admission from the existing generation-scoped activity map on
/// every task exit path, including panic unwind and Tokio cancellation/drop.
struct SharedEnvironmentActivityGuard {
    activity: Arc<BackgroundActivity>,
    key: (String, String),
}

impl SharedEnvironmentActivityGuard {
    fn acquire(activity: Arc<BackgroundActivity>, class: &BackgroundWorkClass) -> Option<Self> {
        let BackgroundWorkClass::SharedEnvironment {
            session_id,
            generation_id,
        } = class
        else {
            return None;
        };
        let key = (session_id.clone(), generation_id.clone());
        *activity
            .shared
            .lock()
            .expect("background activity mutex poisoned")
            .entry(key.clone())
            .or_default() += 1;
        Some(Self { activity, key })
    }
}

impl Drop for SharedEnvironmentActivityGuard {
    fn drop(&mut self) {
        let mut shared = self
            .activity
            .shared
            .lock()
            .expect("background activity mutex poisoned");
        let remove = shared.get_mut(&self.key).is_some_and(|count| {
            *count -= 1;
            *count == 0
        });
        if remove {
            shared.remove(&self.key);
        }
        drop(shared);
        self.activity.changed.notify_waiters();
    }
}

/// Every detached task must state whether it can mutate one Session
/// Environment. This replaces inference from Tokio task identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackgroundWorkClass {
    // No current detached caller is allowed to touch a Session Environment;
    // this variant is the mandatory admission token for future callers and is
    // exercised by the quiescence conformance test.
    #[allow(dead_code)]
    SharedEnvironment {
        session_id: String,
        generation_id: String,
    },
    ExternalDurable {
        durable_intent_id: String,
    },
    EphemeralCache,
}

impl BackgroundRuns {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the one process admission shared by every foreground and detached
    /// call bound to this exact Session Environment generation. Weak indexing
    /// prevents completed Session generations from becoming a second registry.
    pub(crate) fn tool_execution_admission(
        &self,
        session_id: &str,
        generation_id: &str,
    ) -> Arc<dyn awaken_runtime_contract::ToolExecutionAdmission> {
        let key = (session_id.to_string(), generation_id.to_string());
        let mut admissions = self
            .admissions
            .lock()
            .expect("shared tool admission registry mutex poisoned");
        if let Some(existing) = admissions.get(&key).and_then(std::sync::Weak::upgrade) {
            return existing;
        }
        let admission = Arc::new(SharedToolExecutionAdmission::default());
        admissions.insert(key, Arc::downgrade(&admission));
        admission
    }

    /// Detach `fut` to run in the background. It is tracked so [`drain`] can await
    /// it; a panic in the task is isolated (JoinSet surfaces it only on join, and
    /// drain swallows it — a background aux run is best-effort).
    ///
    /// The detached task is linked into the trace that spawned it: `tokio::spawn`
    /// starts a task with no ambient span, so an aux sub-run (memory extraction,
    /// dream) would otherwise emit a disconnected trace root. We attach a child
    /// span of the *current* span, so the aux run's spans nest under the
    /// originating Run's trace. The child holds only the parent's id, so the Run
    /// span still closes on time while the aux run continues.
    pub async fn spawn(
        &self,
        class: BackgroundWorkClass,
        fut: impl Future<Output = ()> + Send + 'static,
    ) {
        let span = tracing::info_span!(
            parent: &tracing::Span::current(),
            "aux.background",
            otel.kind = "internal"
        );
        let activity_guard = SharedEnvironmentActivityGuard::acquire(self.activity.clone(), &class);
        self.tasks.lock().await.spawn(
            async move {
                let _activity_guard = activity_guard;
                fut.await;
            }
            .instrument(span),
        );
    }

    #[must_use]
    pub fn has_shared_environment_work(&self, session_id: &str, generation_id: &str) -> bool {
        self.activity
            .shared
            .lock()
            .expect("background activity mutex poisoned")
            .contains_key(&(session_id.to_string(), generation_id.to_string()))
    }

    fn shared_environment_waiter_with_probe<'a>(
        &'a self,
        session_id: &str,
        generation_id: &str,
        after_check: impl FnOnce(),
    ) -> Option<tokio::sync::futures::Notified<'a>> {
        // `notify_waiters` does not retain a permit for a future listener. A
        // Notified created first records its generation immediately, so a task
        // dropping after the count check cannot wake before registration.
        let changed = self.activity.changed.notified();
        let active = self.has_shared_environment_work(session_id, generation_id);
        after_check();
        active.then_some(changed)
    }

    /// Wait only for work that can mutate this exact environment generation.
    /// External durable work and caches never retain Session compute.
    pub async fn quiesce_shared_environment(
        &self,
        session_id: &str,
        generation_id: &str,
        timeout: Duration,
    ) -> bool {
        tokio::time::timeout(timeout, async {
            while let Some(changed) =
                self.shared_environment_waiter_with_probe(session_id, generation_id, || {})
            {
                changed.await;
            }
        })
        .await
        .is_ok()
    }

    /// Await all in-flight background tasks, up to `timeout`. Returns `true` if
    /// every task finished, `false` if the timeout fired first (some are still
    /// running). Best-effort: a task that panicked counts as finished.
    pub async fn drain(&self, timeout: Duration) -> bool {
        let mut tasks = self.tasks.lock().await;
        let drained = tokio::time::timeout(timeout, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        drained.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn drain_awaits_all_spawned_tasks() {
        let bg = BackgroundRuns::new();
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..5 {
            let c = counter.clone();
            bg.spawn(BackgroundWorkClass::EphemeralCache, async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                c.fetch_add(1, Ordering::SeqCst);
            })
            .await;
        }
        let finished = bg.drain(Duration::from_secs(5)).await;
        assert!(finished, "drain should complete within the timeout");
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn drain_returns_false_when_a_task_outlives_the_timeout() {
        let bg = BackgroundRuns::new();
        bg.spawn(BackgroundWorkClass::EphemeralCache, async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
        .await;
        let finished = bg.drain(Duration::from_millis(20)).await;
        assert!(
            !finished,
            "drain should time out while the task is still running"
        );
    }

    #[tokio::test]
    async fn a_panicking_task_does_not_break_drain() {
        let bg = BackgroundRuns::new();
        bg.spawn(BackgroundWorkClass::EphemeralCache, async {
            panic!("boom");
        })
        .await;
        let finished = bg.drain(Duration::from_secs(5)).await;
        assert!(
            finished,
            "a panicked background task still counts as drained"
        );
    }

    // Cause/effect decision table: C1=SharedEnvironment task exit is normal;
    // C2=query is exact or another generation; C3=external/cache work coexists.
    // R1 exact+active -> E1 quiescence blocks; R2 other generation -> E2 it is
    // immediately quiescent; R3 exact+normal completion -> E3 the activity
    // count is released; R4 external/cache -> E4 no environment count changes.
    #[tokio::test]
    async fn shared_environment_quiescence_is_generation_scoped() {
        let bg = BackgroundRuns::new();
        let release = Arc::new(tokio::sync::Notify::new());
        let task_release = release.clone();
        bg.spawn(
            BackgroundWorkClass::SharedEnvironment {
                session_id: "s1".into(),
                generation_id: "g1".into(),
            },
            async move { task_release.notified().await },
        )
        .await;
        bg.spawn(
            BackgroundWorkClass::ExternalDurable {
                durable_intent_id: "intent".into(),
            },
            async {},
        )
        .await;
        assert!(
            !bg.quiesce_shared_environment("s1", "g1", Duration::from_millis(10))
                .await
        );
        assert!(
            bg.quiesce_shared_environment("s1", "g2", Duration::from_millis(10))
                .await
        );
        release.notify_waiters();
        assert!(
            bg.quiesce_shared_environment("s1", "g1", Duration::from_secs(1))
                .await
        );
    }

    #[tokio::test]
    async fn tool_admission_is_shared_by_exact_environment_generation() {
        // Cause/effect decision table: R1 two callers resolve the same
        // Session+generation -> conflicting writes share one active map and the
        // second blocks; R2 first permit drops -> second enters; R3 another
        // generation -> independent admission. This is the foreground/background
        // confluence: callers receive one port, not observer-local locks.
        let background = BackgroundRuns::new();
        let first = background.tool_execution_admission("session", "generation");
        let same = background.tool_execution_admission("session", "generation");
        let other = background.tool_execution_admission("session", "other-generation");
        let claim = awaken_runtime_contract::ToolConcurrency::Serial;
        let guard = first.clone().acquire(claim.clone()).await.unwrap();
        let mut blocked = tokio::spawn(async move { same.acquire(claim).await.unwrap() });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut blocked)
                .await
                .is_err(),
            "R1"
        );
        let independent = other
            .acquire(awaken_runtime_contract::ToolConcurrency::Serial)
            .await
            .expect("R3");
        drop(independent);
        drop(guard);
        let resumed = tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .expect("R2 wake")
            .expect("R2 join");
        drop(resumed);
    }

    #[tokio::test]
    async fn panicking_shared_environment_work_releases_exact_generation() {
        // Cause/effect decision table: C1=SharedEnvironment task panics after
        // admission; C2=query is exact or another generation. R1 exact+active
        // -> E1 quiescence blocks; R2 other generation -> E2 remains quiescent;
        // R3 panic unwinds the task -> E3 exact activity is released and drain
        // still treats the best-effort task as finished.
        let bg = BackgroundRuns::new();
        let (panic_now, admitted) = tokio::sync::oneshot::channel::<()>();
        bg.spawn(
            BackgroundWorkClass::SharedEnvironment {
                session_id: "panic-session".into(),
                generation_id: "panic-generation".into(),
            },
            async move {
                let _ = admitted.await;
                panic!("shared background panic");
            },
        )
        .await;

        assert!(
            !bg.quiesce_shared_environment(
                "panic-session",
                "panic-generation",
                Duration::from_millis(10),
            )
            .await,
            "R1/E1 exact generation remains active"
        );
        assert!(
            bg.quiesce_shared_environment(
                "panic-session",
                "other-generation",
                Duration::from_millis(10),
            )
            .await,
            "R2/E2 another generation is independent"
        );

        panic_now.send(()).expect("release panicking task");
        assert!(bg.drain(Duration::from_secs(1)).await, "R3 drain");
        assert!(
            bg.quiesce_shared_environment(
                "panic-session",
                "panic-generation",
                Duration::from_secs(1),
            )
            .await,
            "R3/E3 panic releases exact generation"
        );
    }

    #[tokio::test]
    async fn aborted_shared_environment_work_releases_exact_generation() {
        // Cause/effect decision table: C1=SharedEnvironment task is pending;
        // C2=query is exact or another generation; C3=the owning JoinSet aborts
        // the task. R1 exact+pending -> E1 quiescence blocks; R2 other generation
        // -> E2 remains quiescent; R3 abort/drop -> E3 exact activity is released
        // and the JoinSet remains drainable.
        let bg = BackgroundRuns::new();
        let (_keep_pending, pending) = tokio::sync::oneshot::channel::<()>();
        bg.spawn(
            BackgroundWorkClass::SharedEnvironment {
                session_id: "abort-session".into(),
                generation_id: "abort-generation".into(),
            },
            async move {
                let _ = pending.await;
            },
        )
        .await;

        assert!(
            !bg.quiesce_shared_environment(
                "abort-session",
                "abort-generation",
                Duration::from_millis(10),
            )
            .await,
            "R1/E1 exact generation remains active"
        );
        assert!(
            bg.quiesce_shared_environment(
                "abort-session",
                "other-generation",
                Duration::from_millis(10),
            )
            .await,
            "R2/E2 another generation is independent"
        );

        bg.tasks.lock().await.abort_all();
        assert!(bg.drain(Duration::from_secs(1)).await, "R3 drain");
        assert!(
            bg.quiesce_shared_environment(
                "abort-session",
                "abort-generation",
                Duration::from_secs(1),
            )
            .await,
            "R3/E3 abort releases exact generation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completion_between_activity_check_and_waiter_creation_is_not_lost() {
        // Cause/effect decision table for one exact generation:
        // R1 task completed before quiesce creates its waiter -> E1 the count is
        // already absent and quiesce succeeds without waiting; R2 task remains
        // active after the check -> E2 its waiter remains pending; R3 task drops
        // in the former check-to-waiter-registration interval -> E3 the already
        // registered waiter observes that release; R4 task remains active past
        // the deadline -> E4 only that case times out. Existing normal/timeout
        // cases cover R1/R2/R4; this probe deterministically forces R3.
        let bg = BackgroundRuns::new();
        let (complete, pending) = tokio::sync::oneshot::channel::<()>();
        bg.spawn(
            BackgroundWorkClass::SharedEnvironment {
                session_id: "race-session".into(),
                generation_id: "race-generation".into(),
            },
            async move {
                let _ = pending.await;
            },
        )
        .await;

        let waiter = bg
            .shared_environment_waiter_with_probe("race-session", "race-generation", || {
                complete.send(()).expect("release race task");
                let deadline = std::time::Instant::now() + Duration::from_secs(1);
                while bg.has_shared_environment_work("race-session", "race-generation") {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "R3 task must drop while the ordering probe is active"
                    );
                    std::thread::yield_now();
                }
            })
            .expect("R3 task was active at the count check");

        assert!(
            tokio::time::timeout(Duration::from_millis(50), waiter)
                .await
                .is_ok(),
            "R3/E3 completion between the check and the former waiter creation must wake"
        );
        assert!(bg.drain(Duration::from_secs(1)).await, "R3 drain");
        assert!(
            bg.quiesce_shared_environment(
                "race-session",
                "race-generation",
                Duration::from_millis(50),
            )
            .await,
            "R1/E1 completed work remains quiescent"
        );
    }
}
