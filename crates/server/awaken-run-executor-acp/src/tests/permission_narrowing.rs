//! New Run-ceiling boundaries share the original ACP fixtures and policies.
use super::*;

pub(super) struct FixedPolicy(pub(super) ToolPermissionVerdict);
#[async_trait]
impl ToolPermissionPolicy for FixedPolicy {
    async fn evaluate(&self, _call: &ToolCall) -> ToolPermissionRuling {
        ToolPermissionRuling::run_narrowing(self.0.clone())
    }
}

pub(super) struct FixedResolver(pub(super) PermissionVerdict);
#[async_trait]
impl PermissionResolver for FixedResolver {
    async fn resolve(&self, _ask: &PermissionAsk) -> PermissionVerdict {
        self.0.clone()
    }
}

#[tokio::test]
async fn acp_tool_ceiling_is_recorded_and_unsupported_no_tools_never_launches() {
    // C1 current Configured/current NoTools/legacy missing input; C2 opaque
    // tool-transparent ACP has no negotiated NoTools profile. R1 Configured
    // keeps the original open/prompt/home lifecycle and records its input.
    // R2 NoTools and R3 legacy unknown -> CapabilityBound before home restore,
    // process open, prompt, poll or tool grant. R3 never backfills provenance.
    use awaken_runtime_contract::permission::{RunToolCapability, ToolCapabilityNarrowing};
    for mode in 0..3 {
        let (executor, opens, prompts, polls) = counting_executor();
        let home = Arc::new(RecordingSessionHome::default());
        let executor = executor.with_session_home(home.clone());
        let original = if mode == 1 {
            activation().without_tools()
        } else {
            activation()
        };
        let run_id = original.run_id.clone();
        let thread_id = original.thread_id.clone();
        let committed = Arc::new(RecordingCoordinator::default());
        if mode == 2 {
            committed
                .commit(ThreadCommit::assemble(
                    thread_id.clone(),
                    RunDisposition::running(run_id.clone()),
                    true,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ))
                .await
                .unwrap();
        }
        let state = executor
            .execute(
                original,
                RuntimeRunContext::new().with_store(committed.clone()),
            )
            .await
            .unwrap();
        if mode == 0 {
            assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
            assert_eq!(opens.load(Ordering::SeqCst), 1);
            assert_eq!(prompts.load(Ordering::SeqCst), 1);
            assert!(polls.load(Ordering::SeqCst) > 0);
            assert_eq!(home.calls.lock().unwrap().len(), 2);
        } else {
            assert_eq!(
                state,
                RunState::Ended(EndCause::Error(Failure::CapabilityBound))
            );
            assert_eq!(opens.load(Ordering::SeqCst), 0);
            assert_eq!(prompts.load(Ordering::SeqCst), 0);
            assert_eq!(polls.load(Ordering::SeqCst), 0);
            assert!(home.calls.lock().unwrap().is_empty());
        }
        let commands = awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView::committed_state(
            committed.as_ref(),
            &thread_id,
        );
        assert_eq!(
            RunToolCapability::recorded(&commands, &run_id).unwrap(),
            match mode {
                0 => Some(ToolCapabilityNarrowing::Configured),
                1 => Some(ToolCapabilityNarrowing::DenyAll),
                _ => None,
            }
        );
    }
}

async fn assert_resumed_restrictions(
    base_allow: &dyn PermissionResolver,
    policies: [Option<&dyn ToolPermissionPolicy>; 4],
    ask: &PermissionAsk,
) {
    let audit = PermissionAudit::default();
    // SR4: exact old Allow/Deny cannot precede a current restriction. Causes:
    // capability C/D x no/allow/deny/ask Run policy x old human allow/deny.
    // A restriction neither consumes the old one-shot grant nor notifies its
    // observer. With no refusal, the original exact decision is consumed once.
    struct GrantCounter(AtomicUsize);
    impl PermissionGrantObserver for GrantCounter {
        fn on_allow(&self, _: &ToolCall) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    for denied in [false, true] {
        for (policy, selected_policy) in policies.iter().copied().enumerate() {
            for allow in [false, true] {
                let observer = GrantCounter(AtomicUsize::new(0));
                let decision = PermissionResume {
                    call_id: "old-wire-id".into(),
                    tool_id: ask.tool.clone(),
                    arguments: ask.arguments.clone(),
                    allow,
                };
                let resumed =
                    ResumedPermissionResolver::new(base_allow, &decision, Some(&observer));
                let narrowed = NarrowedPermissionResolver {
                    base: &resumed,
                    narrowing: selected_policy,
                    capability: if denied {
                        awaken_runtime_contract::permission::ToolCapabilityNarrowing::DenyAll
                    } else {
                        awaken_runtime_contract::permission::ToolCapabilityNarrowing::Configured
                    },
                    audit: &audit,
                };
                let verdict = narrowed.resolve(ask).await;
                let restricted = denied || policy >= 2;
                assert_eq!(
                    verdict,
                    if denied || policy == 2 {
                        PermissionVerdict::Deny
                    } else if policy == 3 {
                        PermissionVerdict::Await {
                            correlation_id: "current-restriction".into(),
                        }
                    } else if allow {
                        PermissionVerdict::Allow
                    } else {
                        PermissionVerdict::Deny
                    }
                );
                assert_eq!(resumed.unconsumed_decision().is_some(), restricted);
                assert_eq!(
                    observer.0.load(Ordering::SeqCst),
                    usize::from(allow && !restricted)
                );
            }
        }
    }
}

#[tokio::test]
async fn resumed_acp_permissions_intersect_the_current_run_ceiling() {
    // The helper below owns SR4's cause/effect table and observer assertions;
    // original ACP tests reuse these same policy/resolver fixtures unchanged.
    let allow = FixedPolicy(ToolPermissionVerdict::Allow);
    let deny = awaken_runtime_contract::permission::DenyAllTools::new("restricted Run");
    let ask_policy = FixedPolicy(ToolPermissionVerdict::RequireConfirmation {
        correlation_id: "current-restriction".into(),
    });
    let base = FixedResolver(PermissionVerdict::Allow);
    let ask = PermissionAsk {
        tool: "bash".into(),
        call_id: "tool-1".into(),
        arguments: serde_json::json!({"cmd":"echo unsafe"}),
    };
    assert_resumed_restrictions(
        &base,
        [None, Some(&allow), Some(&deny), Some(&ask_policy)],
        &ask,
    )
    .await;
}

#[tokio::test]
async fn legacy_missing_ceiling_cannot_reach_a_new_restricted_source_profile() {
    // Causes: C1 a durable Running row lacks original RuntimeInputs; C2 a newly
    // installed source would accept the effective DenyAll; C3 home/open are
    // effect boundaries. Effect: legacy unknown remains CapabilityBound before
    // even consulting C2, with zero restore/open/prompt/poll and no provenance
    // backfill. Rule U1=C1+C2=>refuse before C3. The accepting preflight is only
    // a mutation-sensitive negative-control input; no unproven process runs.
    struct NewProfile {
        inner: Arc<dyn AgentChannelSource>,
        checks: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl AgentChannelSource for NewProfile {
        fn check_tool_narrowing(
            &self,
            _narrowing: &awaken_runtime_contract::permission::ToolCapabilityNarrowing,
        ) -> std::result::Result<(), OpenError> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn open(
            &self,
            activation: &RunActivation,
            context: &RuntimeRunContext,
            door: ChannelOpenDoor,
        ) -> std::result::Result<AgentSession, OpenError> {
            self.inner.open(activation, context, door).await
        }
    }
    let (original_executor, opens, prompts, polls) = counting_executor();
    let checks = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(NewProfile {
        inner: original_executor.source.clone(),
        checks: checks.clone(),
    });
    let home = Arc::new(RecordingSessionHome::default());
    let executor = AcpRunExecutor::new(source).with_session_home(home.clone());
    let activation = activation();
    let run_id = activation.run_id.clone();
    let thread_id = activation.thread_id.clone();
    let committed = Arc::new(RecordingCoordinator::default());
    committed
        .commit(ThreadCommit::assemble(
            thread_id.clone(),
            RunDisposition::running(run_id.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
    let state = executor
        .execute(
            activation,
            RuntimeRunContext::new().with_store(committed.clone()),
        )
        .await
        .unwrap();
    assert_eq!(
        state,
        RunState::Ended(EndCause::Error(Failure::CapabilityBound))
    );
    assert_eq!(
        checks.load(Ordering::SeqCst),
        0,
        "U1 no new profile can replace missing original evidence"
    );
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    assert_eq!(prompts.load(Ordering::SeqCst), 0);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(home.calls.lock().unwrap().is_empty());
    let commands = awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView::committed_state(committed.as_ref(), &thread_id);
    assert_eq!(
        awaken_runtime_contract::permission::RunToolCapability::recorded(&commands, &run_id)
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn restricted_session_projection_cannot_restore_configured_mcp_routes() {
    // C1 a source/session or Host carries configured routes; C2 current Run
    // ceiling is Configured or DenyAll. E1 Configured keeps the exact Host
    // override; E2 DenyAll yields no effective routes, including after reopen.
    // R1=C1+Configured=>E1; R2=C1+DenyAll=>E2. This does not authorize an opaque
    // source: its independent preflight must have succeeded before open.
    let (executor, _, _, _) = counting_executor();
    let route = SessionMcpServer {
        name: "configured".into(),
        command: None,
        args: Vec::new(),
        url: Some("http://127.0.0.1:1".into()),
        auth: None,
    };
    let executor = executor.for_session_servers(
        Arc::new(FixedPolicy(ToolPermissionVerdict::Allow)),
        std::slice::from_ref(&route),
    );
    for capability in [
        awaken_runtime_contract::permission::ToolCapabilityNarrowing::Configured,
        awaken_runtime_contract::permission::ToolCapabilityNarrowing::DenyAll,
    ] {
        let (channel, _peer) = tokio::io::duplex(16);
        let mut session = fixture_session(
            Box::new(channel),
            Arc::new(CountingProcess {
                polls: Arc::new(AtomicUsize::new(0)),
                observation: Arc::new(AtomicUsize::new(0)),
            }),
            Codec::Newline,
            None,
        );
        session.mcp_session_servers = vec![route.clone()];
        executor.apply_session_mcp_projection(&mut session, &capability);
        assert_eq!(
            session.mcp_session_servers,
            if capability.is_configured() {
                vec![route.clone()]
            } else {
                Vec::new()
            }
        );
    }
}
