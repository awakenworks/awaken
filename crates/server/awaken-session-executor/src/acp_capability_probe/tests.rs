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

struct PairedHandshake(Arc<tokio::sync::Barrier>);
#[async_trait]
impl AcpCapabilityHandshake for PairedHandshake {
    async fn negotiate(
        &self,
        channel: &mut dyn awaken_agent_channel::AgentChannel,
        config: &AcpCapabilityProbeConfig,
    ) -> Result<NegotiatedAcpCapabilities, String> {
        self.0.wait().await;
        ReadyHandshake.negotiate(channel, config).await
    }
}

#[tokio::test]
async fn shared_workspace_worker_incarnations_probe_independently_and_blank_owner_has_no_effect() {
    // C1 shared platform Workspace/provider/argv; C2 distinct registry-issued
    // runtime incarnations. R1 C1+C2 -> concurrent independent realizations,
    // both exact disposals, no name collision. R2 missing owner -> reject
    // before create. The barrier requires both actual processes to reach
    // handshake; changing back to shared Workspace naming fails this row.
    let base = tempfile::tempdir().unwrap();
    let handshake = Arc::new(PairedHandshake(Arc::new(tokio::sync::Barrier::new(2))));
    let mut first = probe_negotiator(base.path(), "shared-workspace", handshake.clone()).await;
    let mut second = probe_negotiator(base.path(), "shared-workspace", handshake).await;
    first.scope_owner = "worker-a/incarnation-1".into();
    second.scope_owner = "worker-b/incarnation-2".into();
    let argv = probe_argv();
    let (a, b) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            first.negotiate(&argv, probe_cwd(), None),
            second.negotiate(&argv, probe_cwd(), None),
        )
    })
    .await
    .expect("R1 both independent environments reached handshake");
    a.expect("R1 first disposed");
    b.expect("R1 second disposed");
    for owner in [&first.scope_owner, &second.scope_owner] {
        assert!(
            !base
                .path()
                .join(SessionAcpCapabilityNegotiator::probe_scope(
                    owner, &argv, None
                ))
                .exists()
        );
    }
    first.scope_owner.clear();
    assert!(
        first
            .negotiate(&argv, probe_cwd(), None)
            .await
            .unwrap_err()
            .contains("incarnation"),
        "R2"
    );
}

#[tokio::test]
async fn a_cancelled_owned_probe_can_be_reconciled_without_creating_a_second_scope() {
    // Causes: P1 create and prompt-free child started; P2 the caller is
    // cancelled during handshake; P3 a later owner repeats the exact inputs.
    // Effects: retain one discoverable realization, independently close its
    // old participants, retry on that same scope, then dispose it. R1 P1+P2+P3
    // must converge without a second orphan or a fabricated capability. A
    // foreign owner/argv/auth and unproved physical closure remain refusal rows
    // in the adjacent identity and provider tests.
    let base = tempfile::tempdir().unwrap();
    let argv = probe_argv();
    let original =
        probe_negotiator(base.path(), "probe-recovery", Arc::new(StalledHandshake)).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1500),
            AcpCapabilityNegotiator::negotiate(&original, &argv, probe_cwd(), None)
        )
        .await
        .is_err()
    );
    let scope = SessionAcpCapabilityNegotiator::probe_scope("probe-recovery", &argv, None);
    assert!(
        original
            .assembly
            .resources
            .memory
            .drain(Duration::from_secs(5))
            .await,
        "the original exact-handle cleanup must finish before a later Host can create"
    );
    assert!(
        !base.path().join(&scope).exists(),
        "cleanup physically disposed the original"
    );
    let later = probe_negotiator(base.path(), "probe-recovery", Arc::new(ReadyHandshake)).await;
    AcpCapabilityNegotiator::negotiate(&later, &argv, probe_cwd(), None)
        .await
        .expect("P1-P3 verified reconciliation must converge on the original scope");
    assert!(
        !base.path().join(&scope).exists(),
        "one scope fully disposed"
    );
}

/// A Workdir Session provider rooted in `base`. The Workdir tier is the one
/// that needs no daemon, and it stamps the scope on the resource the same way
/// every other tier does: the environment *is* the directory named by it.
async fn probe_negotiator(
    base: &Path,
    owner: &str,
    handshake: Arc<dyn AcpCapabilityHandshake>,
) -> SessionAcpCapabilityNegotiator {
    let mut dependencies = crate::HostDependencies::ephemeral(Arc::new(NoLlm), "stub")
        .await
        .with_local_workspace(owner);
    dependencies.env.session_provider =
        awaken_session_environment::SandboxEnvironmentProvider::workdir(base)
            .into_session_provider();
    SessionAcpCapabilityNegotiator::new(
        (dependencies)
            .build()
            .expect("validated Host dependencies")
            .run_assembly()
            .clone(),
        owner.to_owned(),
        Duration::from_secs(30),
        handshake,
    )
}

fn probe_argv() -> Vec<String> {
    // C1 requires a live, stdin-bound child before the injected handshake stalls.
    // The probe's identity uses these exact argv bytes on either host OS.
    #[cfg(windows)]
    {
        vec![
            std::env::var("ComSpec").expect("Windows command interpreter"),
            "/c".into(),
            "more".into(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec!["/bin/cat".into()]
    }
}

fn probe_cwd() -> &'static Path {
    Path::new(pc::WorkspaceLayout::OUTPUTS_ROOT)
}

#[tokio::test]
async fn probe_handshake_timeout_and_spawn_failure_dispose_without_capability_evidence() {
    // R1 exact create + handshake deadline -> error and exact disposal;
    // R2 exact create + failed exec -> error and exact disposal. Neither row
    // returns protocol capability evidence or leaves a false settled strand.
    for fail_spawn in [false, true] {
        let base = tempfile::tempdir().unwrap();
        let argv = if fail_spawn {
            vec!["/missing-acp-test-program".into()]
        } else {
            probe_argv()
        };
        let mut negotiator =
            probe_negotiator(base.path(), "probe-failure", Arc::new(StalledHandshake)).await;
        negotiator.timeout = Duration::from_millis(20);
        let scope = SessionAcpCapabilityNegotiator::probe_scope("probe-failure", &argv, None);
        let error = AcpCapabilityNegotiator::negotiate(&negotiator, &argv, probe_cwd(), None)
            .await
            .unwrap_err();
        assert!(
            error.contains(if fail_spawn {
                "spawn ACP"
            } else {
                "probe timed out"
            }),
            "{error}"
        );
        assert!(
            !base.path().join(&scope).exists(),
            "R1/R2 provider disposal"
        );
        assert!(
            !negotiator.stranded.contains(&scope),
            "R1/R2 no false strand"
        );
    }
}

/// Probe lifecycle decision table: R1 caller cancellation after create ->
/// exact-handle cleanup continues in the Host registry; R2 later Host after
/// proved disposal -> fresh create at the same scope; R3 foreign/process-loss
/// legacy tree -> refuse and preserve bytes; R4 normal handshake -> verified
/// capabilities only after disposal, with no stranded ledger entry. Different
/// owner, argv and auth are distinct discovery coordinates, never grants.
#[tokio::test]
async fn a_cancelled_probe_strands_an_environment_that_its_own_scope_still_names() {
    let argv = probe_argv();

    // R1: dropping the receiver cancels handshake; the owner still runs cleanup.
    let stranded_base = tempfile::tempdir().unwrap();
    let negotiator =
        probe_negotiator(stranded_base.path(), "ws-alpha", Arc::new(StalledHandshake)).await;
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
        negotiator
            .assembly
            .resources
            .memory
            .drain(Duration::from_secs(5))
            .await
    );
    assert!(
        !stranded_base.path().join(&scope).exists(),
        "receiver cancellation must finish exact-handle disposal"
    );
    assert!(
        !negotiator.stranded.contains(&scope),
        "verified disposal settles the strand"
    );
    assert!(
        !negotiator
            .stranded
            .contains(&SessionAcpCapabilityNegotiator::probe_scope(
                "ws-beta", &argv, None
            )),
        "E2 the retention names the probe that ran, not every scope"
    );

    // R2: only the actual disposal above permits fresh creation.
    let later = probe_negotiator(stranded_base.path(), "ws-alpha", Arc::new(ReadyHandshake)).await;
    AcpCapabilityNegotiator::negotiate(&later, &argv, probe_cwd(), None)
        .await
        .expect("E4 retry follows proved disposal, not adoption by name");
    // A process-loss/foreign legacy tree carries no transferable authority.
    // Same derived name still cannot grant adoption or destructive cleanup.
    std::fs::create_dir(stranded_base.path().join(&scope)).unwrap();
    std::fs::write(stranded_base.path().join(&scope).join("foreign"), "retain").unwrap();
    let refusal = AcpCapabilityNegotiator::negotiate(&later, &argv, probe_cwd(), None)
        .await
        .unwrap_err();
    assert!(refusal.contains(&scope));
    assert_eq!(
        std::fs::read_to_string(stranded_base.path().join(&scope).join("foreign")).unwrap(),
        "retain"
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
    let clean = probe_negotiator(clean_base.path(), "ws-alpha", Arc::new(ReadyHandshake)).await;
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
