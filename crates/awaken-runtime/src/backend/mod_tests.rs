use super::*;

#[test]
fn backend_status_timeout_is_first_class_at_runtime_boundary() {
    let status = BackendRunStatus::Timeout;

    assert_eq!(
        status.durable_run_status(&TerminationReason::Error("polling timeout exceeded".into())),
        RunStatus::Done
    );
    assert_eq!(
        status
            .durable_status_reason(&TerminationReason::Error("polling timeout exceeded".into()))
            .as_deref(),
        Some("timeout")
    );
    assert_eq!(
        status.result_status_label(&TerminationReason::Error("polling timeout exceeded".into())),
        "timeout"
    );
}

#[test]
fn backend_status_waiting_is_first_class_at_runtime_boundary() {
    let status = BackendRunStatus::WaitingInput(Some("need details".into()));

    assert_eq!(
        status.durable_run_status(&TerminationReason::Error("should not win".into())),
        RunStatus::Waiting
    );
    assert_eq!(
        status
            .durable_status_reason(&TerminationReason::Error("should not win".into()))
            .as_deref(),
        Some("input_required")
    );
    assert_eq!(
        status.result_status_label(&TerminationReason::Error("should not win".into())),
        "waiting_input"
    );
}
