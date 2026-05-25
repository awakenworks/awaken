use super::*;

#[test]
fn api_error_bad_request_response() {
    let err = ApiError::BadRequest("missing field".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn api_error_not_found_response() {
    let err = ApiError::NotFound("resource".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn api_error_thread_not_found_response() {
    let err = ApiError::ThreadNotFound("t-123".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn api_error_run_not_found_response() {
    let err = ApiError::RunNotFound("r-123".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn api_error_internal_response() {
    let err = ApiError::Internal("db error".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn convert_run_messages_works() {
    let msgs = vec![
        RunMessage {
            role: "user".into(),
            content: "hello".into(),
        },
        RunMessage {
            role: "unknown".into(),
            content: "x".into(),
        },
    ];
    let converted = convert_run_messages(msgs);
    assert_eq!(converted.len(), 1);
    assert_eq!(converted[0].text(), "hello");
}

#[test]
fn list_params_defaults() {
    let params: ListParams = serde_json::from_str("{}").unwrap();
    assert_eq!(params.offset, None);
    assert_eq!(params.limit, 50);
}

#[test]
fn decision_payload_deserialize() {
    let json = r#"{"toolCallId":"c1","action":"resume","payload":{"approved":true}}"#;
    let payload: DecisionPayload = serde_json::from_str(json).unwrap();
    assert_eq!(payload.tool_call_id, "c1");
    assert_eq!(payload.action, "resume");
}

// ── CreateRunPayload deserialization ──

#[test]
fn create_run_payload_camel_case() {
    let json = r#"{"agentId":"a1","threadId":"t1","messages":[{"role":"user","content":"hi"}]}"#;
    let p: CreateRunPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.agent_id, "a1");
    assert_eq!(p.thread_id.as_deref(), Some("t1"));
    assert_eq!(p.messages.len(), 1);
    assert_eq!(p.messages[0].role, "user");
    assert_eq!(p.messages[0].content, "hi");
}

#[test]
fn create_run_payload_snake_case_alias() {
    let json = r#"{"agent_id":"a2","thread_id":"t2","messages":[]}"#;
    let p: CreateRunPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.agent_id, "a2");
    assert_eq!(p.thread_id.as_deref(), Some("t2"));
    assert!(p.messages.is_empty());
}

#[test]
fn create_run_payload_defaults() {
    let json = r#"{"agentId":"a3"}"#;
    let p: CreateRunPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.agent_id, "a3");
    assert_eq!(p.thread_id, None);
    assert!(p.messages.is_empty());
}

#[test]
fn create_run_payload_missing_agent_id_fails() {
    let json = r#"{"messages":[]}"#;
    let result = serde_json::from_str::<CreateRunPayload>(json);
    assert!(result.is_err());
}

// ── PushRunInputsPayload deserialization ──

#[test]
fn push_run_inputs_payload_empty_default() {
    let json = r#"{}"#;
    let p: PushRunInputsPayload = serde_json::from_str(json).unwrap();
    assert!(p.messages.is_empty());
    assert_eq!(p.mode, PushInputMode::Queue);
}

#[test]
fn push_run_inputs_payload_with_messages() {
    let json =
        r#"{"messages":[{"role":"user","content":"msg1"},{"role":"user","content":"msg2"}]}"#;
    let p: PushRunInputsPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.messages.len(), 2);
    assert_eq!(p.messages[0].content, "msg1");
    assert_eq!(p.messages[1].content, "msg2");
}

#[test]
fn push_run_inputs_payload_accepts_interrupt_then_queue_mode() {
    let json =
        r#"{"mode":"interrupt_then_queue","messages":[{"role":"user","content":"redirect"}]}"#;
    let p: PushRunInputsPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.mode, PushInputMode::InterruptThenQueue);
    assert_eq!(p.messages.len(), 1);
}

#[test]
fn push_run_inputs_payload_accepts_live_then_queue_mode_and_steer_alias() {
    let json = r#"{"mode":"live_then_queue","messages":[{"role":"user","content":"steer"}]}"#;
    let p: PushRunInputsPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.mode, PushInputMode::LiveThenQueue);
    assert_eq!(p.messages.len(), 1);

    let alias = r#"{"mode":"steer","messages":[{"role":"user","content":"steer"}]}"#;
    let p: PushRunInputsPayload = serde_json::from_str(alias).unwrap();
    assert_eq!(p.mode, PushInputMode::LiveThenQueue);
}

#[test]
fn push_run_inputs_payload_accepts_resume_open_run_mode() {
    let json = r#"{"mode":"resume_open_run","messages":[{"role":"user","content":"continue"}]}"#;
    let p: PushRunInputsPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.mode, PushInputMode::ResumeOpenRun);
    assert_eq!(p.messages.len(), 1);
}

// ── ListRunsParams deserialization ──

#[test]
fn list_runs_params_defaults() {
    let p: ListRunsParams = serde_json::from_str("{}").unwrap();
    assert_eq!(p.offset, None);
    assert_eq!(p.limit, 50);
    assert_eq!(p.status, None);
}

#[test]
fn list_runs_params_with_status_filter() {
    let json = r#"{"offset":10,"limit":25,"status":"running"}"#;
    let p: ListRunsParams = serde_json::from_str(json).unwrap();
    assert_eq!(p.offset, Some(10));
    assert_eq!(p.limit, 25);
    assert_eq!(p.status.as_deref(), Some("running"));
}

// ── PatchThreadPayload deserialization ──

#[test]
fn patch_thread_payload_title_only() {
    let json = r#"{"title":"new title"}"#;
    let p: PatchThreadPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.title.as_deref(), Some("new title"));
    assert!(p.custom.is_none());
}

#[test]
fn patch_thread_payload_custom_only() {
    let json = r#"{"custom":{"key":"value"}}"#;
    let p: PatchThreadPayload = serde_json::from_str(json).unwrap();
    assert!(p.title.is_none());
    let custom = p.custom.unwrap();
    assert_eq!(custom.get("key").unwrap(), "value");
}

#[test]
fn patch_thread_payload_both() {
    let json = r#"{"title":"t","custom":{"k":"v"}}"#;
    let p: PatchThreadPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.title.as_deref(), Some("t"));
    assert!(p.custom.is_some());
}

#[test]
fn patch_thread_payload_empty() {
    let json = r#"{}"#;
    let p: PatchThreadPayload = serde_json::from_str(json).unwrap();
    assert!(p.title.is_none());
    assert!(p.custom.is_none());
}

// ── DecisionPayload additional tests ──

#[test]
fn decision_payload_snake_case_alias() {
    let json = r#"{"tool_call_id":"c2","action":"cancel"}"#;
    let p: DecisionPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.tool_call_id, "c2");
    assert_eq!(p.action, "cancel");
    assert_eq!(p.payload, Value::Null);
}

#[test]
fn decision_payload_missing_required_fails() {
    // missing action
    let json = r#"{"toolCallId":"c1"}"#;
    assert!(serde_json::from_str::<DecisionPayload>(json).is_err());

    // missing tool_call_id
    let json = r#"{"action":"resume"}"#;
    assert!(serde_json::from_str::<DecisionPayload>(json).is_err());
}

// ── convert_run_messages edge cases ──

#[test]
fn convert_run_messages_empty() {
    let converted = convert_run_messages(vec![]);
    assert!(converted.is_empty());
}

#[test]
fn convert_run_messages_system_role() {
    let msgs = vec![RunMessage {
        role: "system".into(),
        content: "you are helpful".into(),
    }];
    let converted = convert_run_messages(msgs);
    assert_eq!(converted.len(), 1);
    assert_eq!(converted[0].text(), "you are helpful");
}

#[test]
fn convert_run_messages_assistant_role() {
    let msgs = vec![RunMessage {
        role: "assistant".into(),
        content: "sure".into(),
    }];
    let converted = convert_run_messages(msgs);
    assert_eq!(converted.len(), 1);
    assert_eq!(converted[0].text(), "sure");
}

#[test]
fn convert_run_messages_mixed_known_unknown() {
    let msgs = vec![
        RunMessage {
            role: "user".into(),
            content: "a".into(),
        },
        RunMessage {
            role: "assistant".into(),
            content: "b".into(),
        },
        RunMessage {
            role: "system".into(),
            content: "c".into(),
        },
        RunMessage {
            role: "function".into(),
            content: "d".into(),
        },
        RunMessage {
            role: "tool".into(),
            content: "e".into(),
        },
    ];
    let converted = convert_run_messages(msgs);
    assert_eq!(converted.len(), 3);
    assert_eq!(converted[0].text(), "a");
    assert_eq!(converted[1].text(), "b");
    assert_eq!(converted[2].text(), "c");
}

// ── ApiError response body validation ──

#[tokio::test]
async fn api_error_thread_not_found_body() {
    let err = ApiError::ThreadNotFound("t-abc".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"], "thread not found: t-abc");
}

#[tokio::test]
async fn api_error_run_not_found_body() {
    let err = ApiError::RunNotFound("r-xyz".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"], "run not found: r-xyz");
}

#[tokio::test]
async fn api_error_internal_body() {
    let err = ApiError::Internal("db crashed".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"], "db crashed");
}

#[tokio::test]
async fn api_error_bad_request_body() {
    let err = ApiError::BadRequest("invalid input".into());
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"], "invalid input");
}

// ── CreateThreadPayload deserialization ──

#[test]
fn create_thread_payload_with_title() {
    let json = r#"{"title":"my thread"}"#;
    let p: CreateThreadPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.title.as_deref(), Some("my thread"));
}

#[test]
fn create_thread_payload_empty() {
    let json = r#"{}"#;
    let p: CreateThreadPayload = serde_json::from_str(json).unwrap();
    assert!(p.title.is_none());
}

// ── MailboxPayload deserialization ──

#[test]
fn mailbox_payload_default() {
    let json = r#"{}"#;
    let p: MailboxPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.payload, Value::Null);
}

#[test]
fn mailbox_payload_with_content() {
    let json = r#"{"payload":{"text":"hello"}}"#;
    let p: MailboxPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.payload["text"], "hello");
}

// ── PostThreadMessagesPayload deserialization ──

#[test]
fn post_thread_messages_payload_camel_case() {
    let json = r#"{"agentId":"agent-1","messages":[{"role":"user","content":"test"}]}"#;
    let p: PostThreadMessagesPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.agent_id.as_deref(), Some("agent-1"));
    assert_eq!(p.messages.len(), 1);
}

#[test]
fn post_thread_messages_payload_snake_case_alias() {
    let json = r#"{"agent_id":"agent-2","messages":[]}"#;
    let p: PostThreadMessagesPayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.agent_id.as_deref(), Some("agent-2"));
}

#[test]
fn post_thread_messages_payload_defaults() {
    let json = r#"{}"#;
    let p: PostThreadMessagesPayload = serde_json::from_str(json).unwrap();
    assert!(p.agent_id.is_none());
    assert!(p.messages.is_empty());
}

// ── RunMessage deserialization ──

#[test]
fn run_message_deserialize() {
    let json = r#"{"role":"user","content":"hello world"}"#;
    let m: RunMessage = serde_json::from_str(json).unwrap();
    assert_eq!(m.role, "user");
    assert_eq!(m.content, "hello world");
}

#[test]
fn run_message_missing_field_fails() {
    let json = r#"{"role":"user"}"#;
    assert!(serde_json::from_str::<RunMessage>(json).is_err());
}

// ── ListParams with explicit values ──

#[test]
fn list_params_explicit_values() {
    let json = r#"{"offset":5,"limit":100}"#;
    let p: ListParams = serde_json::from_str(json).unwrap();
    assert_eq!(p.offset, Some(5));
    assert_eq!(p.limit, 100);
}

// ── Health check integration tests ──────────────────────────────

mod health_integration {
    use super::*;
    use crate::app::{ServerConfig, ServerState};
    use crate::mailbox::{Mailbox, MailboxConfig};
    use awaken_runtime::AgentRuntime;
    use awaken_stores::{InMemoryMailboxStore, InMemoryStore};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct StubResolver;
    impl awaken_runtime::AgentResolver for StubResolver {
        fn resolve(
            &self,
            agent_id: &str,
        ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
            Err(awaken_runtime::RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
    }

    fn make_app_state() -> ServerState {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
        let store = Arc::new(InMemoryStore::new());
        let mailbox_store = Arc::new(InMemoryMailboxStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            mailbox_store,
            store.clone(),
            "test".to_string(),
            MailboxConfig::default(),
        ));
        let mut state = ServerState::new(
            runtime,
            mailbox,
            store.clone(),
            Arc::new(StubResolver),
            ServerConfig::default(),
        );
        state.admin.admin_api_config.bearer_token = Some("test-admin-token".into());
        state
    }

    #[tokio::test]
    async fn config_routes_return_404_when_admin_surface_disabled() {
        use crate::app::AdminApiConfig;
        use axum::http::StatusCode;

        let mut state = make_app_state();
        state.admin.admin_api_config = AdminApiConfig {
            expose_config_routes: false,
            ..AdminApiConfig::default()
        };
        let app = build_router(&state);

        let req = axum::http::Request::builder()
            .uri("/v1/agents")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "disabled config routes must not be mounted"
        );
    }

    #[tokio::test]
    async fn admin_routes_return_404_when_admin_surface_disabled() {
        use crate::app::AdminApiConfig;
        use axum::http::StatusCode;

        let mut state = make_app_state();
        state.admin.admin_api_config = AdminApiConfig {
            expose_config_routes: false,
            bearer_token: Some("test-admin-token".into()),
            ..AdminApiConfig::default()
        };
        let app = build_router(&state);

        for uri in [
            "/v1/agents/runtime-stats",
            "/v1/agents/default/runtime-stats",
        ] {
            let req = axum::http::Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{uri} must not be mounted when the config-admin surface is disabled"
            );
        }

        let req = axum::http::Request::builder()
            .uri("/v1/system/info")
            .header(axum::http::header::AUTHORIZATION, "Bearer test-admin-token")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_routes_require_bearer_token_when_configured() {
        use crate::app::AdminApiConfig;
        use awaken_contract::RedactedString;
        use axum::http::{StatusCode, header};

        let token = RedactedString::new("admin-token");
        let mut state = make_app_state();
        state.admin.admin_api_config = AdminApiConfig {
            bearer_token: Some(token),
            ..AdminApiConfig::default()
        };
        let app = build_router(&state);

        let req = axum::http::Request::builder()
            .uri("/v1/system/info")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let req = axum::http::Request::builder()
            .uri("/v1/system/info")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn config_routes_are_absent_without_config_module() {
        use axum::http::StatusCode;

        let state = make_app_state();
        let app = build_router(&state);

        let req = axum::http::Request::builder()
            .uri("/v1/agents")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "config routes require a ConfigModuleState"
        );
    }

    #[tokio::test]
    async fn health_live_returns_200() {
        let state = make_app_state();
        let app = build_router(&state);

        let req = axum::http::Request::builder()
            .uri("/health/live")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn system_info_returns_real_fields() {
        let state = make_app_state();
        let app = build_router(&state);

        let req = axum::http::Request::builder()
            .uri("/v1/system/info")
            .header(axum::http::header::AUTHORIZATION, "Bearer test-admin-token")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert!(json["uptime_seconds"].is_u64());
        // make_app_state() does not wire any optional subsystem.
        assert_eq!(json["config_store_enabled"], false);
        assert_eq!(json["audit_log_enabled"], false);
        assert_eq!(json["runtime_stats_enabled"], false);
    }

    #[tokio::test]
    async fn health_ready_returns_healthy_with_working_store() {
        let state = make_app_state();
        let app = build_router(&state);

        let req = axum::http::Request::builder()
            .uri("/health")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "healthy");
        assert_eq!(json["components"]["store"], "ok");
        assert_eq!(json["components"]["runtime"], "ok");
    }

    #[tokio::test]
    async fn metrics_endpoint_is_installed_and_records_http_requests() {
        let state = make_app_state();
        let app = build_router(&state);

        let req = axum::http::Request::builder()
            .uri("/health/live")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = axum::http::Request::builder()
            .uri("/metrics")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("awaken_http_requests_total"),
            "expected HTTP counter in /metrics output: {text}"
        );
        assert!(
            text.contains("route=\"/health/live\""),
            "expected matched route label in /metrics output: {text}"
        );
    }

    #[tokio::test]
    async fn health_ready_returns_unhealthy_with_failing_store() {
        use awaken_contract::contract::message::Message;
        use awaken_contract::contract::storage::{
            RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore,
        };
        use awaken_contract::thread::{Thread, ThreadMetadata};

        /// Store that always returns errors.
        struct FailingStore;

        #[async_trait::async_trait]
        impl ThreadStore for FailingStore {
            async fn load_thread(&self, _id: &str) -> Result<Option<Thread>, StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn save_thread(&self, _t: &Thread) -> Result<(), StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn delete_thread(&self, _id: &str) -> Result<(), StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn list_threads(
                &self,
                _offset: usize,
                _limit: usize,
            ) -> Result<Vec<String>, StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn load_messages(&self, _id: &str) -> Result<Option<Vec<Message>>, StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn save_messages(
                &self,
                _id: &str,
                _msgs: &[Message],
            ) -> Result<(), StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn delete_messages(&self, _id: &str) -> Result<(), StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn update_thread_metadata(
                &self,
                _id: &str,
                _meta: ThreadMetadata,
            ) -> Result<(), StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
        }

        #[async_trait::async_trait]
        impl RunStore for FailingStore {
            async fn create_run(&self, _r: &RunRecord) -> Result<(), StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn load_run(&self, _id: &str) -> Result<Option<RunRecord>, StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn latest_run(&self, _id: &str) -> Result<Option<RunRecord>, StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn list_runs(&self, _q: &RunQuery) -> Result<RunPage, StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
        }

        #[async_trait::async_trait]
        impl ThreadRunStore for FailingStore {
            async fn checkpoint(
                &self,
                _thread_id: &str,
                _messages: &[Message],
                _run: &RunRecord,
            ) -> Result<(), StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
        }

        let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
        let store = Arc::new(FailingStore);
        let mailbox_store = Arc::new(InMemoryMailboxStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            mailbox_store,
            store.clone(),
            "test".to_string(),
            MailboxConfig::default(),
        ));
        let state = ServerState::new(
            runtime,
            mailbox,
            store,
            Arc::new(StubResolver),
            ServerConfig::default(),
        );
        let app = build_router(&state);

        let req = axum::http::Request::builder()
            .uri("/health")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "unhealthy");
        assert_eq!(json["components"]["store"], "error");
    }
}
