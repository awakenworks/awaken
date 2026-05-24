use std::sync::Arc;

use awaken_contract::CancellationToken;
use awaken_contract::contract::event_sink::EventSink;

use super::{A2aBackend, ExecutionBackendError, PollCompletion, TaskSnapshot};
use awaken_protocol_a2a::TaskState;

pub(super) async fn observe_to_completion_or_cancel(
    backend: &A2aBackend,
    snapshot: TaskSnapshot,
    sink: &Arc<dyn EventSink>,
    token: Option<&CancellationToken>,
) -> Result<PollCompletion, ExecutionBackendError> {
    let Some(token) = token else {
        return backend.observe_to_completion(snapshot, sink).await;
    };

    let task_id = snapshot.task_id.clone();
    let submitted_snapshot = snapshot.clone();
    if token.is_cancelled() {
        return backend
            .cancel_task(&task_id)
            .await
            .map(|_| PollCompletion::Finished(cancelled_snapshot_from(snapshot)));
    }

    tokio::select! {
        completion = backend.observe_to_completion(snapshot, sink) => completion,
        _ = token.cancelled() => {
            backend
                .cancel_task(&task_id)
                .await
                .map(|_| PollCompletion::Finished(cancelled_snapshot_from(submitted_snapshot)))
        }
    }
}

fn cancelled_snapshot_from(mut snapshot: TaskSnapshot) -> TaskSnapshot {
    snapshot.state = TaskState::Canceled;
    snapshot.failure_message = Some("task cancelled by parent run".to_string());
    snapshot
}
