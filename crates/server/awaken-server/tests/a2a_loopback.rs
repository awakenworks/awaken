//! A delegated A2A Agent uses the same publication, child Run, attempt registry,
//! credential admission and commit path as a directly admitted remote Run.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_protocol_a2a::{Response, Transport};
use awaken_run_executor_a2a::{A2aRunExecutor, TransportResolver};
use awaken_runtime_contract::StaticPublishedAgentSnapshots;
use awaken_runtime_contract::agent_bindings::AgentBindings;
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
use awaken_scenario_host::{DelegatingModel, EchoModel, build_router};
use awaken_server::SharedHost;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
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
            delegate_ids: vec![AgentId("researcher".into())],
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
