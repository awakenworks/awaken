//! Cooperative cancellation token for agent runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// A cooperative cancellation token.
///
/// Supports both synchronous polling (`is_cancelled()`) and async waiting
/// (`cancelled()`). The async path uses `tokio::select!` in streaming loops
/// to interrupt mid-stream without waiting for the next token.
#[derive(Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal cancellation. Wakes all async waiters immediately.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Check if cancellation has been requested (synchronous poll).
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Wait until cancellation is signalled. Resolves immediately if already cancelled.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_starts_not_cancelled() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_sets_flag() {
        let token = CancellationToken::new();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn clone_shares_state() {
        let token = CancellationToken::new();
        let clone = token.clone();
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let token = CancellationToken::new();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn default_creates_uncancelled_token() {
        let token = CancellationToken::default();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn multiple_clones_all_see_cancellation() {
        let token = CancellationToken::new();
        let c1 = token.clone();
        let c2 = token.clone();
        let c3 = c1.clone();
        token.cancel();
        assert!(c1.is_cancelled());
        assert!(c2.is_cancelled());
        assert!(c3.is_cancelled());
    }

    #[test]
    fn cancel_from_clone_visible_to_original() {
        let token = CancellationToken::new();
        let clone = token.clone();
        clone.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_resolves_immediately_if_already_cancelled() {
        let token = CancellationToken::new();
        token.cancel();
        token.cancelled().await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_resolves_when_cancel_called() {
        let token = CancellationToken::new();
        let clone = token.clone();
        let handle = tokio::spawn(async move {
            clone.cancelled().await;
            true
        });
        tokio::task::yield_now().await;
        token.cancel();
        let result = handle.await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn cancellation_works_with_select() {
        let token = CancellationToken::new();
        let clone = token.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            clone.cancel();
        });
        tokio::select! {
            _ = token.cancelled() => {
                assert!(token.is_cancelled());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("cancellation should have fired before timeout");
            }
        }
    }
}
