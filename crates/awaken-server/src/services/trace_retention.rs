//! ADR-0030 D6: scheduled prune of unreferenced traces.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use awaken_ext_observability::trace_store::TraceStore;

#[derive(Debug, Clone)]
pub struct RetentionConfig {
    pub ttl: Duration,
    pub interval: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(7 * 24 * 60 * 60),
            interval: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// Tick request: optionally carries a oneshot the loop signals after the
/// triggered prune cycle completes (used by tests for deterministic
/// synchronisation; production callers pass `None`).
type TickRequest = Option<tokio::sync::oneshot::Sender<()>>;

pub struct RetentionHandle {
    trigger: tokio::sync::mpsc::Sender<TickRequest>,
}

impl RetentionHandle {
    /// Manually trigger an immediate prune cycle. Best-effort: if the loop's
    /// channel is full, the tick is coalesced with the next periodic cycle.
    ///
    /// Exposed as `pub` only because integration tests live outside the
    /// crate; production callers should rely on the periodic cadence
    /// (`RetentionConfig::interval`). Hidden from rustdoc to discourage
    /// drive-by external use.
    #[doc(hidden)]
    pub async fn tick_now(&self) {
        let _ = self.trigger.send(None).await;
    }

    /// Trigger an immediate prune cycle and wait for it to finish before
    /// returning. Used by integration tests to avoid scheduling races —
    /// production code never needs this.
    #[doc(hidden)]
    pub async fn tick_now_and_wait(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.trigger.send(Some(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

/// Spawn the background retention loop.
///
/// The loop wakes on either the periodic `interval` tick **or** a manual
/// trigger sent via [`RetentionHandle::tick_now`] / [`tick_now_and_wait`].
/// On each wake it calls [`TraceStore::prune`] with `cutoff = now - ttl`.
///
/// When every [`RetentionHandle`] is dropped the channel closes and the
/// loop exits cleanly — the spawned task does not orphan past server
/// shutdown.
///
/// The `except_referenced` set passed to `prune` is intentionally empty:
/// ADR-0030 D6 specifies that the *store itself* tracks references via
/// `.ref` sentinel files (written by [`TraceStore::mark_referenced`]).
/// Caller-supplied runtime reference sets from Tracks B/D will be threaded
/// through when those services exist.
///
/// ## Embedding
///
/// Call this function after constructing your [`ServerState`] and hold the
/// returned [`RetentionHandle`] for the lifetime of the server so the
/// spawned task is not orphaned:
///
/// ```rust,ignore
/// let handle = spawn_retention_loop(
///     state.trace_store().unwrap(),
///     RetentionConfig::default(),
/// );
/// // keep `handle` alive alongside the server
/// ```
pub fn spawn_retention_loop(
    store: Arc<dyn TraceStore>,
    config: RetentionConfig,
) -> RetentionHandle {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<TickRequest>(8);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(config.interval);
        let referenced: HashSet<String> = HashSet::new();
        loop {
            let reply: Option<tokio::sync::oneshot::Sender<()>> = tokio::select! {
                _ = interval.tick() => None,
                msg = rx.recv() => match msg {
                    Some(reply) => reply,
                    // Channel closed — every handle dropped; exit.
                    None => break,
                },
            };
            let cutoff = SystemTime::now()
                .checked_sub(config.ttl)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if let Err(e) = store.prune(cutoff, &referenced) {
                tracing::warn!(error = %e, "TraceStore prune failed");
            }
            if let Some(reply) = reply {
                let _ = reply.send(());
            }
        }
    });
    RetentionHandle { trigger: tx }
}
