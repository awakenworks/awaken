//! T33-C03 (cross-adapter consistency), local Native row: the same injected
//! upstream fault driven through the production `Runtime` loop in-process.
//!
//! The fault fixture is the one shared by every adapter row of this scenario
//! (HTTP Worker: `awaken-dispatch-http/tests/cross_adapter_fault_http_worker.rs`,
//! ACP: `awaken-run-executor-acp/tests/cross_adapter_fault_acp.rs`, MCP:
//! `awaken-protocol-mcp/tests/cross_adapter_fault_mcp.rs`, Hand relay:
//! `awaken-tool-relay/tests/cross_adapter_fault_hand_relay.rs`):
//!
//! | fault | upstream answer                                   | authority                         | uncertain | retry hint            |
//! | F1    | 529 Overloaded, Retry-After 7s, proved undispatched | `Error(Inference{"overloaded"})` | no        | retryable class       |
//! | F2    | the same 529 without dispatch evidence (Unknown)  | `Indeterminate`                   | yes       | never repeated        |
//!
//! Oracles: O0 the original authority, uncertainty and retry hint are kept in
//! the committed terminal; O1 the fault never becomes a successful end.
//! Negative control (executed): the same checker is run against three adapter
//! mutants of the real committed terminal (silent success, authority collapse
//! to an opaque code, uncertainty flip) and must reject every one of them.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::{RetryPolicy, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, Error as LlmError, LlmExecutor};
use awaken_runtime_contract::model_operation::{ModelExecutionEvidence, NoDispatchProof};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryCommitCoordinator;

/// The provider's own account of the fault, including its retry hint.
const UPSTREAM_DETAIL: &str = "upstream 529 overloaded_error (retry-after 7s)";
const RETRY_AFTER: Duration = Duration::from_secs(7);
/// The native codes whose class `Error::is_retryable` reports as retryable.
const RETRYABLE_CODES: [&str; 4] = ["provider_error", "rate_limited", "overloaded", "timeout"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InjectedFault {
    /// F1: certain, retryable, carries a Retry-After hint.
    Overloaded,
    /// F2: the same answer with no proof the request stayed undispatched.
    UnknownOutcome,
}

impl InjectedFault {
    const ALL: [Self; 2] = [Self::Overloaded, Self::UnknownOutcome];

    fn upstream_error(self) -> LlmError {
        let overloaded = LlmError::Overloaded {
            message: UPSTREAM_DETAIL.to_string(),
            retry_after: Some(RETRY_AFTER),
        };
        match self {
            Self::Overloaded => overloaded.with_execution_evidence(
                ModelExecutionEvidence::NotDispatched(NoDispatchProof::request_projection()),
            ),
            // An unannotated provider error carries Unknown execution evidence.
            Self::UnknownOutcome => overloaded,
        }
    }

    fn expected(self) -> Classification {
        match self {
            Self::Overloaded => Classification {
                authority: "overloaded".to_string(),
                uncertain: false,
                retryable: true,
                success: false,
            },
            Self::UnknownOutcome => Classification {
                authority: "indeterminate".to_string(),
                uncertain: true,
                retryable: false,
                success: false,
            },
        }
    }
}

/// The neutral classification every adapter must carry for the injected fault.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Classification {
    authority: String,
    uncertain: bool,
    retryable: bool,
    success: bool,
}

fn classify(state: &RunState) -> Classification {
    match state {
        RunState::Ended(EndCause::Error(failure)) => Classification {
            authority: failure.code().to_string(),
            uncertain: false,
            retryable: RETRYABLE_CODES.contains(&failure.code()),
            success: false,
        },
        RunState::Ended(EndCause::Indeterminate) => Classification {
            authority: "indeterminate".to_string(),
            uncertain: true,
            retryable: false,
            success: false,
        },
        RunState::Ended(EndCause::NaturalEnd) => Classification {
            authority: "natural_end".to_string(),
            uncertain: false,
            retryable: false,
            success: true,
        },
        other => Classification {
            authority: format!("{other:?}"),
            uncertain: false,
            retryable: false,
            success: false,
        },
    }
}

/// The per-adapter oracle check (O1 first, then O0).
fn check(
    adapter: &str,
    expected: &Classification,
    observed: &Classification,
) -> Result<(), String> {
    if observed.success {
        return Err(format!(
            "O1 {adapter}: the injected fault surfaced as success: {observed:?}"
        ));
    }
    if observed != expected {
        return Err(format!(
            "O0 {adapter}: expected {expected:?}, observed {observed:?}"
        ));
    }
    Ok(())
}

/// A deterministic provider that answers every call with the injected fault.
struct FaultyUpstream {
    fault: InjectedFault,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmExecutor for FaultyUpstream {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(self.fault.upstream_error())
    }
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("t33-c03-local".to_string());
    RunActivation::new(
        RunId("t33-c03-run".to_string()),
        ThreadId("t33-c03-thread".to_string()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("t33-c03-snapshot".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("t33-c03-agent".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: Some(4),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::parse("p", "m", "b").expect("valid test binding"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        vec![Message {
            id: MessageId("t33-c03-user".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
    )
}

/// Drive one fault through the production loop and return the returned state,
/// the single committed terminal state, and the upstream invocation count.
async fn run_local(fault: InjectedFault) -> (RunState, RunState, usize) {
    let calls = Arc::new(AtomicUsize::new(0));
    let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    // An explicit zero disables retries, so the terminal is the first answer
    // and the test needs no Retry-After sleep.
    let runtime = Runtime::new()
        .with_llm(Arc::new(FaultyUpstream {
            fault,
            calls: calls.clone(),
        }))
        .with_retry_policy(RetryPolicy {
            max_retries: 0,
            ..Default::default()
        });
    let returned = runtime
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(durable.clone()),
        )
        .await
        .expect("the runtime commits a terminal for the injected fault");
    let terminals: Vec<RunState> = durable
        .committed()
        .run_facts
        .iter()
        .filter(|fact| matches!(fact.state, RunState::Ended(_)))
        .map(|fact| fact.state.clone())
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "{fault:?}: exactly one committed terminal"
    );
    (returned, terminals[0].clone(), calls.load(Ordering::SeqCst))
}

#[tokio::test]
async fn local_runtime_commits_each_injected_upstream_fault_with_its_original_classification() {
    for fault in InjectedFault::ALL {
        let (returned, committed, calls) = run_local(fault).await;
        assert_eq!(
            committed, returned,
            "{fault:?}: the committed authority is the returned one"
        );
        check("local", &fault.expected(), &classify(&committed))
            .unwrap_or_else(|violation| panic!("{fault:?}: {violation}"));
        assert_eq!(calls, 1, "{fault:?}: no hidden repeat of the upstream call");
        if let RunState::Ended(EndCause::Error(failure)) = &committed {
            assert!(
                failure.message().contains(UPSTREAM_DETAIL),
                "{fault:?}: the provider's account and hint survive: {}",
                failure.message()
            );
        }
    }
}

fn silent_success(_state: &RunState) -> RunState {
    RunState::Ended(EndCause::NaturalEnd)
}

fn authority_collapse(state: &RunState) -> RunState {
    let message = match state {
        RunState::Ended(EndCause::Error(failure)) => failure.message(),
        _ => "collapsed".to_string(),
    };
    RunState::Ended(EndCause::Error(Failure::Inference {
        code: "acp_failure".to_string(),
        message,
    }))
}

fn uncertainty_flip(state: &RunState) -> RunState {
    match state {
        RunState::Ended(EndCause::Indeterminate) => {
            RunState::Ended(EndCause::Error(Failure::Inference {
                code: "provider_error".to_string(),
                message: "certain failure".to_string(),
            }))
        }
        _ => RunState::Ended(EndCause::Indeterminate),
    }
}

/// One named adapter mutant of the committed terminal state.
type TerminalMutant = (&'static str, fn(&RunState) -> RunState);

#[tokio::test]
async fn local_fault_checker_rejects_adapter_mutants_of_the_real_committed_terminal() {
    let mutants: [TerminalMutant; 3] = [
        ("silent success", silent_success),
        ("authority collapse", authority_collapse),
        ("uncertainty flip", uncertainty_flip),
    ];
    for fault in InjectedFault::ALL {
        let (_, committed, _) = run_local(fault).await;
        assert!(
            check("local", &fault.expected(), &classify(&committed)).is_ok(),
            "{fault:?}: the unmutated projection passes, so rejection below is meaningful"
        );
        for (name, mutate) in mutants {
            let mutated = mutate(&committed);
            assert_ne!(
                mutated, committed,
                "{fault:?}/{name}: the mutant changes the projection"
            );
            let verdict = check(name, &fault.expected(), &classify(&mutated));
            assert!(
                verdict.is_err(),
                "{fault:?}/{name}: the checker must detect the downgraded projection {mutated:?}"
            );
        }
        let violation = check(
            "silent success",
            &fault.expected(),
            &classify(&silent_success(&committed)),
        )
        .expect_err("silent success is detected");
        assert!(violation.starts_with("O1"), "{fault:?}: {violation}");
    }
}
