//! A delegated A2A Agent uses the same publication, child Run, attempt registry,
//! credential admission and commit path as a directly admitted remote Run.

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};
use awaken_protocol_a2a::{Response, Transport};
use awaken_run_executor_a2a::{A2aRunExecutor, TransportResolver};
use awaken_runtime_contract::StaticPublishedAgentSnapshots;
use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
use awaken_runtime_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
    CredentialUsage,
};
use awaken_scenario_host::{DelegatingModel, EchoModel, build_router};
use awaken_server::SharedHost;
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
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
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
        .resolved_model(ResolvedModelCandidate::remote(
            ModelBinding::new("remote", "", "a2a:http://remote.invalid"),
            awaken_tenancy::ScopeId::from("default"),
            None,
            "test-fixed-transport",
        ))
        .build();
    let publications = StaticPublishedAgentSnapshots::try_new([parent, remote])
        .expect("parent and delegated remote publications are valid");
    SharedHost::new(Arc::new(DelegatingModel), "parent")
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
    materializer: awaken_runtime_host::PinnedCredentialMaterializer,
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
        .resolved_model(ResolvedModelCandidate::remote(
            ModelBinding::new("remote", "", format!("a2a:{endpoint}")),
            awaken_tenancy::ScopeId::from("default"),
            Some(credential),
            security_fingerprint,
        ))
        .build();
    let publications = StaticPublishedAgentSnapshots::try_new([parent, remote])
        .expect("authenticated parent and child publications are valid");
    SharedHost::new(Arc::new(DelegatingModel), "parent")
        .with_agent_publications(Arc::new(publications))
        .with_remote_attempt_executor(awaken_server::a2a_attempt_executor(Some(materializer)))
}

#[tokio::test]
async fn delegated_remote_uses_the_published_child_run_and_attempt_executor() {
    // Cause/effect graph:
    // C1 parent publication permits researcher; C2 researcher publication pins
    // a2a:*; C3 one remote attempt executor is installed.
    // E1 agent_run creates the stable child Run; E2 exact backend routing sends
    // message:send through A2A; E3 child and parent commit ordinary results.
    //
    // Decision rule U1: C1+C2+C3 => E1+E2+E3. Missing C1 is covered by the
    // delegation target gate; missing C2/C3 is covered by fail-closed resolver
    // tests in awaken-runtime-host.
    let transport = Arc::new(RouterTransport {
        app: build_router(Arc::new(EchoModel), "remote"),
    });
    let host = delegating_host(transport);

    host.run(None, "thread", vec![user("research the answer")])
        .await
        .expect("delegated A2A child settles through the ordinary Run path");

    let reply = host
        .committed_messages("thread")
        .await
        .iter()
        .rev()
        .find(|message| {
            matches!(message.role, Role::Assistant) && text_of(message).contains("delegate said:")
        })
        .map(text_of)
        .expect("parent commits the child result");
    assert!(reply.contains("delegate said: Echo: do the research"));
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

#[tokio::test]
async fn origin_credential_authenticates_the_unified_delegated_a2a_attempt() {
    // Cause/effect graph:
    // C1 the published remote child pins a card fingerprint and one exact
    // origin-tagged credential revision; C2 the card requires Bearer auth; C3
    // only the ordinary A2A attempt installation is configured.
    // E1 child admission freezes one claim binding; E2 the production resolver
    // materializes that binding and injects Bearer auth; E3 the remote Run and
    // parent settle through their ordinary commit boundaries.
    //
    // Decision rule E1: C1+C2+C3 => E1+E2+E3. Missing credential and card
    // fingerprint drift are the fail-closed rules in `a2a_remote` and
    // `PinnedA2aTransportResolver`; anonymous U1 is covered above.
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
    );
    let host = published_delegating_host(
        &endpoint,
        access,
        security_fingerprint,
        awaken_runtime_host::PinnedCredentialMaterializer::new(credentials, secrets),
    );

    host.run(
        None,
        "authenticated-thread",
        vec![user("research securely")],
    )
    .await
    .expect("origin credential authenticates the delegated A2A child");
    let reply = host
        .committed_messages("authenticated-thread")
        .await
        .iter()
        .rev()
        .find(|message| {
            matches!(message.role, Role::Assistant) && text_of(message).contains("delegate said:")
        })
        .map(text_of)
        .expect("parent commits the authenticated child result");
    assert!(reply.contains("delegate said: Echo: do the research"));
    peer_task.abort();
}
