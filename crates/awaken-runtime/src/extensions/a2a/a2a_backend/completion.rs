use awaken_protocol_a2a::TaskState;
use awaken_runtime_contract::contract::lifecycle::TerminationReason;

use super::{
    BackendRunStatus, TaskSnapshot, WAIT_REASON_AUTH_REQUIRED, WAIT_REASON_INPUT_REQUIRED,
    WAIT_REASON_TIMEOUT,
};

pub(super) enum PollCompletion {
    Finished(TaskSnapshot),
    TimedOut(TaskSnapshot),
}

pub(super) struct CompletionResult {
    pub(super) snapshot: TaskSnapshot,
    pub(super) status: BackendRunStatus,
    pub(super) termination: TerminationReason,
    pub(super) status_reason: Option<String>,
}

pub(super) fn map_completion_result(
    completion: PollCompletion,
    root_run: bool,
) -> CompletionResult {
    match completion {
        PollCompletion::TimedOut(snapshot) => CompletionResult {
            snapshot,
            status: BackendRunStatus::Timeout,
            termination: TerminationReason::stopped(WAIT_REASON_TIMEOUT),
            status_reason: Some(WAIT_REASON_TIMEOUT.to_string()),
        },
        PollCompletion::Finished(snapshot) => {
            let (status, termination, status_reason) = match snapshot.state {
                TaskState::Completed => (
                    BackendRunStatus::Completed,
                    TerminationReason::NaturalEnd,
                    None,
                ),
                TaskState::Canceled => (
                    BackendRunStatus::Cancelled,
                    TerminationReason::Cancelled,
                    None,
                ),
                TaskState::Failed => {
                    let message = snapshot
                        .failure_message
                        .clone()
                        .unwrap_or_else(|| "remote agent run failed".into());
                    (
                        BackendRunStatus::Failed(message.clone()),
                        TerminationReason::Error(message),
                        None,
                    )
                }
                TaskState::Rejected => {
                    let message = snapshot
                        .failure_message
                        .clone()
                        .unwrap_or_else(|| "remote agent rejected the task".into());
                    (
                        BackendRunStatus::Failed(message.clone()),
                        TerminationReason::Error(message),
                        None,
                    )
                }
                TaskState::InputRequired => {
                    interrupted_completion(snapshot.failure_message.clone(), root_run, false)
                }
                TaskState::AuthRequired => {
                    interrupted_completion(snapshot.failure_message.clone(), root_run, true)
                }
                TaskState::Submitted | TaskState::Working => (
                    BackendRunStatus::Failed("remote agent did not reach a terminal state".into()),
                    TerminationReason::Error("remote agent did not reach a terminal state".into()),
                    None,
                ),
            };
            CompletionResult {
                snapshot,
                status,
                termination,
                status_reason,
            }
        }
    }
}

fn interrupted_completion(
    failure_message: Option<String>,
    root_run: bool,
    auth_required: bool,
) -> (BackendRunStatus, TerminationReason, Option<String>) {
    let (default_message, wait_reason) = if auth_required {
        (
            "remote agent requires authentication",
            WAIT_REASON_AUTH_REQUIRED,
        )
    } else {
        (
            "remote agent requires additional input",
            WAIT_REASON_INPUT_REQUIRED,
        )
    };

    let message = if root_run {
        failure_message
    } else {
        Some(failure_message.unwrap_or_else(|| default_message.into()))
    };
    (
        if auth_required {
            BackendRunStatus::WaitingAuth(message)
        } else {
            BackendRunStatus::WaitingInput(message)
        },
        TerminationReason::Suspended,
        Some(wait_reason.to_string()),
    )
}
