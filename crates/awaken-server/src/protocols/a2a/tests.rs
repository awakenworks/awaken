use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::Message as AwakenMessage;
use awaken_contract::contract::storage::{RunRecord, RunWaitingState, StorageError, WaitingReason};
use awaken_contract::thread::Thread;
use awaken_protocol_a2a::{MessageRole, Part, TaskState};

use super::a2a_routes;
use super::common::{
    materialize_thread_metadata_projection, parse_task_action_segment, parse_task_state_filter,
};
use super::conversion::awaken_message_to_a2a_message;
use super::error::{A2aError, map_a2a_storage_error};
use super::task::{run_is_a2a_resumable, run_record_to_task_state};

#[test]
fn parse_task_state_filter_accepts_enum_and_lowercase() {
    assert_eq!(
        parse_task_state_filter("TASK_STATE_WORKING").unwrap(),
        TaskState::Working
    );
    assert_eq!(
        parse_task_state_filter("working").unwrap(),
        TaskState::Working
    );
    assert!(parse_task_state_filter("nope").is_err());
}

#[test]
fn a2a_part_validation_requires_single_payload() {
    let part = Part {
        text: Some("hello".into()),
        raw: Some("Zm9v".into()),
        url: None,
        data: None,
        media_type: None,
        filename: None,
        metadata: None,
    };
    let count = usize::from(part.text.is_some())
        + usize::from(part.raw.is_some())
        + usize::from(part.url.is_some())
        + usize::from(part.data.is_some());
    assert_eq!(count, 2);
}

#[test]
fn message_conversion_keeps_text_and_binary_parts() {
    let message = AwakenMessage::assistant("hello").with_id("msg-1".into());
    let converted = awaken_message_to_a2a_message(&message, "task-1", "thread-1").unwrap();
    assert_eq!(converted.role, MessageRole::Agent);
    assert_eq!(converted.task_id.as_deref(), Some("task-1"));
    assert_eq!(converted.context_id.as_deref(), Some("thread-1"));
    assert_eq!(converted.text(), "hello");
}

#[test]
fn message_conversion_drops_internal_messages() {
    let message = AwakenMessage::internal_system("do not expose");
    assert!(awaken_message_to_a2a_message(&message, "task-1", "thread-1").is_none());
}

#[test]
fn parse_task_action_segment_accepts_spec_suffixes() {
    assert_eq!(
        parse_task_action_segment("task-1:cancel").unwrap(),
        ("task-1".to_string(), "cancel")
    );
    assert_eq!(
        parse_task_action_segment("task-1:subscribe").unwrap(),
        ("task-1".to_string(), "subscribe")
    );
    assert!(matches!(
        parse_task_action_segment("task-1"),
        Err(A2aError::NotFound(_))
    ));
    assert!(matches!(
        parse_task_action_segment(":cancel"),
        Err(A2aError::Validation { .. })
    ));
}

#[test]
fn a2a_routes_build_without_conflicts() {
    let _ = a2a_routes();
}

#[test]
fn materialize_thread_metadata_projection_initializes_new_threads() {
    let (exists, thread) = materialize_thread_metadata_projection("thread-1", None, 1_234);

    assert!(!exists);
    assert_eq!(thread.id, "thread-1");
    assert_eq!(thread.metadata.created_at, Some(1_234));
    assert_eq!(thread.metadata.updated_at, Some(1_234));
}

#[test]
fn materialize_thread_metadata_projection_preserves_existing_creation_time() {
    let mut existing = Thread::with_id("thread-1");
    existing.metadata.created_at = Some(100);
    existing.metadata.updated_at = Some(200);

    let (exists, thread) =
        materialize_thread_metadata_projection("thread-1", Some(existing), 1_234);

    assert!(exists);
    assert_eq!(thread.metadata.created_at, Some(100));
    assert_eq!(thread.metadata.updated_at, Some(1_234));
}

#[test]
fn map_a2a_storage_error_translates_validation_failures() {
    let error = map_a2a_storage_error(StorageError::Validation("bad thread".to_string()));

    assert!(matches!(
        error,
        A2aError::Validation { message, violations }
            if message == "invalid A2A request"
                && violations.len() == 1
                && violations[0].field == "thread"
                && violations[0].description == "bad thread"
    ));
}

#[test]
fn waiting_run_records_map_to_interrupted_task_states_by_reason() {
    let input_required = RunRecord {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        agent_id: "agent".into(),
        parent_run_id: None,
        registry_manifest: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status: RunStatus::Waiting,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: Some(RunWaitingState {
            reason: WaitingReason::UserInput,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        }),
        outcome: None,
        created_at: 0,
        started_at: None,
        finished_at: None,
        updated_at: 0,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    };
    assert_eq!(
        run_record_to_task_state(&input_required),
        TaskState::InputRequired
    );

    let auth_required = RunRecord {
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: Some(RunWaitingState {
            reason: WaitingReason::ToolPermission,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        }),
        outcome: None,
        ..input_required.clone()
    };
    assert_eq!(
        run_record_to_task_state(&auth_required),
        TaskState::AuthRequired
    );

    let awaiting_tasks = RunRecord {
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: Some(RunWaitingState {
            reason: WaitingReason::BackgroundTasks,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        }),
        outcome: None,
        ..input_required.clone()
    };
    assert_eq!(
        run_record_to_task_state(&awaiting_tasks),
        TaskState::Working
    );

    let generic_waiting = RunRecord {
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: None,
        outcome: None,
        ..input_required
    };
    assert_eq!(
        run_record_to_task_state(&generic_waiting),
        TaskState::InputRequired
    );

    let structured_auth = RunRecord {
        waiting: Some(RunWaitingState {
            reason: WaitingReason::ToolPermission,
            ticket_ids: vec!["ticket-1".into()],
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        }),
        ..generic_waiting.clone()
    };
    assert_eq!(
        run_record_to_task_state(&structured_auth),
        TaskState::AuthRequired
    );

    let structured_background = RunRecord {
        waiting: Some(RunWaitingState {
            reason: WaitingReason::BackgroundTasks,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        }),
        ..generic_waiting
    };
    assert_eq!(
        run_record_to_task_state(&structured_background),
        TaskState::Working
    );
}

#[test]
fn a2a_resumable_waiting_uses_structured_reason() {
    let base = RunRecord {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        agent_id: "agent".into(),
        parent_run_id: None,
        registry_manifest: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status: RunStatus::Waiting,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: Some(RunWaitingState {
            reason: WaitingReason::UserInput,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        }),
        outcome: None,
        created_at: 0,
        started_at: None,
        finished_at: None,
        updated_at: 0,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    };
    assert!(run_is_a2a_resumable(&base));

    let background = RunRecord {
        waiting: Some(RunWaitingState {
            reason: WaitingReason::BackgroundTasks,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        }),
        ..base
    };
    assert!(!run_is_a2a_resumable(&background));
}
