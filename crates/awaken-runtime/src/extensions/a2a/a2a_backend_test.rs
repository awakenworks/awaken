use super::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

use awaken_contract::CancellationToken;
use awaken_contract::contract::event_sink::NullEventSink;
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::storage::{RunRecord, ThreadRunStore};
use awaken_stores::memory::InMemoryStore;
use serde_json::json;

struct NoopResolver;

impl crate::registry::AgentResolver for NoopResolver {
    fn resolve(
        &self,
        agent_id: &str,
    ) -> Result<crate::registry::ResolvedAgent, crate::RuntimeError> {
        Err(crate::RuntimeError::AgentNotFound {
            agent_id: agent_id.to_string(),
        })
    }
}

impl crate::registry::ExecutionResolver for NoopResolver {
    fn resolve_execution(
        &self,
        agent_id: &str,
    ) -> Result<crate::registry::ResolvedExecution, crate::RuntimeError> {
        Err(crate::RuntimeError::AgentNotFound {
            agent_id: agent_id.to_string(),
        })
    }
}

fn make_task(state: TaskState) -> Task {
    Task {
        id: "task-1".into(),
        context_id: "ctx-1".into(),
        status: awaken_protocol_a2a::TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        artifacts: vec![],
        history: vec![],
        metadata: None,
    }
}

#[test]
fn extract_output_prefers_artifacts() {
    let task = Task {
        artifacts: vec![awaken_protocol_a2a::Artifact {
            artifact_id: "response".into(),
            name: None,
            description: None,
            parts: vec![Part::text("hello"), Part::text(" world")],
            metadata: None,
        }],
        ..make_task(TaskState::Completed)
    };
    assert_eq!(
        extract_output_text(&task).as_deref(),
        Some("hello\n\n world")
    );
    let snapshot = TaskSnapshot::from_task(task);
    assert_eq!(snapshot.output.text.as_deref(), Some("hello\n\n world"));
    assert_eq!(snapshot.output.artifacts.len(), 1);
    assert_eq!(snapshot.output.artifacts[0].id.as_deref(), Some("response"));
}

#[test]
fn extract_output_falls_back_to_status_message_then_history() {
    let status_message = A2aMessage {
        task_id: Some("task-1".into()),
        context_id: Some("ctx-1".into()),
        message_id: "msg-1".into(),
        role: MessageRole::Agent,
        parts: vec![Part::text("status output")],
        metadata: None,
    };
    let task = Task {
        status: awaken_protocol_a2a::TaskStatus {
            state: TaskState::Completed,
            message: Some(status_message.clone()),
            timestamp: None,
        },
        history: vec![A2aMessage {
            task_id: Some("task-1".into()),
            context_id: Some("ctx-1".into()),
            message_id: "msg-2".into(),
            role: MessageRole::Agent,
            parts: vec![Part::text("history output")],
            metadata: None,
        }],
        ..make_task(TaskState::Completed)
    };
    assert_eq!(extract_output_text(&task).as_deref(), Some("status output"));
}

#[test]
fn task_snapshot_maps_failure_states() {
    let task = Task {
        status: awaken_protocol_a2a::TaskStatus {
            state: TaskState::Rejected,
            message: Some(A2aMessage {
                task_id: Some("task-1".into()),
                context_id: Some("ctx-1".into()),
                message_id: "msg-1".into(),
                role: MessageRole::Agent,
                parts: vec![Part::text("policy rejected")],
                metadata: None,
            }),
            timestamp: None,
        },
        ..make_task(TaskState::Rejected)
    };
    let snapshot = TaskSnapshot::from_task(task);
    assert_eq!(snapshot.state, TaskState::Rejected);
    assert_eq!(snapshot.failure_message.as_deref(), Some("policy rejected"));
}

#[test]
fn submitted_task_requires_follow_up_polling() {
    let snapshot = TaskSnapshot::from_task(make_task(TaskState::Submitted));
    assert!(!snapshot.is_done());
}

#[test]
fn send_message_response_requires_task_or_message() {
    let err = SubmissionOutcome::from_response(SendMessageResponse::default()).unwrap_err();
    assert!(err.to_string().contains("task or message"));
}

#[test]
fn send_message_response_preserves_direct_message_path() {
    let outcome = SubmissionOutcome::from_response(SendMessageResponse {
        message: Some(A2aMessage {
            task_id: None,
            context_id: None,
            message_id: "msg-1".into(),
            role: MessageRole::Agent,
            parts: vec![Part::text("hello")],
            metadata: None,
        }),
        ..Default::default()
    })
    .unwrap();

    let SubmissionOutcome::DirectMessage(snapshot) = outcome else {
        panic!("expected direct message outcome");
    };
    assert_eq!(snapshot.output.text.as_deref(), Some("hello"));
}

#[test]
fn a2a_config_builder() {
    let config = A2aConfig::new("https://api.example.com/v1/a2a")
        .with_bearer_token("tok_123")
        .with_target_agent_id("worker")
        .with_poll_interval(Duration::from_millis(5000))
        .with_timeout(Duration::from_secs(60))
        .with_history_length(4)
        .with_return_immediately(false);

    assert_eq!(config.base_url, "https://api.example.com/v1/a2a");
    assert_eq!(config.bearer_token.as_deref(), Some("tok_123"));
    assert_eq!(config.target_agent_id.as_deref(), Some("worker"));
    assert_eq!(config.poll_interval, Duration::from_millis(5000));
    assert_eq!(config.timeout, Duration::from_secs(60));
    assert_eq!(config.history_length, Some(4));
    assert!(!config.return_immediately);
}

#[test]
fn a2a_config_try_from_remote_endpoint_reads_canonical_fields() {
    let mut options = BTreeMap::new();
    options.insert(POLL_INTERVAL_OPTION_KEY.into(), json!(1500));
    options.insert(HISTORY_LENGTH_OPTION_KEY.into(), json!(3));
    options.insert(RETURN_IMMEDIATELY_OPTION_KEY.into(), json!(false));
    let endpoint = RemoteEndpoint {
        backend: "a2a".into(),
        base_url: "https://api.example.com/v1/a2a".into(),
        auth: Some(awaken_contract::registry_spec::RemoteAuth::bearer(
            "tok_123",
        )),
        target: Some("worker".into()),
        timeout_ms: 60_000,
        options,
    };

    let config = A2aConfig::try_from_remote_endpoint(&endpoint).unwrap();
    assert_eq!(config.base_url, "https://api.example.com/v1/a2a");
    assert_eq!(config.bearer_token.as_deref(), Some("tok_123"));
    assert_eq!(config.target_agent_id.as_deref(), Some("worker"));
    assert_eq!(config.poll_interval, Duration::from_millis(1500));
    assert_eq!(config.timeout, Duration::from_secs(60));
    assert_eq!(config.history_length, Some(3));
    assert!(!config.return_immediately);
}

#[test]
fn a2a_config_try_from_remote_endpoint_rejects_non_bearer_auth() {
    let endpoint = RemoteEndpoint {
        backend: "a2a".into(),
        base_url: "https://api.example.com/v1/a2a".into(),
        auth: Some(awaken_contract::registry_spec::RemoteAuth {
            auth_type: "basic".into(),
            params: BTreeMap::new(),
        }),
        ..Default::default()
    };

    let err = A2aConfig::try_from_remote_endpoint(&endpoint).unwrap_err();
    assert!(err.to_string().contains("only supports bearer auth"));
}

#[test]
fn a2a_backend_factory_builds_backend_for_a2a_endpoint() {
    let backend = A2aBackendFactory
        .build(&RemoteEndpoint {
            backend: "a2a".into(),
            base_url: "https://api.example.com/v1/a2a".into(),
            ..Default::default()
        })
        .unwrap();

    let _backend: Arc<dyn crate::backend::ExecutionBackend> = backend;
}

#[test]
fn a2a_backend_factory_validates_endpoint_config_without_building() {
    A2aBackendFactory
        .validate(&RemoteEndpoint {
            backend: "a2a".into(),
            base_url: "https://api.example.com/v1/a2a".into(),
            ..Default::default()
        })
        .unwrap();

    let err = A2aBackendFactory
        .validate(&RemoteEndpoint {
            backend: "a2a".into(),
            base_url: "https://api.example.com/v1/a2a".into(),
            auth: Some(awaken_contract::registry_spec::RemoteAuth {
                auth_type: "basic".into(),
                params: BTreeMap::new(),
            }),
            ..Default::default()
        })
        .unwrap_err();
    assert!(err.to_string().contains("only supports bearer auth"));
}

#[test]
fn timed_out_poll_completion_maps_to_timeout_status() {
    let timed_out_snapshot = TaskSnapshot {
        task_id: "task-1".into(),
        context_id: Some("ctx-1".into()),
        state: TaskState::Working,
        output_text: Some("partial output".into()),
        output: BackendRunOutput::from_text(Some("partial output".into())),
        failure_message: Some("polling timeout exceeded".into()),
    };

    let result = map_completion_result(PollCompletion::TimedOut(timed_out_snapshot.clone()), true);

    assert!(matches!(result.status, BackendRunStatus::Timeout));
    assert_eq!(result.snapshot.output_text, timed_out_snapshot.output_text);
    assert!(matches!(
        result.termination,
        TerminationReason::Stopped(ref reason) if reason.code == WAIT_REASON_TIMEOUT
    ));
    assert_eq!(result.status_reason.as_deref(), Some(WAIT_REASON_TIMEOUT));
}

#[test]
fn interrupted_root_poll_completion_maps_to_suspended_waiting_reason() {
    let input_required = TaskSnapshot {
        task_id: "task-1".into(),
        context_id: Some("ctx-1".into()),
        state: TaskState::InputRequired,
        output_text: Some("Need more details".into()),
        output: BackendRunOutput::from_text(Some("Need more details".into())),
        failure_message: Some("Need more details".into()),
    };
    let auth_required = TaskSnapshot {
        task_id: "task-2".into(),
        context_id: Some("ctx-2".into()),
        state: TaskState::AuthRequired,
        output_text: Some("Sign in first".into()),
        output: BackendRunOutput::from_text(Some("Sign in first".into())),
        failure_message: Some("Sign in first".into()),
    };

    let input_result = map_completion_result(PollCompletion::Finished(input_required), true);
    assert!(matches!(
        input_result.status,
        BackendRunStatus::WaitingInput(Some(ref message)) if message == "Need more details"
    ));
    assert_eq!(input_result.termination, TerminationReason::Suspended);
    assert_eq!(
        input_result.status_reason.as_deref(),
        Some(WAIT_REASON_INPUT_REQUIRED)
    );

    let auth_result = map_completion_result(PollCompletion::Finished(auth_required), true);
    assert!(matches!(
        auth_result.status,
        BackendRunStatus::WaitingAuth(Some(ref message)) if message == "Sign in first"
    ));
    assert_eq!(auth_result.termination, TerminationReason::Suspended);
    assert_eq!(
        auth_result.status_reason.as_deref(),
        Some(WAIT_REASON_AUTH_REQUIRED)
    );
}

#[test]
fn interrupted_delegate_poll_completion_maps_to_suspended_waiting_reason() {
    let snapshot = TaskSnapshot {
        task_id: "task-1".into(),
        context_id: Some("ctx-1".into()),
        state: TaskState::InputRequired,
        output_text: None,
        output: BackendRunOutput::default(),
        failure_message: Some("Need more details".into()),
    };

    let result = map_completion_result(PollCompletion::Finished(snapshot), false);
    assert!(matches!(
        result.status,
        BackendRunStatus::WaitingInput(Some(ref message)) if message == "Need more details"
    ));
    assert_eq!(result.termination, TerminationReason::Suspended);
    assert_eq!(
        result.status_reason.as_deref(),
        Some(WAIT_REASON_INPUT_REQUIRED)
    );
}

#[test]
fn direct_message_snapshot_preserves_artifacts() {
    let snapshot = DirectMessageSnapshot::from_message(A2aMessage {
        task_id: Some("task-direct".into()),
        context_id: Some("ctx-direct".into()),
        message_id: "msg-direct".into(),
        role: MessageRole::Agent,
        parts: vec![
            Part::text("summary"),
            Part {
                text: None,
                raw: None,
                url: None,
                data: Some(json!({"answer": 42})),
                media_type: Some("application/json".into()),
                filename: Some("answer.json".into()),
                metadata: None,
            },
        ],
        metadata: None,
    });

    assert_eq!(
        snapshot.output.text.as_deref(),
        Some("summary\n\n{\"answer\":42}")
    );
    assert_eq!(snapshot.output.artifacts.len(), 1);
    assert_eq!(
        snapshot.output.artifacts[0].id.as_deref(),
        Some("msg-direct:1")
    );
    assert_eq!(
        snapshot.output.artifacts[0].media_type.as_deref(),
        Some("application/json")
    );
    assert_eq!(
        snapshot.output.artifacts[0].content["data"],
        json!({"answer": 42})
    );
}

#[test]
fn extract_text_from_parts_supports_structured_data() {
    let parts = vec![Part {
        text: None,
        raw: None,
        url: None,
        data: Some(json!({"ok": true})),
        media_type: Some("application/json".into()),
        filename: None,
        metadata: None,
    }];
    assert_eq!(
        extract_text_from_parts(&parts).as_deref(),
        Some("{\"ok\":true}")
    );
}

#[test]
fn update_persisted_state_roundtrips_remote_task_binding() {
    let persisted = update_persisted_state(
        None,
        "a2a:https://gateway.example.com/v1/a2a/worker",
        &TaskSnapshot {
            task_id: "task-1".into(),
            context_id: Some("ctx-1".into()),
            state: TaskState::Completed,
            output_text: Some("done".into()),
            output: BackendRunOutput::from_text(Some("done".into())),
            failure_message: None,
        },
    )
    .expect("state should serialize");

    let remote =
        read_remote_state_entry(&persisted, "a2a:https://gateway.example.com/v1/a2a/worker")
            .expect("remote state entry");
    assert_eq!(remote.task_id.as_deref(), Some("task-1"));
    assert_eq!(remote.context_id.as_deref(), Some("ctx-1"));
    assert_eq!(remote.last_state.as_deref(), Some("TASK_STATE_COMPLETED"));
    assert_eq!(remote.version, REMOTE_STATE_SCHEMA_VERSION);
    assert!(remote.updated_at_ms.is_some());
}

#[test]
fn completed_remote_task_is_not_reused_for_next_turn() {
    let state = PersistedA2aThreadState {
        task_id: Some("completed-task".into()),
        context_id: Some("ctx-1".into()),
        last_state: Some("TASK_STATE_COMPLETED".into()),
        ..Default::default()
    };

    assert_eq!(reusable_prior_task_id(&state), None);
}

#[test]
fn interrupted_remote_task_is_reused_for_resume_turn() {
    let state = PersistedA2aThreadState {
        task_id: Some("waiting-task".into()),
        context_id: Some("ctx-1".into()),
        last_state: Some("TASK_STATE_INPUT_REQUIRED".into()),
        ..Default::default()
    };

    assert_eq!(
        reusable_prior_task_id(&state).as_deref(),
        Some("waiting-task")
    );
}

#[test]
fn state_without_last_state_never_reuses_task() {
    let state = PersistedA2aThreadState {
        task_id: Some("unknown-task".into()),
        context_id: Some("ctx-1".into()),
        last_state: None,
        ..Default::default()
    };

    assert_eq!(reusable_prior_task_id(&state), None);
}

#[test]
fn abort_task_id_falls_back_to_persisted_interrupted_state() {
    let target_key = "a2a:https://gateway.example.com/v1/a2a/worker";
    let persisted = update_persisted_state(
        None,
        target_key,
        &TaskSnapshot {
            task_id: "waiting-task".into(),
            context_id: Some("ctx-1".into()),
            state: TaskState::InputRequired,
            output_text: None,
            output: BackendRunOutput::default(),
            failure_message: None,
        },
    )
    .expect("persisted remote state");
    let run_identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "remote-agent".into(),
        RunOrigin::User,
    );
    let request = BackendAbortRequest {
        agent_id: "remote-agent",
        run_identity: &run_identity,
        parent: None,
        persisted_state: Some(&persisted),
        is_continuation: false,
    };

    assert_eq!(
        persisted_abort_task_id(&request, target_key).as_deref(),
        Some("waiting-task")
    );
}

#[test]
fn abort_task_id_does_not_reuse_completed_prior_state() {
    let target_key = "a2a:https://gateway.example.com/v1/a2a/worker";
    let persisted = update_persisted_state(
        None,
        target_key,
        &TaskSnapshot {
            task_id: "completed-task".into(),
            context_id: Some("ctx-1".into()),
            state: TaskState::Completed,
            output_text: None,
            output: BackendRunOutput::default(),
            failure_message: None,
        },
    )
    .expect("persisted remote state");
    let run_identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "remote-agent".into(),
        RunOrigin::User,
    );
    let request = BackendAbortRequest {
        agent_id: "remote-agent",
        run_identity: &run_identity,
        parent: None,
        persisted_state: Some(&persisted),
        is_continuation: false,
    };

    assert_eq!(persisted_abort_task_id(&request, target_key), None);
}

#[test]
fn update_persisted_state_from_direct_message_records_remote_ids() {
    let persisted = update_persisted_state_from_direct(
        None,
        "a2a:https://gateway.example.com/v1/a2a/worker",
        &DirectMessageSnapshot {
            task_id: Some("task-direct".into()),
            context_id: Some("ctx-direct".into()),
            output: BackendRunOutput::from_text(Some("done".into())),
        },
    )
    .expect("direct message state should serialize");

    let remote =
        read_remote_state_entry(&persisted, "a2a:https://gateway.example.com/v1/a2a/worker")
            .expect("remote state entry");
    assert_eq!(remote.task_id.as_deref(), Some("task-direct"));
    assert_eq!(remote.context_id.as_deref(), Some("ctx-direct"));
    assert_eq!(remote.last_state.as_deref(), Some("DIRECT_MESSAGE"));
}

#[test]
fn update_persisted_state_from_direct_message_without_ids_keeps_state() {
    let original = PersistedState {
        revision: 7,
        extensions: HashMap::new(),
    };

    let persisted = update_persisted_state_from_direct(
        Some(original.clone()),
        "a2a:https://gateway.example.com/v1/a2a/worker",
        &DirectMessageSnapshot {
            task_id: None,
            context_id: None,
            output: BackendRunOutput::from_text(Some("done".into())),
        },
    )
    .expect("state should pass through");

    assert_eq!(persisted, original);
}

#[tokio::test]
async fn continuation_loads_state_from_continue_run_id_not_latest_thread_run() {
    let backend = A2aBackend::new(
        A2aConfig::new("https://gateway.example.com/v1/a2a").with_target_agent_id("worker"),
    );
    let target_key = backend.remote_target_key();
    let continued_state = update_persisted_state(
        None,
        &target_key,
        &TaskSnapshot {
            task_id: "continued-task".into(),
            context_id: Some("continued-context".into()),
            state: TaskState::InputRequired,
            output_text: None,
            output: BackendRunOutput::default(),
            failure_message: None,
        },
    )
    .expect("continued state");
    let newer_state = update_persisted_state(
        None,
        &target_key,
        &TaskSnapshot {
            task_id: "newer-task".into(),
            context_id: Some("newer-context".into()),
            state: TaskState::Completed,
            output_text: None,
            output: BackendRunOutput::default(),
            failure_message: None,
        },
    )
    .expect("newer state");

    let store = InMemoryStore::new();
    store
        .checkpoint(
            "thread-1",
            &[Message::user("old turn")],
            &RunRecord {
                run_id: "continued-run".into(),
                thread_id: "thread-1".into(),
                agent_id: "remote-agent".into(),
                parent_run_id: None,
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
                waiting: None,
                outcome: None,
                created_at: 1,
                started_at: None,
                finished_at: None,
                updated_at: 1,
                steps: 1,
                input_tokens: 0,
                output_tokens: 0,
                state: Some(continued_state),
            },
        )
        .await
        .expect("checkpoint continued run");
    store
        .checkpoint(
            "thread-1",
            &[Message::user("newer turn")],
            &RunRecord {
                run_id: "newer-run".into(),
                thread_id: "thread-1".into(),
                agent_id: "remote-agent".into(),
                parent_run_id: None,
                request: None,
                input: None,
                output: None,
                status: RunStatus::Done,
                termination_reason: None,
                final_output: None,
                error_payload: None,
                dispatch_id: None,
                session_id: None,
                transport_request_id: None,
                waiting: None,
                outcome: None,
                created_at: 2,
                started_at: None,
                finished_at: None,
                updated_at: 2,
                steps: 1,
                input_tokens: 0,
                output_tokens: 0,
                state: Some(newer_state),
            },
        )
        .await
        .expect("checkpoint newer run");

    let resolver = NoopResolver;
    let request = BackendRootRunRequest {
        agent_id: "remote-agent",
        messages: vec![Message::user("resume")],
        new_messages: vec![Message::user("resume")],
        sink: Arc::new(NullEventSink),
        resolver: &resolver,
        run_identity: RunIdentity::new(
            "thread-1".into(),
            None,
            "continued-run".into(),
            None,
            "remote-agent".into(),
            RunOrigin::User,
        ),
        checkpoint_store: Some(&store),
        control: crate::backend::BackendControl::default(),
        decisions: Vec::new(),
        overrides: None,
        frontend_tools: Vec::new(),
        local: None,
        inbox: None,
        is_continuation: true,
    };

    let state = backend
        .load_persisted_state(&A2aExecutionRequest::Root(Box::new(request)))
        .await
        .expect("load state")
        .expect("state");
    let remote = read_remote_state_entry(&state, &target_key).expect("remote state");
    assert_eq!(remote.task_id.as_deref(), Some("continued-task"));
    assert_eq!(remote.context_id.as_deref(), Some("continued-context"));
}

#[test]
fn sse_decoder_collects_json_payloads() {
    let mut decoder = SseDataDecoder::default();
    let events = decoder.push(
        "data: {\"task\":{\"id\":\"task-1\"}}\n\
         \n\
         data: {\"statusUpdate\":{\"taskId\":\"task-1\"}}\n\
         \n",
    );
    assert_eq!(
        events,
        vec![
            "{\"task\":{\"id\":\"task-1\"}}".to_string(),
            "{\"statusUpdate\":{\"taskId\":\"task-1\"}}".to_string()
        ]
    );
}

#[test]
fn stream_status_update_preserves_terminal_message() {
    let mut snapshot = TaskSnapshot::from_task(make_task(TaskState::Working));
    snapshot.apply_stream_response(StreamResponse {
        status_update: Some(TaskStatusUpdateEvent {
            task_id: "task-1".into(),
            context_id: "ctx-1".into(),
            status: awaken_protocol_a2a::TaskStatus {
                state: TaskState::InputRequired,
                message: Some(A2aMessage {
                    task_id: Some("task-1".into()),
                    context_id: Some("ctx-1".into()),
                    message_id: "msg-1".into(),
                    role: MessageRole::Agent,
                    parts: vec![Part::text("Need more details")],
                    metadata: None,
                }),
                timestamp: None,
            },
            metadata: None,
        }),
        ..Default::default()
    });

    assert_eq!(snapshot.state, TaskState::InputRequired);
    assert_eq!(
        snapshot.failure_message.as_deref(),
        Some("Need more details")
    );
}

#[test]
fn stream_artifact_append_accumulates_output_text() {
    let mut snapshot = TaskSnapshot::from_task(make_task(TaskState::Working));
    snapshot.apply_stream_response(StreamResponse {
        artifact_update: Some(TaskArtifactUpdateEvent {
            task_id: "task-1".into(),
            context_id: "ctx-1".into(),
            artifact: awaken_protocol_a2a::Artifact {
                artifact_id: "response".into(),
                name: None,
                description: None,
                parts: vec![Part::text("hello")],
                metadata: None,
            },
            append: Some(false),
            last_chunk: Some(false),
            metadata: None,
        }),
        ..Default::default()
    });
    snapshot.apply_stream_response(StreamResponse {
        artifact_update: Some(TaskArtifactUpdateEvent {
            task_id: "task-1".into(),
            context_id: "ctx-1".into(),
            artifact: awaken_protocol_a2a::Artifact {
                artifact_id: "response".into(),
                name: None,
                description: None,
                parts: vec![Part::text("world")],
                metadata: None,
            },
            append: Some(true),
            last_chunk: Some(true),
            metadata: None,
        }),
        ..Default::default()
    });

    assert_eq!(snapshot.output_text.as_deref(), Some("hello\n\nworld"));
}

#[test]
fn task_progress_content_preserves_state_text_and_artifacts() {
    let mut snapshot = TaskSnapshot::from_task(make_task(TaskState::Working));
    snapshot.apply_stream_response(StreamResponse {
        artifact_update: Some(TaskArtifactUpdateEvent {
            task_id: "task-1".into(),
            context_id: "ctx-1".into(),
            artifact: awaken_protocol_a2a::Artifact {
                artifact_id: "response".into(),
                name: Some("answer".into()),
                description: None,
                parts: vec![Part::text("hello")],
                metadata: None,
            },
            append: Some(false),
            last_chunk: Some(true),
            metadata: None,
        }),
        ..Default::default()
    });

    let content = task_progress_content(&snapshot);
    assert_eq!(content["schema"], "a2a-task-progress.v1");
    assert_eq!(content["task_id"], "task-1");
    assert_eq!(content["context_id"], "ctx-1");
    assert_eq!(content["state"], "TASK_STATE_WORKING");
    assert_eq!(content["text"], "hello");
    assert_eq!(content["artifacts"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn execute_delegate_rejects_state_seed_directly() {
    // Defense-in-depth check: calling `A2aBackend::execute_delegate` directly
    // (i.e. bypassing the capability-gated dispatch helper) must still reject
    // a seeded request rather than silently dropping the seed over the wire.
    let backend = A2aBackend::new(A2aConfig::new("https://example.invalid/v1/a2a"));
    let resolver = NoopResolver;

    let mut extensions = std::collections::HashMap::new();
    extensions.insert("test.seed".to_string(), json!(1));
    let seed = PersistedState {
        revision: 0,
        extensions,
    };

    let result = backend
        .execute_delegate(crate::backend::BackendDelegateRunRequest {
            agent_id: "remote-child",
            messages: vec![Message::user("ignored")],
            new_messages: vec![Message::user("ignored")],
            sink: Arc::new(NullEventSink),
            resolver: &resolver,
            parent: crate::backend::BackendParentContext::default(),
            control: crate::backend::BackendControl::default(),
            policy: crate::backend::BackendDelegatePolicy::default(),
            state_seed: Some(seed),
        })
        .await;

    let err = result.expect_err("A2A must reject seeded delegate requests");
    let message = err.to_string();
    assert!(
        message.contains("delegate_state_seed"),
        "error should name the unsupported capability, got: {message}"
    );
}

async fn spawn_minimal_a2a_server(
    request_paths: Arc<StdMutex<Vec<String>>>,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept test request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let n = socket.read(&mut buffer).await.expect("read request");
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..n]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request_text = String::from_utf8_lossy(&request);
            let path = request_text
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("<missing-path>")
                .to_string();
            request_paths.lock().expect("paths lock").push(path.clone());

            let body = if path.ends_with("/message:send") {
                serde_json::to_string(&SendMessageResponse::task(make_task(TaskState::Working)))
                    .expect("serialize task")
            } else if path.ends_with("/tasks/task-1:cancel") {
                "{}".to_string()
            } else {
                serde_json::json!({"error": "unexpected path", "path": path}).to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        }
    });

    (format!("http://{addr}"), handle)
}

async fn wait_for_recorded_path(request_paths: &Arc<StdMutex<Vec<String>>>, suffix: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if request_paths
                .lock()
                .expect("paths lock")
                .iter()
                .any(|path| path.ends_with(suffix))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for request path ending with {suffix}"));
}

async fn spawn_midflight_cancel_a2a_server(
    request_paths: Arc<StdMutex<Vec<String>>>,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let cancel_seen = Arc::new(Notify::new());
    let handle = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.expect("accept test request");
            let request_paths = request_paths.clone();
            let cancel_seen = cancel_seen.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    let n = socket.read(&mut buffer).await.expect("read request");
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request_text = String::from_utf8_lossy(&request);
                let path = request_text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("<missing-path>")
                    .to_string();
                request_paths.lock().expect("paths lock").push(path.clone());

                let body = if path.ends_with("/message:send") {
                    serde_json::to_string(&SendMessageResponse::task(make_task(TaskState::Working)))
                        .expect("serialize task")
                } else if path.ends_with("/tasks/task-1:subscribe") {
                    cancel_seen.notified().await;
                    String::new()
                } else if path.ends_with("/tasks/task-1:cancel") {
                    cancel_seen.notify_waiters();
                    "{}".to_string()
                } else {
                    serde_json::json!({"error": "unexpected path", "path": path}).to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            });
        }
    });

    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn execute_delegate_cancels_remote_task_when_parent_token_cancelled() {
    let request_paths = Arc::new(StdMutex::new(Vec::new()));
    let (base_url, server) = spawn_minimal_a2a_server(request_paths.clone()).await;
    let backend = A2aBackend::new(A2aConfig::new(base_url));
    let resolver = NoopResolver;
    let token = CancellationToken::new();
    token.cancel();

    let result = backend
        .execute_delegate(crate::backend::BackendDelegateRunRequest {
            agent_id: "remote-child",
            messages: vec![Message::user("delegate")],
            new_messages: vec![Message::user("delegate")],
            sink: Arc::new(NullEventSink),
            resolver: &resolver,
            parent: crate::backend::BackendParentContext::default(),
            control: crate::backend::BackendControl {
                cancellation_token: Some(token),
                decision_rx: None,
            },
            policy: crate::backend::BackendDelegatePolicy::default(),
            state_seed: None,
        })
        .await
        .expect("cancelled delegate should return terminal result");

    assert!(matches!(result.status, BackendRunStatus::Cancelled));
    server.await.expect("server task should finish");
    let paths = request_paths.lock().expect("paths lock").clone();
    assert!(
        paths.iter().any(|path| path.ends_with("/message:send")),
        "delegate should submit a remote A2A task first: {paths:?}"
    );
    assert!(
        paths
            .iter()
            .any(|path| path.ends_with("/tasks/task-1:cancel")),
        "delegate cancellation should call remote task cancel endpoint: {paths:?}"
    );
}

#[tokio::test]
async fn execute_delegate_midflight_cancel_preserves_remote_task_context() {
    let request_paths = Arc::new(StdMutex::new(Vec::new()));
    let (base_url, server) = spawn_midflight_cancel_a2a_server(request_paths.clone()).await;
    let backend = A2aBackend::new(A2aConfig::new(base_url));
    let target_key = backend.remote_target_key();
    let token = CancellationToken::new();
    let token_for_request = token.clone();

    let execution = tokio::spawn(async move {
        let resolver = NoopResolver;
        backend
            .execute_delegate(crate::backend::BackendDelegateRunRequest {
                agent_id: "remote-child",
                messages: vec![Message::user("delegate")],
                new_messages: vec![Message::user("delegate")],
                sink: Arc::new(NullEventSink),
                resolver: &resolver,
                parent: crate::backend::BackendParentContext::default(),
                control: crate::backend::BackendControl {
                    cancellation_token: Some(token_for_request),
                    decision_rx: None,
                },
                policy: crate::backend::BackendDelegatePolicy::default(),
                state_seed: None,
            })
            .await
    });

    wait_for_recorded_path(&request_paths, "/message:send").await;
    wait_for_recorded_path(&request_paths, "/tasks/task-1:subscribe").await;
    token.cancel();

    let result = execution
        .await
        .expect("delegate task should join")
        .expect("cancelled delegate should return terminal result");

    assert!(matches!(result.status, BackendRunStatus::Cancelled));
    let state = result
        .state
        .expect("cancelled delegate should retain persisted remote task state");
    let remote = read_remote_state_entry(&state, &target_key).expect("remote state entry");
    assert_eq!(remote.task_id.as_deref(), Some("task-1"));
    assert_eq!(remote.context_id.as_deref(), Some("ctx-1"));
    assert_eq!(remote.last_state.as_deref(), Some("TASK_STATE_CANCELED"));

    let paths = request_paths.lock().expect("paths lock").clone();
    assert!(
        paths
            .iter()
            .any(|path| path.ends_with("/tasks/task-1:cancel")),
        "mid-flight delegate cancellation should call remote task cancel endpoint: {paths:?}"
    );
    server.abort();
}
