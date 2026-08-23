//! Fire-and-forget auxiliary runs with a bounded drain.
//!
//! Out-of-band aux agents (memory extraction, dream) run *after* a main Run
//! finishes and must not block it, yet they should still be given a chance to
//! finish before the process exits — otherwise a memory write is lost on
//! shutdown. [`BackgroundRuns`] is that seam: [`spawn`](BackgroundRuns::spawn)
//! detaches a task, [`drain`](BackgroundRuns::drain) awaits the in-flight ones up
//! to a timeout so a well-behaved shutdown flushes them without a hang blocking
//! exit forever.

use std::collections::HashMap;
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
}

#[derive(Default)]
struct BackgroundActivity {
    shared: std::sync::Mutex<HashMap<(String, String), usize>>,
    changed: tokio::sync::Notify,
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
        let activity = self.activity.clone();
        if let BackgroundWorkClass::SharedEnvironment {
            session_id,
            generation_id,
        } = &class
        {
            *activity
                .shared
                .lock()
                .expect("background activity mutex poisoned")
                .entry((session_id.clone(), generation_id.clone()))
                .or_default() += 1;
        }
        self.tasks.lock().await.spawn(
            async move {
                fut.await;
                if let BackgroundWorkClass::SharedEnvironment {
                    session_id,
                    generation_id,
                } = class
                {
                    let mut shared = activity
                        .shared
                        .lock()
                        .expect("background activity mutex poisoned");
                    let key = (session_id, generation_id);
                    if let Some(count) = shared.get_mut(&key) {
                        *count -= 1;
                        if *count == 0 {
                            shared.remove(&key);
                        }
                    }
                    drop(shared);
                    activity.changed.notify_waiters();
                }
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

    /// Wait only for work that can mutate this exact environment generation.
    /// External durable work and caches never retain Session compute.
    pub async fn quiesce_shared_environment(
        &self,
        session_id: &str,
        generation_id: &str,
        timeout: Duration,
    ) -> bool {
        tokio::time::timeout(timeout, async {
            while self.has_shared_environment_work(session_id, generation_id) {
                self.activity.changed.notified().await;
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

    // Cause/effect design: C2=shared task active then complete; external durable
    // and cache tasks coexist. R4 retains the environment only for the matching
    // session+generation and then permits E2 when that exact count reaches zero.
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
}
