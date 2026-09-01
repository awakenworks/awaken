//! A delegated A2A Agent uses the same publication, child Run, attempt registry,
//! credential admission and commit path as a directly admitted remote Run.

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::delegation::DelegationStatus;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_coordinator::SharedHost;
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
use awaken_scenario_host::{EchoModel, build_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use http_body_util::BodyExt;
use tower::ServiceExt;

const COMPOSED_ASYNC_TEST_STACK_BYTES: usize = 32 * 1024 * 1024;

fn run_composed_async_test<F, Fut>(case: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    // The in-process A2A fixture composes parent Runtime, transport, remote
    // Router, and child Runtime on one process stack. Production crosses a
    // socket; this dedicated test executor preserves that scheduling boundary
    // while giving the deliberately composed future a bounded explicit stack.
    let test = std::thread::Builder::new()
        .name("a2a-loopback-composed-test".into())
        .stack_size(COMPOSED_ASYNC_TEST_STACK_BYTES)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("composed A2A test runtime")
                .block_on(case());
        })
        .expect("spawn composed A2A test thread");
    if let Err(panic) = test.join() {
        std::panic::resume_unwind(panic);
    }
}

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

async fn assert_committed_child_report(host: &SharedHost, thread: &str) {
    let relationships = host
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
    let expected_result_id = MessageId::tool_result(&relationship.parent_call_id);
    let (tool_result_index, child_report) = messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| {
            (message.role == Role::Tool && message.id == expected_result_id)
                .then(|| {
                    message.content.iter().find_map(|block| match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } if tool_use_id == &relationship.parent_call_id && !is_error => {
                            Some(awaken_runtime_host::block_text(content))
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
            .map(|message| awaken_runtime_host::block_text(&message.content));
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

fn delegating_host(transport: Arc<dyn Transport>) -> SharedHost {
    let parent = ExecutableAgentSnapshot::builder("assistant")
        .model(ModelBinding::new("default", "parent", "default"))
        .tools(awaken_runtime_host::authorable_tools())
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
    SharedHost::new(Arc::new(NativeDelegatingModel), "parent")
        .with_agent_publications(Arc::new(publications))
        .with_remote_attempt_executor(awaken_runtime_host::RemoteAttemptInstallation {
            executor: Arc::new(A2aRunExecutor::new(Arc::new(FixedTransportResolver(
                transport,
            )))),
            credential_realization: Default::default(),
        })
}

fn published_delegating_host(
    endpoint: &str,
    credential: CredentialAccess,
    security_fingerprint: String,
    materializer: awaken_credential_materializer::PinnedCredentialMaterializer,
) -> SharedHost {
    let parent = ExecutableAgentSnapshot::builder("assistant")
        .model(ModelBinding::new("default", "parent", "default"))
        .tools(awaken_runtime_host::authorable_tools())
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
    SharedHost::new(Arc::new(NativeDelegatingModel), "parent")
        .with_agent_publications(Arc::new(publications))
        .with_remote_attempt_executor(awaken_coordinator::a2a_attempt_executor(Some(materializer)))
}

#[test]
fn delegated_remote_uses_the_published_child_run_and_attempt_executor() {
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
    // Constraints/invariants: RunDelegations and the parent transcript are the
    // committed relationship/report authorities; A2A owns only its wire projection.
    // Decision rule U1: C1+C2+C3+C4+C5(no result) => E1+E2+E3+E5;
    // U2=the resulting ToolResult=>E4+E5(report). Missing C1 is covered by the
    // resolved-tool and target gates; missing C2/C3 is covered by fail-closed
    // resolver tests in awaken-runtime-host.
    run_composed_async_test(|| async {
        let transport = Arc::new(RouterTransport {
            app: build_router(Arc::new(EchoModel), "remote"),
        });
        let host = delegating_host(transport);

        host.run(None, "thread", vec![user("research the answer")])
            .await
            .expect("delegated A2A child settles through the ordinary Run path");

        assert_committed_child_report(&host, "thread").await;
    });
}

async fn require_remote_bearer(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        != Some("Bearer remote-secret")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

#[test]
fn origin_credential_authenticates_the_unified_delegated_a2a_attempt() {
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind authenticated A2A peer");
        let endpoint = format!("http://{}", listener.local_addr().expect("peer address"));
        let mut card = awaken_protocol_a2a::agent_card("remote");
        card.url = format!("{endpoint}/v1/a2a");
        card.security_schemes.insert(
            "bearer".into(),
            serde_json::from_value(serde_json::json!({
                "type": "http",
                "scheme": "Bearer"
            }))
            .expect("valid HTTP Bearer security scheme"),
        );
        card.security = serde_json::from_value(serde_json::json!([{"bearer": []}]))
            .expect("valid security requirement");
        let security_fingerprint =
            awaken_runtime_contract::content_fingerprint(&(&card.security_schemes, &card.security))
                .map(|fingerprint| format!("sha256:{fingerprint}"))
                .expect("fingerprint card security");
        let card_router = Router::new().route(
            awaken_protocol_a2a::client::AGENT_CARD_PATH,
            get({
                let card = card.clone();
                move || {
                    let card = card.clone();
                    async move { axum::Json(card) }
                }
            }),
        );
        let authenticated_peer = build_router(Arc::new(EchoModel), "remote")
            .layer(axum::middleware::from_fn(require_remote_bearer));
        let peer = card_router.fallback_service(authenticated_peer);
        let peer_task = tokio::spawn(async move {
            axum::serve(listener, peer)
                .await
                .expect("serve authenticated A2A peer");
        });

        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let entered = enter_credential(
            CredentialCreateParams {
                workspace_id: "default".into(),
                kind: CredentialKind::Vault,
                provider_id: Some(endpoint.clone()),
                env_key: None,
                secret: Some(RedactedString::new("remote-secret")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .expect("enter origin credential");
        let access = CredentialAccess::new(
            CredentialRef {
                id: entered.id.0,
                revision: u64::try_from(entered.version).expect("positive credential revision"),
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            CredentialExecutionPolicy::self_hosted_provider(),
        )
        .with_target(CredentialTarget::new(
            CredentialPurpose::RemoteAgentAuthorization,
            endpoint.clone(),
        ));
        let host = published_delegating_host(
            &endpoint,
            access,
            security_fingerprint,
            awaken_credential_materializer::PinnedCredentialMaterializer::new(credentials, secrets),
        );

        host.run(
            None,
            "authenticated-thread",
            vec![user("research securely")],
        )
        .await
        .expect("origin credential authenticates the delegated A2A child");
        assert_committed_child_report(&host, "authenticated-thread").await;
        peer_task.abort();
    });
}
