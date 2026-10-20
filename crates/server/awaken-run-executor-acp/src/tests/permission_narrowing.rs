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
