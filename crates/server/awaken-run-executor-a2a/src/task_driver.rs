use awaken_protocol_a2a::client::{self as a2a, Transport};
use awaken_protocol_a2a::{Task, TaskState};
use awaken_runtime_contract::CancellationToken;

const MAX_TASK_POLLS: usize = 600;
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) enum PollError {
    Cancelled,
    Timeout,
    Client(a2a::ClientError),
}

/// Reattach to an A2A task and drive only its transport lifecycle. Root Runs and
/// delegated Runs deliberately share this one polling/cancellation mechanism;
/// each caller maps the returned task boundary into its own domain disposition.
pub(crate) async fn poll_to_boundary(
    transport: &dyn Transport,
    mut task: Task,
    cancellation: Option<&CancellationToken>,
) -> Result<Task, PollError> {
    for poll in 0..=MAX_TASK_POLLS {
        if !matches!(
            task.status.state,
            TaskState::Submitted | TaskState::Working | TaskState::Unknown
        ) {
            return Ok(task);
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            a2a::try_cancel_task(transport, &task.id)
                .await
                .map_err(PollError::Client)?;
            return Err(PollError::Cancelled);
        }
        if poll == MAX_TASK_POLLS {
            return Err(PollError::Timeout);
        }
        match cancellation {
            Some(token) => {
                tokio::select! {
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                    _ = token.cancelled() => {
                        a2a::try_cancel_task(transport, &task.id)
                            .await
                            .map_err(PollError::Client)?;
                        return Err(PollError::Cancelled);
                    }
                }
            }
            None => tokio::time::sleep(POLL_INTERVAL).await,
        }
        task = a2a::get_task(transport, &task.id)
            .await
            .map_err(PollError::Client)?;
    }
    unreachable!("bounded A2A poll loop always returns")
}
