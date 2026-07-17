//! `SharedHost::remote_agent_card` — outbound discovery of a registered remote
//! delegate through the neutral [`RemoteDelegate`] port. The wire (which route, how the
//! card parses) lives in the adapter (`awaken-run-executor-a2a`) and is pinned there;
//! this pins the HOST adapter: a card fetch routes to the registered remote's port
//! (happy path), an unregistered agent id is fail-closed (bad request, never a silent
//! empty card), and a remote error surfaces as an error — all named by `agent_id`, with
//! no protocol type on host state.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::agent_resolver::{AgentError, AgentStep, RemoteDelegate};
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult};
use awaken_runtime_host::SharedHost;
use serde_json::{Value, json};

struct NoLlm;
#[async_trait]
impl LlmExecutor for NoLlm {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("delegation discovery never infers")
    }
}

/// A neutral in-process remote delegate: records the `agent_id`s whose card was asked
/// for and replies with a queued card (or an error), so the host adapter's routing
/// (which delegate, fail-closed) is asserted without any protocol type.
struct MockRemote {
    seen: Mutex<Vec<String>>,
    reply: Result<Value, String>,
}

#[async_trait]
impl RemoteDelegate for MockRemote {
    async fn run(
        &self,
        _agent_id: &str,
        _input: &str,
        _cancellation: Option<&CancellationToken>,
    ) -> Result<AgentStep, AgentError> {
        unreachable!("card discovery never runs a turn")
    }

    async fn card(&self, agent_id: &str) -> Result<Value, AgentError> {
        self.seen.lock().unwrap().push(agent_id.to_string());
        self.reply.clone().map_err(AgentError::new)
    }
}

#[tokio::test]
async fn remote_agent_card_fetches_through_the_registered_delegate() {
    let remote = Arc::new(MockRemote {
        seen: Mutex::new(Vec::new()),
        reply: Ok(json!({ "name": "researcher", "protocolVersion": "1.0" })),
    });
    let host =
        SharedHost::new(Arc::new(NoLlm), "test").with_remote_delegate("researcher", remote.clone());

    let card = host
        .remote_agent_card("researcher")
        .await
        .expect("the registered remote answers a card");
    assert_eq!(card["name"], "researcher");
    assert_eq!(card["protocolVersion"], "1.0");
    // The fetch routed to the registered delegate, keyed by its agent id.
    let seen = remote.seen.lock().unwrap();
    assert_eq!(seen.as_slice(), &["researcher".to_string()]);
}

#[tokio::test]
async fn an_unregistered_remote_is_fail_closed() {
    // A host with a remote registered under a DIFFERENT id must not answer for an
    // agent it does not route — it is a bad request, not a silent/empty card.
    let remote = Arc::new(MockRemote {
        seen: Mutex::new(Vec::new()),
        reply: Ok(json!({})),
    });
    let host =
        SharedHost::new(Arc::new(NoLlm), "test").with_remote_delegate("known", remote.clone());

    let err = host
        .remote_agent_card("stranger")
        .await
        .expect_err("an unregistered remote is refused");
    assert!(
        err.to_string().contains("stranger"),
        "the error names the missing remote: {err}"
    );
    // A local-only host (no remotes at all) is likewise fail-closed.
    let local = SharedHost::new(Arc::new(NoLlm), "test");
    assert!(local.remote_agent_card("anyone").await.is_err());
    // The unregistered lookup never reached the delegate.
    assert!(remote.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_remote_that_errors_surfaces_an_internal_error() {
    // The delegate is registered but its card fetch fails: the host maps it to an
    // error rather than a bogus card.
    let remote = Arc::new(MockRemote {
        seen: Mutex::new(Vec::new()),
        reply: Err("boom".to_string()),
    });
    let host =
        SharedHost::new(Arc::new(NoLlm), "test").with_remote_delegate("flaky", remote.clone());
    assert!(host.remote_agent_card("flaky").await.is_err());
    // It did route to the delegate (the failure is from the reply, not a missing route).
    assert_eq!(remote.seen.lock().unwrap().len(), 1);
}
