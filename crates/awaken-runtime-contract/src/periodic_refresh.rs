//! Reusable periodic-refresh primitive for registry managers.
//!
//! Spawns a background tokio task that calls a user-supplied async closure on a
//! fixed interval, with cooperative shutdown via a oneshot channel.

use std::future::Future;
use std::sync::Arc;

use parking_lot::Mutex;

use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

/// Internal state guarded by a mutex so the outer type is `Clone + Send + Sync`.
struct RefreshRuntime {
    stop_tx: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

/// A reusable handle for a single periodic background task.
///
/// Manages start / stop / is-running lifecycle so callers don't have to
/// duplicate oneshot + JoinHandle bookkeeping.
#[derive(Clone)]
pub struct PeriodicRefresher {
    runtime: Arc<Mutex<Option<RefreshRuntime>>>,
}

impl PeriodicRefresher {
    /// Create a new refresher with no running task.
    pub fn new() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(None)),
        }
    }

    /// Start a periodic background task.
    ///
    /// `refresh_fn` is an async factory called every `interval`.  It receives
    /// no arguments — callers should capture any shared state via `Arc` /
    /// `Weak` in the closure.
    ///
    /// Returns `true` on success.  Returns `Err(msg)` when:
    /// - `interval` is zero
    /// - a task is already running
    /// - no tokio runtime is available
    pub fn start<F, Fut>(&self, interval: std::time::Duration, refresh_fn: F) -> Result<(), String>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if interval.is_zero() {
            return Err("interval must be non-zero".into());
        }

        let handle = Handle::try_current().map_err(|e| e.to_string())?;

        let mut guard = self.runtime.lock();
        if guard.as_ref().is_some_and(|rt| !rt.join.is_finished()) {
            return Err("periodic refresh already running".into());
        }

        let (stop_tx, stop_rx) = oneshot::channel();
        let join = handle.spawn(periodic_loop(interval, stop_rx, refresh_fn));

        *guard = Some(RefreshRuntime {
            stop_tx: Some(stop_tx),
            join,
        });
        Ok(())
    }

    /// Stop the running task and wait for it to finish.
    /// Returns `true` if a task was actually running.
    pub async fn stop(&self) -> bool {
        let runtime = {
            let mut guard = self.runtime.lock();
            guard.take()
        };

        let Some(mut runtime) = runtime else {
            return false;
        };

        if let Some(stop_tx) = runtime.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        let _ = runtime.join.await;
        true
    }

    /// Check whether the background task is still running.
    ///
    /// Also cleans up finished tasks so a subsequent [`start`](Self::start)
    /// can succeed.
    pub fn is_running(&self) -> bool {
        let mut guard = self.runtime.lock();
        if guard.as_ref().is_some_and(|rt| rt.join.is_finished()) {
            *guard = None;
            return false;
        }
        guard.is_some()
    }
}

impl Default for PeriodicRefresher {
    fn default() -> Self {
        Self::new()
    }
}

async fn periodic_loop<F, Fut>(
    interval: std::time::Duration,
    mut stop_rx: oneshot::Receiver<()>,
    refresh_fn: F,
) where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Skip the immediate first tick so we wait one full interval before the
    // first refresh (matching the existing behaviour of all three callers).
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            _ = ticker.tick() => {
                refresh_fn().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn start_stop_basic() {
        let refresher = PeriodicRefresher::new();
        let counter = Arc::new(AtomicU32::new(0));

        let c = counter.clone();
        refresher
            .start(Duration::from_millis(10), move || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                }
            })
            .unwrap();

        assert!(refresher.is_running());

        // Let a few ticks run.
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(refresher.stop().await);
        assert!(!refresher.is_running());
        assert!(counter.load(Ordering::Relaxed) > 0);
    }

    #[tokio::test]
    async fn zero_interval_rejected() {
        let refresher = PeriodicRefresher::new();
        let err = refresher.start(Duration::ZERO, || async {}).unwrap_err();
        assert!(err.contains("non-zero"));
    }

    #[tokio::test]
    async fn double_start_rejected() {
        let refresher = PeriodicRefresher::new();
        refresher
            .start(Duration::from_secs(60), || async {})
            .unwrap();
        let err = refresher
            .start(Duration::from_secs(60), || async {})
            .unwrap_err();
        assert!(err.contains("already running"));
        refresher.stop().await;
    }

    #[tokio::test]
    async fn stop_when_not_running() {
        let refresher = PeriodicRefresher::new();
        assert!(!refresher.stop().await);
    }

    #[tokio::test]
    async fn can_restart_after_stop() {
        let refresher = PeriodicRefresher::new();
        refresher
            .start(Duration::from_millis(10), || async {})
            .unwrap();
        refresher.stop().await;

        // Should be able to start again.
        refresher
            .start(Duration::from_millis(10), || async {})
            .unwrap();
        assert!(refresher.is_running());
        refresher.stop().await;
    }
}
