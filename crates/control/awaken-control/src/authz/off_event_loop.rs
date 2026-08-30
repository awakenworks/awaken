//! One scheduling boundary for synchronous IAM authentication work.

/// Execute a synchronous IAM operation on Tokio's blocking pool.
///
/// API-token verification intentionally uses Argon2. Keeping that work on an
/// async runtime worker can starve unrelated lease heartbeats, while caching a
/// successful verification would create a second revocation authority. This
/// helper changes only scheduling: every request still consults the canonical
/// IAM gate and token rows.
pub(super) async fn run<T, F>(operation: F) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(operation).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn synchronous_authentication_work_yields_the_async_runtime() {
        // Cause/effect graph: C1 synchronous credential work waits for a
        // scheduler-owned release; C2 the service runtime has only one async
        // worker. Effects: E1 the observer task runs and releases the work; E2
        // the exact operation result returns. Decision table:
        // R1(C1+C2,blocking-pool)->E1+E2; R2(C1+C2,event-loop)->the operation's
        // bounded receive expires before the observer can run and the assertion
        // fails. This is the scheduling gate; IAM contract tests separately own
        // valid, expired, revoked, and invalid credential semantics.
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let work = run(move || {
            let _ = started_tx.send(());
            let released = release_rx.recv_timeout(Duration::from_millis(500)).is_ok();
            (released, 41_u8)
        });
        let observer = async move {
            started_rx.await.expect("R1 blocking work started");
            release_tx.send(()).expect("R1 scheduler released work");
        };

        let (result, ()) = tokio::join!(work, observer);
        let (released, value) = result.expect("R1 blocking task joined");
        assert!(released, "R1/E1 async runtime remained schedulable");
        assert_eq!(value, 41, "R1/E2 exact result");
    }
}
