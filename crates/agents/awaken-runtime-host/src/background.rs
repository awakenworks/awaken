//! Fire-and-forget auxiliary runs with a bounded drain.
//!
//! Out-of-band aux agents (memory extraction, dream) run *after* a main turn
//! finishes and must not block it, yet they should still be given a chance to
//! finish before the process exits — otherwise a memory write is lost on
//! shutdown. [`BackgroundRuns`] is that seam: [`spawn`](BackgroundRuns::spawn)
//! detaches a task, [`drain`](BackgroundRuns::drain) awaits the in-flight ones up
//! to a timeout so a well-behaved shutdown flushes them without a hang blocking
//! exit forever.

use std::future::Future;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tracing::Instrument;

/// A registry of detached background tasks that can be drained before shutdown.
#[derive(Default)]
pub struct BackgroundRuns {
    tasks: Mutex<JoinSet<()>>,
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
    /// originating turn's trace. The child holds only the parent's id, so the turn
    /// span still closes on time while the aux run continues.
    pub async fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        let span = tracing::info_span!(
            parent: &tracing::Span::current(),
            "aux.background",
            otel.kind = "internal"
        );
        self.tasks.lock().await.spawn(fut.instrument(span));
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
            bg.spawn(async move {
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
        bg.spawn(async {
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
        bg.spawn(async {
            panic!("boom");
        })
        .await;
        let finished = bg.drain(Duration::from_secs(5)).await;
        assert!(
            finished,
            "a panicked background task still counts as drained"
        );
    }
}
