use crate::contracts::storage::VersionPrecondition;
use crate::contracts::thread::ThreadChangeSet;
use async_trait::async_trait;
use futures::future::pending;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub type RunCancellationToken = CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelAware<T> {
    Value(T),
    Cancelled,
}

pub fn is_cancelled(token: Option<&RunCancellationToken>) -> bool {
    token.is_some_and(RunCancellationToken::is_cancelled)
}

pub async fn cancelled(token: Option<&RunCancellationToken>) {
    if let Some(token) = token {
        token.cancelled().await;
    } else {
        pending::<()>().await;
    }
}

pub async fn await_or_cancel<T, F>(token: Option<&RunCancellationToken>, fut: F) -> CancelAware<T>
where
    F: std::future::Future<Output = T>,
{
    if let Some(token) = token {
        tokio::select! {
            _ = token.cancelled() => CancelAware::Cancelled,
            value = fut => CancelAware::Value(value),
        }
    } else {
        CancelAware::Value(fut.await)
    }
}

/// Error returned by state commit sinks.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct StateCommitError {
    pub message: String,
}

impl StateCommitError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Sink for committed thread deltas.
#[async_trait]
pub trait StateCommitter: Send + Sync {
    /// Commit a single change set for a thread.
    ///
    /// Returns the committed storage version after the write succeeds.
    async fn commit(
        &self,
        thread_id: &str,
        changeset: ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<u64, StateCommitError>;
}

impl std::fmt::Debug for dyn StateCommitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<StateCommitter>")
    }
}

/// Scope key: caller session id visible to tools.
pub const TOOL_SCOPE_CALLER_THREAD_ID_KEY: &str = "__agent_tool_caller_thread_id";
/// Scope key: caller agent id visible to tools.
pub const TOOL_SCOPE_CALLER_AGENT_ID_KEY: &str = "__agent_tool_caller_agent_id";
/// Scope key: caller state snapshot visible to tools.
pub const TOOL_SCOPE_CALLER_STATE_KEY: &str = "__agent_tool_caller_state";
/// Scope key: caller message snapshot visible to tools.
pub const TOOL_SCOPE_CALLER_MESSAGES_KEY: &str = "__agent_tool_caller_messages";

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn await_or_cancel_returns_value_without_token() {
        let out = await_or_cancel(None, async { 42usize }).await;
        assert_eq!(out, CancelAware::Value(42));
    }

    #[tokio::test]
    async fn await_or_cancel_returns_cancelled_when_token_cancelled() {
        let token = RunCancellationToken::new();
        let token_for_task = token.clone();
        let handle = tokio::spawn(async move {
            await_or_cancel(Some(&token_for_task), async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                7usize
            })
            .await
        });

        token.cancel();
        let out = timeout(Duration::from_millis(300), handle)
            .await
            .expect("await_or_cancel should resolve quickly after cancellation")
            .expect("task should not panic");
        assert_eq!(out, CancelAware::Cancelled);
    }

    #[tokio::test]
    async fn cancelled_waits_for_token_signal() {
        let token = RunCancellationToken::new();
        let token_for_task = token.clone();
        let handle = tokio::spawn(async move {
            cancelled(Some(&token_for_task)).await;
            true
        });

        token.cancel();
        let done = timeout(Duration::from_millis(300), handle)
            .await
            .expect("cancelled() should return after token cancellation")
            .expect("task should not panic");
        assert!(done);
    }
}
