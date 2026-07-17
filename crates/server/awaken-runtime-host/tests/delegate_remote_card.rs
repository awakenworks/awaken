//! `SharedHost::remote_agent_card` — outbound A2A discovery of a registered remote
//! delegate. The A2A *wire* (routes, parsing) is proven in `awaken-protocol-a2a`;
//! this pins the host adapter: a card fetch routes through the registered remote's
//! transport (happy path), and an unregistered agent id is fail-closed (bad request),
//! never a silent empty card.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_protocol_a2a::client::{Response, Transport};
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult};
use awaken_runtime_host::SharedHost;

struct NoLlm;
#[async_trait]
impl LlmExecutor for NoLlm {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("delegation discovery never infers")
    }
}

/// An in-process A2A transport: records the requests it saw and replies with a
/// queued body, so the host adapter's routing (which transport, which path) is
/// asserted without a socket.
struct MockTransport {
    seen: Mutex<Vec<(String, String)>>,
    status: u16,
    reply: String,
}

#[async_trait]
impl Transport for MockTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        _body: Option<Vec<u8>>,
    ) -> Result<Response, String> {
        self.seen
            .lock()
            .unwrap()
            .push((method.to_string(), path.to_string()));
        Ok(Response::new(self.status, self.reply.clone().into_bytes()))
    }
}

#[tokio::test]
async fn remote_agent_card_fetches_through_the_registered_transport() {
    let transport = Arc::new(MockTransport {
        seen: Mutex::new(Vec::new()),
        status: 200,
        reply: r#"{"name":"researcher","description":"a remote agent","version":"1.2.3","protocolVersion":"1.0","capabilities":{"streaming":false,"pushNotifications":false}}"#
            .to_string(),
    });
    let host =
        SharedHost::new(Arc::new(NoLlm), "test").with_remote_a2a("researcher", transport.clone());

    let card = host
        .remote_agent_card("researcher")
        .await
        .expect("the registered remote answers a card");
    assert_eq!(card.name, "researcher");
    assert_eq!(card.protocol_version, "1.0");
    // The fetch went out over the registered transport on the A2A agent-card route.
    let seen = transport.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "GET");
    assert_eq!(seen[0].1, "/v1/a2a/agent-card");
}

#[tokio::test]
async fn an_unregistered_remote_is_fail_closed() {
    // A host with a remote registered under a DIFFERENT id must not answer for an
    // agent it does not route — it is a bad request, not a silent/empty card.
    let transport = Arc::new(MockTransport {
        seen: Mutex::new(Vec::new()),
        status: 200,
        reply: "{}".to_string(),
    });
    let host = SharedHost::new(Arc::new(NoLlm), "test").with_remote_a2a("known", transport.clone());

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
    // The unregistered lookup never reached out over the wire.
    assert!(transport.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_remote_that_errors_surfaces_an_internal_error() {
    // The transport is registered, but the remote replies 500: the host maps a
    // non-2xx card fetch to an error rather than a bogus card.
    let transport = Arc::new(MockTransport {
        seen: Mutex::new(Vec::new()),
        status: 500,
        reply: r#"{"error":{"message":"boom"}}"#.to_string(),
    });
    let host = SharedHost::new(Arc::new(NoLlm), "test").with_remote_a2a("flaky", transport.clone());
    assert!(host.remote_agent_card("flaky").await.is_err());
    // It did route to the transport (fail is from the reply, not a missing route).
    assert_eq!(transport.seen.lock().unwrap().len(), 1);
}
