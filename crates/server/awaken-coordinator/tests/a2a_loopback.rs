//! A delegated A2A Agent uses the same publication, child Run, attempt registry,
//! credential admission and commit path as a directly admitted remote Run.

#[path = "a2a_loopback/authenticated.rs"]
mod authenticated;
#[path = "a2a_loopback/cancellation.rs"]
mod cancellation;

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::delegation::DelegationStatus;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_coordinator::ExecutorComponents;
use awaken_credential_contract::{CredentialPurpose, CredentialTarget};
use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};
use awaken_protocol_a2a::{Response, Transport};
use awaken_run_executor_a2a::{A2aRunExecutor, TransportResolver};
use awaken_runtime_contract::StaticPublishedAgentSnapshots;
use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
use awaken_runtime_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
    CredentialUsage,
};
use awaken_scenario_host::{
    EchoModel, build_router, build_unmounted_dependencies_with_agent_publications,
};
use awaken_service_lifecycle::run_composed_async_test;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use http_body_util::BodyExt;
use tower::ServiceExt;

struct RouterTransport {
    app: Router,
}

#[async_trait::async_trait]
impl Transport for RouterTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response, String> {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(body.map(Body::from).unwrap_or_else(Body::empty))
            .map_err(|error| error.to_string())?;
        // Cause/effect rule T1: the production A2A transport crosses a socket and
        // therefore cannot poll the complete remote Coordinator underneath the
        // parent's already-deep Runtime future. This in-process transport is the
        // sole network substitute in this test, so give it the same scheduling
        // boundary. Without it, C1 nested parent+remote Runtime futures => E1 a
        // test-thread stack overflow before any protocol result; with it => E2
        // the ordinary response/error contract is observed on a separate task.
        let response = tokio::spawn(self.app.clone().oneshot(request))
            .await
            .map_err(|error| format!("remote A2A task failed: {error}"))?
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|error| error.to_string())?
            .to_bytes()
            .to_vec();
        Ok(Response::new(status, body))
    }
}

struct FixedTransportResolver(Arc<dyn Transport>);

#[async_trait::async_trait]
impl TransportResolver for FixedTransportResolver {
    async fn resolve(
        &self,
        _candidate: &ResolvedModelCandidate,
        _context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<Arc<dyn Transport>, String> {
        Ok(self.0.clone())
    }
}

fn user(text: &str) -> Message {
    Message::text(MessageId("user".into()), Role::User, text)
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn assert_committed_child_report(host: &ExecutorComponents, thread: &str) {
    let relationships = host
        .thread_reads()
        .delegated_runs(thread)
        .await
        .expect("committed child-Run relationships remain readable");
    assert_eq!(
        relationships.len(),
        1,
        "one agent_run call owns one committed child Run: {relationships:?}"
    );
    let relationship = &relationships[0];
    assert_eq!(relationship.agent_id, "researcher");
    assert_eq!(relationship.parent_call_id, "native-a2a-delegate");
    assert_eq!(relationship.status, DelegationStatus::Completed);
    assert!(
        !relationship.run_id.0.is_empty(),
        "child Run identity is stable"
    );

    let messages = host
        .thread_reads()
        .committed_messages(thread)
        .await
        .expect("committed parent transcript remains readable");
    let tool_use_index = messages
        .iter()
        .position(|message| {
            message.role == Role::Assistant
                && message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::ToolUse { id, name, input }
                            if id == &relationship.parent_call_id
                                && name == "agent_run"
                                && input["agent_id"] == "researcher"
                    )
                })
        })
        .expect("parent commits the agent_run request");
    let (tool_result_index, child_report) = messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| {
            (message.role == Role::Tool
                && message
                    .id
                    .is_tool_result_for_call(&relationship.parent_call_id))
            .then(|| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } if tool_use_id == &relationship.parent_call_id && !is_error => {
                        Some(awaken_session_executor::block_text(content))
                    }
                    _ => None,
                })
            })
            .flatten()
            .map(|report| (index, report))
        })
        .unwrap_or_else(|| {
            panic!("completed child Run lacks its correlated non-error ToolResult: {messages:#?}")
        });
    assert!(
        !child_report.is_empty(),
        "committed child report is non-empty"
    );
    let (reply_index, reply) = messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| {
            (index > tool_result_index && message.role == Role::Assistant)
                .then(|| text_of(message))
                .filter(|text| text.starts_with("delegate said:"))
                .map(|text| (index, text))
        })
        .expect("parent commits a reply derived from the child report");
    assert_eq!(reply, format!("delegate said: {child_report}"));
    assert!(
        tool_use_index < tool_result_index && tool_result_index < reply_index,
        "committed causality is ToolUse -> child ToolResult -> parent reply"
    );
}

/// Native A2A fixture kept local to this protocol test. The shared scenario
/// `DelegatingModel` owns Managed `list_agents`/`send_message`; reusing it here
/// would merge two distinct protocol contracts back into one compatibility
/// model.
struct NativeDelegatingModel;

#[async_trait::async_trait]
impl LlmExecutor for NativeDelegatingModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let result = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .map(|message| awaken_session_executor::block_text(&message.content));
        let output = match result {
            Some(result) => AssistantOutput::text(format!("delegate said: {result}")),
            None => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "native-a2a-delegate".into(),
                tool_id: "agent_run".into(),
                arguments: serde_json::json!({
                    "agent_id": "researcher",
                    "input": "do the research"
                }),
            }]),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

async fn delegating_host(transport: Arc<dyn Transport>) -> Arc<ExecutorComponents> {
    let parent = ExecutableAgentSnapshot::builder("assistant")
        .model(ModelBinding::new("default", "parent", "default"))
        .tools(awaken_session_executor::authorable_tools())
        .agent_bindings(AgentBindings {
            delegates: vec![AgentDelegateBinding {
                agent_id: AgentId("researcher".into()),
                source_revision: None,
                recursive_self: false,
            }],
            ..Default::default()
        })
        .build();
    let remote = ExecutableAgentSnapshot::builder("researcher")
        .resolved_model(
            ResolvedModelCandidate::try_remote(
                ModelBinding::new("remote", "", "a2a:http://remote.invalid"),
                awaken_tenancy::ScopeId::from("default"),
                None,
                "test-fixed-transport",
            )
            .expect("coherent anonymous A2A candidate"),
        )
        .build();
    let publications = StaticPublishedAgentSnapshots::try_new([parent, remote])
        .expect("parent and delegated remote publications are valid");
    (build_unmounted_dependencies_with_agent_publications(
        Arc::new(NativeDelegatingModel),
        "parent",
        Arc::new(publications),
    )
    .await
    .with_remote_attempt_executor(awaken_session_executor::RemoteAttemptInstallation {
        executor: Arc::new(A2aRunExecutor::new(Arc::new(FixedTransportResolver(
            transport,
        )))),
        credential_realization: Default::default(),
    }))
    .build()
    .expect("validated Host dependencies")
}

async fn published_delegating_host(
    endpoint: &str,
    credential: CredentialAccess,
    security_fingerprint: String,
    materializer: awaken_credential_materializer::PinnedCredentialMaterializer,
) -> Arc<ExecutorComponents> {
    let parent = ExecutableAgentSnapshot::builder("assistant")
        .model(ModelBinding::new("default", "parent", "default"))
        .tools(awaken_session_executor::authorable_tools())
        .agent_bindings(AgentBindings {
            delegates: vec![AgentDelegateBinding {
                agent_id: AgentId("researcher".into()),
                source_revision: None,
                recursive_self: false,
            }],
            ..Default::default()
        })
        .build();
    let remote = ExecutableAgentSnapshot::builder("researcher")
        .resolved_model(
            ResolvedModelCandidate::try_remote(
                ModelBinding::new("remote", "", format!("a2a:{endpoint}")),
                awaken_tenancy::ScopeId::from("default"),
                Some(credential),
                security_fingerprint,
            )
            .expect("coherent authenticated A2A candidate"),
        )
        .build();
    let publications = StaticPublishedAgentSnapshots::try_new([parent, remote])
        .expect("authenticated parent and child publications are valid");
    (build_unmounted_dependencies_with_agent_publications(
        Arc::new(NativeDelegatingModel),
        "parent",
        Arc::new(publications),
    )
    .await
    .with_remote_attempt_executor(awaken_coordinator::a2a_attempt_executor(Some(materializer))))
    .build()
    .expect("validated Host dependencies")
}

#[tokio::test]
async fn delegated_remote_uses_the_published_child_run_and_attempt_executor() {
    // Causes:
    // C1 parent publication permits researcher and freezes AgentDelegation; C2
    // researcher publication pins a2a:*; C3 one remote attempt executor is
    // installed; C4 the remote protocol has no pre-existing Session but carries
    // the router's authenticated local owner; C5 the local native fixture has
    // no/one ToolResult. Effects: E1 agent_run creates the stable child Run; E2
    // exact backend routing sends message:send through A2A; E3 C4 creates the
    // default Session under that one owner before admission; E4 child and parent
    // commit ordinary results; E5 C5=no result emits exactly agent_run, while
    // C5=result reports delegate said.
    //
    // C6 the canonical unmounted Scenario Host installs one Resources application,
    // including artifact capture. C6 + successful child/parent commits => no
    // missing-publication error, even for an empty artifact set. Both anonymous
    // and authenticated paths exercise this composition (no substitute publisher).
    // Constraints/invariants: RunDelegations and the parent transcript are the
    // committed relationship/report authorities; A2A owns only its wire projection.
    // Decision rule U1: C1+C2+C3+C4+C5(no result) => E1+E2+E3+E5;
    // U2=the resulting ToolResult=>E4+E5(report). Missing C1 is covered by the
    // resolved-tool and target gates; missing C2/C3 is covered by fail-closed
    // resolver tests in awaken-session-executor.
    run_composed_async_test(|| async {
        let transport = Arc::new(RouterTransport {
            app: build_router(Arc::new(EchoModel), "remote").await,
        });
        let host = delegating_host(transport).await;

        host.session_runs()
            .run(None, "thread", vec![user("research the answer")])
            .await
            .expect("delegated A2A child settles through the ordinary Run path");

        assert_committed_child_report(&host, "thread").await;
    });
}

#[tokio::test]
async fn origin_credential_authenticates_the_unified_delegated_a2a_attempt() {
    // Causes:
    // C1 the published remote child pins a card fingerprint, verified origin
    // target, and one exact origin-tagged credential revision; C2 the card requires Bearer auth; C3
    // only the ordinary A2A attempt installation is configured; C4 the native
    // fixture receives no/one ToolResult.
    // Effects: E1 child admission freezes one claim binding; E2 the production resolver
    // materializes that binding and injects Bearer auth; E3 the remote Run and
    // parent settle through their ordinary commit boundaries; E4 C4 selects the
    // exact agent_run call or terminal delegate report.
    //
    // Constraints/invariants: the origin credential is materialized only at the
    // A2A transport edge; committed RunDelegations and messages own causal truth.
    // Decision rule E1: C1+C2+C3+C4(no result) => E1+E2+E4(call); E2: the
    // authenticated ToolResult => E3+E4(report). Missing credential and card
    // fingerprint drift are the fail-closed rules in `a2a_remote` and
    // `PinnedA2aTransportResolver`; anonymous U1/U2 are covered above.
    run_composed_async_test(|| async {
        let peer = authenticated::AuthenticatedPeer::start(
            build_router(Arc::new(EchoModel), "remote").await,
        )
        .await;
        let host = published_delegating_host(
            &peer.endpoint,
            peer.access.clone(),
            peer.security_fingerprint.clone(),
            peer.materializer(),
        )
        .await;

        host.session_runs()
            .run(
                None,
                "authenticated-thread",
                vec![user("research securely")],
            )
            .await
            .expect("origin credential authenticates the delegated A2A child");
        assert_committed_child_report(&host, "authenticated-thread").await;
        assert!(
            peer.requests
                .lock()
                .unwrap()
                .iter()
                .all(|(_, _, authorized)| *authorized)
        );
    });
}
