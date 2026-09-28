use super::*;
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor};

struct NoLlm;
#[async_trait]
impl LlmExecutor for NoLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        unreachable!("a capability probe never calls the model")
    }
}

/// Pends forever once the probe reaches the handshake, which is strictly after
/// the environment and the agent process exist. Cancelling there is the exact
/// shape the defect describes: both resources stand, and the task that knew
/// their name is gone.
struct StalledHandshake;
#[async_trait]
impl AcpCapabilityHandshake for StalledHandshake {
    async fn negotiate(
        &self,
        _channel: &mut dyn awaken_agent_channel::AgentChannel,
        _config: &AcpCapabilityProbeConfig,
    ) -> Result<NegotiatedAcpCapabilities, String> {
        std::future::pending().await
    }
}

struct ReadyHandshake;
#[async_trait]
impl AcpCapabilityHandshake for ReadyHandshake {
    async fn negotiate(
        &self,
        _channel: &mut dyn awaken_agent_channel::AgentChannel,
        _config: &AcpCapabilityProbeConfig,
    ) -> Result<NegotiatedAcpCapabilities, String> {
        Ok(NegotiatedAcpCapabilities {
            protocol_version: "1".into(),
            load_session: false,
            prompt_image: false,
            prompt_audio: false,
            prompt_embedded_context: false,
            mcp_http: false,
            mcp_sse: false,
            session_list: false,
            modes: Vec::new(),
            config_options: Vec::new(),
        })
    }
}

/// A Workdir Session provider rooted in `base`. The Workdir tier is the one
/// that needs no daemon, and it stamps the scope on the resource the same way
/// every other tier does: the environment *is* the directory named by it.
fn probe_negotiator(
    base: &Path,
    owner: &str,
    handshake: Arc<dyn AcpCapabilityHandshake>,
) -> SessionAcpCapabilityNegotiator {
    let mut host = SharedHost::new(Arc::new(NoLlm), "stub");
    host.local_workspace = owner.to_string();
    host.env.session_provider =
        crate::session_environment::SessionEnvironmentProvider::workdir(base);
    SessionAcpCapabilityNegotiator::new(Arc::new(host), Duration::from_secs(30), handshake)
}

fn probe_argv() -> Vec<String> {
    vec!["/bin/cat".to_string()]
}

fn probe_cwd() -> &'static Path {
    Path::new(pc::WorkspaceLayout::OUTPUTS_ROOT)
}

/// A cancelled probe must leave something every tier's reconciliation can
/// select on, and a completed one must leave nothing at all.
///
/// Causes: C1 the probe is cancelled after its environment exists; C2 the probe
/// completes and disposes; C3 a later probe supplies the same owner/argv/auth;
/// C4 it supplies a different one.
/// Effects: E1 the stranded environment stands at the scope
/// [`SessionAcpCapabilityNegotiator::probe_scope`] recomputes from inputs that
/// outlive the task; E2 the in-process ledger names that scope; E3 nothing is
/// stranded and the ledger is empty; E4 the later probe addresses the same
/// environment rather than creating a second orphan beside it; E5 it addresses
/// a different one.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect      |
/// | R1   | T  | F  | F  | F  | E1 + E2     |
/// | R2   | T  | F  | T  | F  | E4          |
/// | R3   | T  | F  | F  | T  | E5          |
/// | R4   | F  | T  | F  | F  | E3          |
#[tokio::test]
async fn a_cancelled_probe_strands_an_environment_that_its_own_scope_still_names() {
    let argv = probe_argv();

    // R1 — cancel while the handshake stalls. `timeout` dropping the future is
    // the ordinary cancellation; nothing in the probe gets to run a tail.
    let stranded_base = tempfile::tempdir().unwrap();
    let negotiator = probe_negotiator(stranded_base.path(), "ws-alpha", Arc::new(StalledHandshake));
    let cancelled = tokio::time::timeout(
        Duration::from_millis(1500),
        AcpCapabilityNegotiator::negotiate(&negotiator, &argv, probe_cwd(), None),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "the stalled handshake must leave the probe unsettled, not finish it"
    );

    let scope = SessionAcpCapabilityNegotiator::probe_scope("ws-alpha", &argv, None);
    assert!(
        stranded_base.path().join(&scope).is_dir(),
        "E1 the stranded environment must stand at the derived scope"
    );
    assert!(
        negotiator.stranded.contains(&scope),
        "E2 the claim's retention must name it"
    );
    assert!(
        !negotiator
            .stranded
            .contains(&SessionAcpCapabilityNegotiator::probe_scope(
                "ws-beta", &argv, None
            )),
        "E2 the retention names the probe that ran, not every scope"
    );

    // R2 — the name survives the task that made it. A second negotiator, built
    // from nothing but the same owner and inputs, addresses that exact
    // environment; a random-UUID scope could only ever create a second orphan
    // beside it and never mention the first again.
    let later = probe_negotiator(stranded_base.path(), "ws-alpha", Arc::new(ReadyHandshake));
    let refusal = AcpCapabilityNegotiator::negotiate(&later, &argv, probe_cwd(), None)
        .await
        .expect_err("E4 the stranded environment occupies the scope");
    assert!(
        refusal.contains(&scope),
        "E4 the refusal must name the scope a reconciliation selects on: {refusal}"
    );

    // R3 — a different runtime owner, a different CLI or a different auth
    // method is a different effect and must never adopt this one.
    for (owner, other_argv, auth) in [
        ("ws-beta", argv.clone(), None),
        ("ws-alpha", vec!["/bin/echo".to_string()], None),
        ("ws-alpha", argv.clone(), Some("oauth")),
    ] {
        assert_ne!(
            SessionAcpCapabilityNegotiator::probe_scope(owner, &other_argv, auth),
            scope,
            "E5 {owner}/{other_argv:?}/{auth:?} must not collide with the probe scope"
        );
    }

    // R4 — a probe that completes disposes its own environment, so a settled
    // claim never leaves a false strand behind for an operator to chase.
    let clean_base = tempfile::tempdir().unwrap();
    let clean = probe_negotiator(clean_base.path(), "ws-alpha", Arc::new(ReadyHandshake));
    AcpCapabilityNegotiator::negotiate(&clean, &argv, probe_cwd(), None)
        .await
        .expect("E3 a completed probe negotiates");
    assert!(
        !clean_base.path().join(&scope).exists(),
        "E3 a completed probe disposes its environment"
    );
    assert!(
        !clean.stranded.contains(&scope),
        "E3 a settled claim records no strand"
    );
}

/// The scope is a pure function: same inputs, same name, in any process.
#[test]
fn the_probe_scope_is_a_stable_function_of_owner_and_probe_inputs() {
    let argv = probe_argv();
    let scope = SessionAcpCapabilityNegotiator::probe_scope("ws-alpha", &argv, Some("oauth"));
    assert_eq!(
        scope,
        SessionAcpCapabilityNegotiator::probe_scope("ws-alpha", &argv, Some("oauth")),
        "a repeat probe must land on the same environment"
    );
    // Container labels, Kubernetes object names and filesystem leaves all have
    // to accept it, so keep it to the intersection of what they allow.
    assert!(
        scope.len() < 63
            && scope
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "the scope must be admissible as a label, an object name and a path leaf: {scope}"
    );
}
