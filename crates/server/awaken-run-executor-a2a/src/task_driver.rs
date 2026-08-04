use std::sync::Arc;

use awaken_protocol_a2a::client::{self as a2a, Transport};
use awaken_protocol_a2a::{Task, TaskState};
use awaken_runtime_contract::CancellationToken;

const MAX_TASK_POLLS: usize = 600;
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Debug)]
pub(crate) enum PollError {
    Cancelled,
    Timeout,
    Client(a2a::ClientError),
}

/// Completes the transport-side half of cooperative cancellation when an owner
/// drops the polling future immediately after cancelling its token. Runtime tool
/// cancellation intentionally drops in-flight futures; without this guard that
/// correct local-process behavior could strand a remote A2A task before this
/// driver had a chance to observe the same token.
struct CancellationOnDrop {
    transport: Arc<dyn Transport>,
    task_id: String,
    cancellation: Option<CancellationToken>,
    armed: bool,
}

impl CancellationOnDrop {
    fn new(
        transport: Arc<dyn Transport>,
        task_id: String,
        cancellation: Option<&CancellationToken>,
    ) -> Self {
        Self {
            transport,
            task_id,
            cancellation: cancellation.cloned(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancellationOnDrop {
    fn drop(&mut self) {
        if !self.armed
            || !self
                .cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            return;
        }
        let transport = self.transport.clone();
        let task_id = self.task_id.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let _ = a2a::try_cancel_task(transport.as_ref(), &task_id).await;
        });
    }
}

/// Reattach to an A2A task and drive only its transport lifecycle. Root Runs and
/// delegated Runs deliberately share this one polling/cancellation mechanism;
/// each caller maps the returned task boundary into its own domain disposition.
pub(crate) async fn poll_to_boundary(
    transport: Arc<dyn Transport>,
    mut task: Task,
    cancellation: Option<&CancellationToken>,
) -> Result<Task, PollError> {
    let mut cancellation_on_drop =
        CancellationOnDrop::new(transport.clone(), task.id.clone(), cancellation);
    for poll in 0..=MAX_TASK_POLLS {
        if !matches!(
            task.status.state,
            TaskState::Submitted | TaskState::Working | TaskState::Unknown
        ) {
            cancellation_on_drop.disarm();
            return Ok(task);
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            let result = a2a::try_cancel_task(transport.as_ref(), &task.id)
                .await
                .map_err(PollError::Client);
            cancellation_on_drop.disarm();
            result?;
            return Err(PollError::Cancelled);
        }
        if poll == MAX_TASK_POLLS {
            cancellation_on_drop.disarm();
            return Err(PollError::Timeout);
        }
        match cancellation {
            Some(token) => {
                tokio::select! {
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                    _ = token.cancelled() => {
                        let result = a2a::try_cancel_task(transport.as_ref(), &task.id)
                            .await
                            .map_err(PollError::Client);
                        cancellation_on_drop.disarm();
                        result?;
                        return Err(PollError::Cancelled);
                    }
                }
            }
            None => tokio::time::sleep(POLL_INTERVAL).await,
        }
        task = match a2a::get_task(transport.as_ref(), &task.id).await {
            Ok(task) => task,
            Err(error) => {
                cancellation_on_drop.disarm();
                return Err(PollError::Client(error));
            }
        };
    }
    unreachable!("bounded A2A poll loop always returns")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_protocol_a2a::client::Response;
    use tokio::sync::Notify;

    use super::*;

    struct RecordingTransport {
        cancellations: AtomicUsize,
        cancelled: Notify,
    }

    #[async_trait::async_trait]
    impl Transport for RecordingTransport {
        async fn request(
            &self,
            method: &str,
            path: &str,
            _body: Option<Vec<u8>>,
        ) -> Result<Response, String> {
            if method == "POST" && path.ends_with(":cancel") {
                self.cancellations.fetch_add(1, Ordering::SeqCst);
                self.cancelled.notify_one();
                return Ok(Response::new(204, Vec::new()));
            }
            Err(format!("unexpected request {method} {path}"))
        }
    }

    fn task(state: TaskState) -> Task {
        serde_json::from_value(serde_json::json!({
            "kind": "task",
            "id": "remote-task",
            "contextId": "remote-context",
            "status": { "state": state }
        }))
        .expect("valid task fixture")
    }

    #[tokio::test]
    async fn cancellation_delivery_covers_explicit_drop_and_terminal_rules() {
        // Cause/effect graph: C1=task remains pollable; C2=token is cancelled;
        // C3=poll future is retained long enough to observe C2; C4=task already
        // reached a terminal boundary. Effects: E1=send one idempotent remote
        // cancel, E2=return Cancelled, E3=return the terminal task, E4=never
        // cancel a terminal task.
        //
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | A1   | Y  | Y  | Y  | N  | E1 + E2 |
        // | A2   | Y  | Y  | N  | N  | E1 from the drop guard |
        // | A3   | N  | N  | -  | Y  | E3 + E4 |
        let explicit = Arc::new(RecordingTransport {
            cancellations: AtomicUsize::new(0),
            cancelled: Notify::new(),
        });
        let explicit_token = CancellationToken::new();
        explicit_token.cancel();
        assert!(matches!(
            poll_to_boundary(
                explicit.clone(),
                task(TaskState::Working),
                Some(&explicit_token)
            )
            .await,
            Err(PollError::Cancelled)
        ));
        assert_eq!(explicit.cancellations.load(Ordering::SeqCst), 1, "A1/E1");

        let dropped = Arc::new(RecordingTransport {
            cancellations: AtomicUsize::new(0),
            cancelled: Notify::new(),
        });
        let dropped_token = CancellationToken::new();
        let polling_token = dropped_token.clone();
        let polling_transport = dropped.clone();
        let polling = tokio::spawn(async move {
            poll_to_boundary(
                polling_transport,
                task(TaskState::Working),
                Some(&polling_token),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        dropped_token.cancel();
        polling.abort();
        let _ = polling.await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            dropped.cancelled.notified(),
        )
        .await
        .expect("A2 drop guard delivers cancellation");
        assert_eq!(dropped.cancellations.load(Ordering::SeqCst), 1, "A2/E1");

        let terminal = Arc::new(RecordingTransport {
            cancellations: AtomicUsize::new(0),
            cancelled: Notify::new(),
        });
        let terminal_token = CancellationToken::new();
        let settled = poll_to_boundary(
            terminal.clone(),
            task(TaskState::Completed),
            Some(&terminal_token),
        )
        .await
        .expect("A3 returns terminal task");
        assert_eq!(settled.status.state, TaskState::Completed, "A3/E3");
        terminal_token.cancel();
        tokio::task::yield_now().await;
        assert_eq!(terminal.cancellations.load(Ordering::SeqCst), 0, "A3/E4");
    }
}
