use std::sync::Arc;

use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_protocol_a2a::client::{self as a2a, Transport};
use awaken_protocol_a2a::{Task, TaskState};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{
    Error as ExecutionError, Result as ExecutionResult, verify_attempt_ownership,
};
use awaken_runtime_contract::runtime_context::{AttemptOwnershipVerifier, RuntimeRunContext};

const MAX_TASK_POLLS: usize = 600;
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Debug)]
pub(crate) enum PollError {
    Cancelled,
    Timeout,
    Client(a2a::ClientError),
    Ownership(ExecutionError),
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
    ownership: Option<Arc<dyn AttemptOwnershipVerifier>>,
    armed: bool,
}

impl CancellationOnDrop {
    fn new(
        transport: Arc<dyn Transport>,
        task_id: String,
        cancellation: Option<&CancellationToken>,
        ownership: Option<Arc<dyn AttemptOwnershipVerifier>>,
    ) -> Self {
        Self {
            transport,
            task_id,
            cancellation: cancellation.cloned(),
            ownership,
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
        let ownership = self.ownership.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            if verify_attempt_ownership(ownership.as_deref()).await.is_ok() {
                let _ = a2a::try_cancel_task(transport.as_ref(), &task_id).await;
            }
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
    ownership: Option<Arc<dyn AttemptOwnershipVerifier>>,
) -> Result<Task, PollError> {
    let mut cancellation_on_drop = CancellationOnDrop::new(
        transport.clone(),
        task.id.clone(),
        cancellation,
        ownership.clone(),
    );
    for poll in 0..=MAX_TASK_POLLS {
        if !matches!(
            task.status.state,
            TaskState::Submitted | TaskState::Working | TaskState::Unknown
        ) {
            cancellation_on_drop.disarm();
            return Ok(task);
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            if let Err(error) = verify_attempt_ownership(ownership.as_deref()).await {
                cancellation_on_drop.disarm();
                return Err(PollError::Ownership(error));
            }
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
                        if let Err(error) = verify_attempt_ownership(ownership.as_deref()).await {
                            cancellation_on_drop.disarm();
                            return Err(PollError::Ownership(error));
                        }
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
        if let Err(error) = verify_attempt_ownership(ownership.as_deref()).await {
            cancellation_on_drop.disarm();
            return Err(PollError::Ownership(error));
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

/// Map one transport boundary into the ordinary committed Run boundary. This
/// remains part of the task driver; the executor selects only the exact frozen
/// candidate and transport before entering it.
pub(crate) async fn drive_task(
    transport: Arc<dyn Transport>,
    endpoint: &str,
    activation: &RunActivation,
    context: &RuntimeRunContext,
    task: Task,
) -> ExecutionResult<RunState> {
    let task = match poll_to_boundary(
        transport.clone(),
        task,
        context.cancellation.as_ref(),
        context.ownership.clone(),
    )
    .await
    {
        Ok(task) => task,
        Err(PollError::Cancelled) => {
            return super::finish_terminal(context, activation, Vec::new(), EndCause::Cancelled)
                .await;
        }
        Err(PollError::Timeout) => {
            return Err(ExecutionError::Execution(
                "remote A2A task did not reach a boundary in time".to_string(),
            ));
        }
        Err(PollError::Client(error)) => {
            return Err(ExecutionError::Execution(error.to_string()));
        }
        Err(PollError::Ownership(error)) => return Err(error),
    };
    let reply = super::task_reply(&task);
    let messages = (!reply.is_empty())
        .then(|| super::assistant_message(context, activation, reply))
        .into_iter()
        .collect();
    match task.status.state {
        TaskState::InputRequired | TaskState::AuthRequired => {
            let reference = super::TaskReference::from_task(endpoint, &task);
            super::commit_boundary(
                context,
                activation,
                RunDisposition::awaiting(super::awaiting_ticket(activation, &task)),
                messages,
                vec![super::task_reference_state(&reference)?],
            )
            .await?;
            Ok(RunState::Awaiting)
        }
        TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected => {
            super::finish_terminal(
                context,
                activation,
                messages,
                super::end_cause_of(&task.status.state),
            )
            .await
        }
        TaskState::Submitted | TaskState::Working | TaskState::Unknown => {
            unreachable!("the shared task driver returns only a boundary")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_protocol_a2a::client::Response;
    use awaken_runtime_contract::runtime_context::AttemptOwnershipError;
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

    enum OwnershipDecision {
        Current,
        Lost,
        Unavailable,
    }

    struct FixedOwnership(OwnershipDecision);

    #[async_trait::async_trait]
    impl AttemptOwnershipVerifier for FixedOwnership {
        async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
            match self.0 {
                OwnershipDecision::Current => Ok(()),
                OwnershipDecision::Lost => Err(AttemptOwnershipError::Lost),
                OwnershipDecision::Unavailable => {
                    Err(AttemptOwnershipError::Unavailable("authority down".into()))
                }
            }
        }
    }

    #[tokio::test]
    async fn cancellation_delivery_covers_explicit_drop_and_terminal_rules() {
        // Causes: the fixtures below establish `cancellation delivery covers explicit drop and
        // terminal rules` with the concrete inputs, state, dependencies, and failure triggers used
        // by this case.
        // Constraints/invariants: the current fenced attempt and committed context are
        // authoritative; remote protocol state cannot become a parallel Run or transcript truth.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
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
                Some(&explicit_token),
                None,
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
                None,
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
            None,
        )
        .await
        .expect("A3 returns terminal task");
        assert_eq!(settled.status.state, TaskState::Completed, "A3/E3");
        terminal_token.cancel();
        tokio::task::yield_now().await;
        assert_eq!(terminal.cancellations.load(Ordering::SeqCst), 0, "A3/E4");
    }

    #[tokio::test]
    async fn cancellation_never_crosses_a_stale_attempt_boundary() {
        // Causes: the fixtures below establish `cancellation` with the concrete inputs, state,
        // dependencies, and failure triggers used by this case.
        // Constraints/invariants: the current fenced attempt and committed context are
        // authoritative; remote protocol state cannot become a parallel Run or transcript truth.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1=authority current/lost/unavailable; C2=cancel
        // is explicit or delivered by the dropped-future guard. Effects:
        // E1=one idempotent A2A cancel while current; E2=zero HTTP calls and an
        // ownership error (when observable) after authority is lost. The absent
        // compatibility rule is covered by `cancellation_delivery...`.
        //
        // | Rule | Authority   | Delivery | Effect |
        // | O1   | current     | explicit | E1     |
        // | O2   | lost/down   | explicit | E2     |
        // | O3   | lost        | drop     | E2     |
        let current = Arc::new(RecordingTransport {
            cancellations: AtomicUsize::new(0),
            cancelled: Notify::new(),
        });
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            poll_to_boundary(
                current.clone(),
                task(TaskState::Working),
                Some(&token),
                Some(Arc::new(FixedOwnership(OwnershipDecision::Current))),
            )
            .await,
            Err(PollError::Cancelled)
        ));
        assert_eq!(current.cancellations.load(Ordering::SeqCst), 1, "O1/E1");

        for decision in [OwnershipDecision::Lost, OwnershipDecision::Unavailable] {
            let stale = Arc::new(RecordingTransport {
                cancellations: AtomicUsize::new(0),
                cancelled: Notify::new(),
            });
            let token = CancellationToken::new();
            token.cancel();
            assert!(matches!(
                poll_to_boundary(
                    stale.clone(),
                    task(TaskState::Working),
                    Some(&token),
                    Some(Arc::new(FixedOwnership(decision))),
                )
                .await,
                Err(PollError::Ownership(_))
            ));
            assert_eq!(stale.cancellations.load(Ordering::SeqCst), 0, "O2/E2");
        }

        let dropped = Arc::new(RecordingTransport {
            cancellations: AtomicUsize::new(0),
            cancelled: Notify::new(),
        });
        let token = CancellationToken::new();
        let polling_token = token.clone();
        let polling_transport = dropped.clone();
        let polling = tokio::spawn(async move {
            poll_to_boundary(
                polling_transport,
                task(TaskState::Working),
                Some(&polling_token),
                Some(Arc::new(FixedOwnership(OwnershipDecision::Lost))),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        token.cancel();
        polling.abort();
        let _ = polling.await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(dropped.cancellations.load(Ordering::SeqCst), 0, "O3/E2");
    }
}
