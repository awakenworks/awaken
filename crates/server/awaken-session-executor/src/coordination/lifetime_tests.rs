//! Cached coordination capabilities borrow the original Session owner per call.

use super::test_support::RemoteControl;
use super::*;
use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
use awaken_dispatch_execution::SessionRunAdmissionObserver as _;
use std::time::Duration;

async fn remote_host(control: Arc<RemoteControl>) -> Arc<crate::ExecutorComponents> {
    let mut deps = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::NoModelConfiguredExecutor),
        "stub",
    )
    .await;
    deps.worker.session_coordination_control = Some(control);
    deps.build()
        .expect("normal Host composition with the existing coordination port")
}

fn claim() -> RunClaim {
    RunClaim {
        run_id: RunId("source-run".into()),
        owner: "original-worker".into(),
        epoch: 7,
    }
}

fn message() -> SessionAgentMessageCommand {
    SessionAgentMessageCommand {
        session_id: "session".into(),
        source_thread_id: ThreadId("session".into()),
        source_run_id: RunId("source-run".into()),
        source_call_id: "call".into(),
        operation_id: "operation".into(),
        target: SessionAgentTarget::Spawn {
            agent_id: "child".into(),
        },
        message: "message".into(),
    }
}

#[derive(Clone, Copy, Debug)]
enum Query {
    List,
    Send,
}

impl Query {
    async fn run(self, endpoint: &CoordinationEndpoint) -> Result<(), ToolError> {
        match self {
            Self::List => endpoint
                .list(&"session".into(), "source-run")
                .await
                .map(|_| ()),
            Self::Send => endpoint.send("source-run", message()).await.map(|_| ()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Send => "send",
        }
    }
}

#[tokio::test]
async fn remote_cached_consumers_release_original_installed_sessions() {
    // Causes: C1 a real resident coordination Context caches a remote model gate;
    // C2 coordinator, gate, claimed-gate factory and observer remain reachable;
    // C3 the final Host and caller Context owners drop, with no active request.
    // Rule R1=C1+C2+C3 -> both original installed Sessions and Context are released. Capture
    // that effect before fixture cleanup; no eviction may create the evidence.
    let host = remote_host(Arc::new(RemoteControl::default())).await;
    host.run_assembly
        .installed_sessions
        .install_run("session", |run| run.session_dispatch = true);
    let context = host
        .run_assembly
        .ctx_for_session_coordination("session", None)
        .await
        .expect("real coordination assembly");
    assert!(context.is_coordination_envelope() && !context.has_active_run());
    assert!(context.env.is_none() && context.attempt_context.model_request_gate.is_some());
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run("session", |run| run
                .context
                .as_ref()
                .is_some_and(|cached| Arc::ptr_eq(cached, &context)))
            .expect("installed Sessions")
            .unwrap()
    );
    assert!(host.dispatch.pool().is_none() && host.run_assembly.mcp.relay().is_none());
    let endpoint = host
        .run_assembly
        .capabilities
        .coordination_endpoint()
        .unwrap();
    assert!(matches!(endpoint, CoordinationEndpoint::Remote { .. }));
    let coordinator = HostAgentCoordinator::new(endpoint.clone());
    let gate = HostModelRequestGate::new(endpoint.clone(), "session");
    let factory = claimed_model_request_gate_factory(endpoint.clone(), "session");
    let observer = host
        .run_assembly
        .capabilities
        .dispatch_settlement_observer()
        .unwrap();
    let weak_host = Arc::downgrade(&host);
    let weak_installed_sessions = Arc::downgrade(&host.run_assembly.installed_sessions);
    let weak_context = Arc::downgrade(&context);
    drop(endpoint);
    drop(context);
    drop(host);
    assert!(
        weak_host.upgrade().is_none(),
        "the original Host was released"
    );
    let installed_sessions_released = weak_installed_sessions.upgrade().is_none();
    let context_released = weak_context.upgrade().is_none();
    if let Some(installed_sessions) = weak_installed_sessions.upgrade() {
        let detached = installed_sessions
            .modify_run("session", |run| run.context.take())
            .expect("installed Sessions");
        drop(detached);
    }
    drop((coordinator, gate, factory, observer));
    assert!(
        weak_installed_sessions.upgrade().is_none() && weak_context.upgrade().is_none(),
        "fixture cleanup releases any captured cycle"
    );
    assert!(
        installed_sessions_released && context_released,
        "cached remote consumers retained the original installed Sessions: slots_released={installed_sessions_released}, context_released={context_released}"
    );
}

#[tokio::test]
async fn current_claim_queries_fail_closed_after_original_owner_drops() {
    // Causes: C1 operation=list/send; C2 the original owner is gone; C3 its
    // current claim was valid. Rules R1/R2=C1+C2+C3 -> DefinitiveFailure before
    // transport entry, no resurrected installed Sessions. Claim validity cannot replace C2.
    for query in [Query::List, Query::Send] {
        let control = Arc::new(RemoteControl::default());
        let host = remote_host(control.clone()).await;
        host.run_assembly
            .installed_sessions
            .install_run("session", |run| run.dispatch_claim = Some(claim()));
        let endpoint = host
            .run_assembly
            .capabilities
            .coordination_endpoint()
            .unwrap();
        let weak = Arc::downgrade(&host.run_assembly.installed_sessions);
        drop(host);
        assert!(weak.upgrade().is_none());
        let error = query.run(&endpoint).await.unwrap_err();
        assert!(matches!(error, ToolError::DefinitiveFailure(_)));
        assert!(error.to_string().contains("owner is unavailable"));
        assert!(
            control.calls.lock().unwrap().is_empty(),
            "{query:?}: no remote I/O"
        );
    }
}

#[tokio::test]
async fn current_claim_queries_reject_missing_or_foreign_claim_before_control_io() {
    // Causes: C1 operation=list/send; C2 original owner is live; C3 current
    // claim=missing/foreign Run. Rules R3-R6=C1+C2+C3 -> original definitive
    // missing/foreign-claim error, zero transport calls. A matching claim's
    // success/error/cancellation effects are covered by the active-query table.
    for query in [Query::List, Query::Send] {
        for foreign in [false, true] {
            let control = Arc::new(RemoteControl::default());
            let host = remote_host(control.clone()).await;
            if foreign {
                let mut other = claim();
                other.run_id = RunId("other-run".into());
                host.run_assembly
                    .installed_sessions
                    .install_run("session", |run| run.dispatch_claim = Some(other));
            }
            let error = query
                .run(
                    &host
                        .run_assembly
                        .capabilities
                        .coordination_endpoint()
                        .unwrap(),
                )
                .await
                .unwrap_err();
            assert!(matches!(error, ToolError::DefinitiveFailure(_)));
            assert!(error.to_string().contains(if foreign {
                "does not own"
            } else {
                "requires a current dispatch claim"
            }));
            assert!(
                control.calls.lock().unwrap().is_empty(),
                "{query:?}, foreign={foreign}"
            );
        }
    }
}

#[tokio::test]
async fn active_remote_queries_retain_original_installed_sessions_until_completion_or_cancellation()
{
    // Causes: C1 operation=list/send; C2 live owner plus matching current claim;
    // C3 transport is entered and blocks; C4 final external owner drops;
    // C5 outcome=success/original error/caller cancellation. R7-R12=C1+C2+C3+C4
    // -> retain the same installed Sessions until C5, pass through success/original error or
    // destroy the original future, then release installed Sessions and reject later I/O.
    // Entry/drop receipts prove the boundary; timeouts only bound broken tests.
    for query in [Query::List, Query::Send] {
        for finish in 0..3 {
            let gate = Arc::new(AdmissionGate::default());
            let control = Arc::new(RemoteControl {
                gate: Some(gate.clone()),
                fail: finish == 1,
                ..Default::default()
            });
            let host = remote_host(control.clone()).await;
            host.run_assembly
                .installed_sessions
                .install_run("session", |run| run.dispatch_claim = Some(claim()));
            let endpoint = host
                .run_assembly
                .capabilities
                .coordination_endpoint()
                .unwrap();
            let weak = Arc::downgrade(&host.run_assembly.installed_sessions);
            let mut call = Box::pin(query.run(&endpoint));
            tokio::select! {
                biased;
                result = &mut call => panic!("{query:?} completed before release: {result:?}"),
                () = gate.entered.notified() => {},
                () = tokio::time::sleep(Duration::from_secs(5)) => panic!("{query:?} did not enter its original control future"),
            }
            drop(host);
            assert!(
                weak.upgrade().is_some(),
                "{query:?}/{finish}: retain the original installed Sessions during control I/O"
            );
            if finish == 2 {
                drop(call);
            } else {
                gate.release.notify_one();
                let result = call.await;
                if finish == 1 {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("original remote query failure")
                    );
                } else {
                    assert!(result.is_ok());
                }
            }
            tokio::time::timeout(Duration::from_secs(5), gate.dropped.notified())
                .await
                .expect("original control future destroyed");
            assert!(
                weak.upgrade().is_none(),
                "{query:?}/{finish}: no idle strong cache edge"
            );
            assert!(
                query
                    .run(&endpoint)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("owner is unavailable")
            );
            assert_eq!(
                *control.calls.lock().unwrap(),
                vec![(query.name(), claim())],
                "same exact original claim, no later I/O"
            );
        }
    }
}

#[tokio::test]
async fn explicit_claim_controls_continue_after_original_installed_sessions_drop() {
    // Causes: C1 explicit claim-bound model/activity/settlement call; C2 the
    // Session previously held another Run and its original owner is now gone;
    // C3 model request Run matches/mismatches the explicit claim. Rules R13-R15:
    // C1+C2+matching -> exactly one call with the original claim, unchanged
    // admitted/rejected/settled result; R16=mismatch -> error before remote I/O.
    // Constraint R17: remote finalization is inert; only Coordinator finalizes.
    let control = Arc::new(RemoteControl::default());
    let host = remote_host(control.clone()).await;
    let mut other = claim();
    other.run_id = RunId("other-run".into());
    host.run_assembly
        .installed_sessions
        .install_run("session", |run| run.dispatch_claim = Some(other));
    let endpoint = host
        .run_assembly
        .capabilities
        .coordination_endpoint()
        .unwrap();
    let factory = claimed_model_request_gate_factory(endpoint.clone(), "session");
    let weak = Arc::downgrade(&host.run_assembly.installed_sessions);
    drop(host);
    assert!(weak.upgrade().is_none());
    let exact = claim();
    assert_eq!(
        factory(&exact)
            .admit_model_request(ModelRequestAdmissionRequest {
                run_id: exact.run_id.clone(),
                thread_id: ThreadId("child".into())
            })
            .await
            .unwrap(),
        ModelRequestAdmission::Admit
    );
    let observer = HostDispatchSettlementObserver::new(endpoint);
    let publication = |agent| {
        awaken_runtime_contract::ExecutableAgentSnapshot::builder(agent)
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse("test", "model", "native")
                    .unwrap(),
            )
            .build()
    };
    // The explicit-claim path now crosses the same dispatch observer the real
    // Worker uses. Root admission and child settlement retain the original
    // claim, mode and frozen identity, without consulting installed Sessions.
    let admission = RunDispatch::new(awaken_runtime_contract::RunActivation::new(
        exact.run_id.clone(),
        ThreadId("session".into()),
        publication("child"),
        Vec::new(),
    ))
    .for_session(ThreadId("session".into()));
    assert_eq!(
        observer
            .admit_session_run_activity(
                &admission,
                &exact,
                SessionRunActivityAdmissionMode::RecoverOnly
            )
            .await
            .unwrap(),
        SessionRunActivityAdmission::Rejected,
    );
    let boundary = RunDispatch::new(awaken_runtime_contract::RunActivation::new(
        exact.run_id.clone(),
        ThreadId("child".into()),
        publication("agent"),
        Vec::new(),
    ))
    .for_session(ThreadId("session".into()))
    .with_session_activity_epoch(3);
    let state = awaken_agent_contract::agent::run::RunState::Ended(
        awaken_agent_contract::agent::run::EndCause::NaturalEnd,
    );
    observer
        .before_settle(&boundary, &exact, &state, false)
        .await
        .unwrap();
    observer
        .after_settle(&boundary, &exact, &state, false)
        .await
        .unwrap();
    assert!(
        factory(&exact)
            .admit_model_request(ModelRequestAdmissionRequest {
                run_id: RunId("wrong-run".into()),
                thread_id: ThreadId("child".into())
            })
            .await
            .is_err()
    );
    assert_eq!(
        *control.calls.lock().unwrap(),
        vec![
            ("model", exact.clone()),
            ("activity", exact.clone()),
            ("settle", exact)
        ]
    );
}

#[tokio::test]
async fn model_gate_preserves_local_and_remote_control_failure_grades() {
    // Causes: C1 embedded/remote authority, C2 unavailable/not-ready/rejected
    // disposition. Cross C1 x C2: preserve transient/overloaded/permanent grade
    // and original diagnostic, without relabeling every error Unauthorized.
    // Each remote row makes exactly one claim-fenced call. This adapter cannot
    // mint inference completion evidence; the Runtime owns pre-Provider proof.
    use awaken_runtime_contract::resilience::FailureClass;
    use awaken_session_realization_contract::{
        ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE, SessionRealizationControlFailure as F,
    };
    let rows = [
        (
            awaken_session_types::RunError::unavailable("causal dependency"),
            F::Unavailable("causal dependency".into()),
            FailureClass::Transient,
        ),
        (
            awaken_session_types::RunError::unavailable_classified(
                ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
                "causal capacity",
            ),
            F::NotReady,
            FailureClass::Overloaded { retry_after: None },
        ),
        (
            awaken_session_types::RunError::bad_request("causal rejection"),
            F::Invalid("causal rejection".into()),
            FailureClass::Permanent,
        ),
    ];
    for (local_error, remote_error, expected) in rows {
        let local: Arc<dyn SessionAgentCoordination> =
            Arc::new(RecordingSessionAgentCoordination {
                model_error: std::sync::Mutex::new(Some(local_error)),
                ..Default::default()
            });
        let control = Arc::new(RemoteControl {
            model_error: Some(remote_error.into()),
            ..Default::default()
        });
        for endpoint in [
            CoordinationEndpoint::Local(Arc::downgrade(&local)),
            CoordinationEndpoint::Remote {
                control: control.clone(),
                installed_sessions: Weak::new(),
            },
        ] {
            let exact = claim();
            let error = HostModelRequestGate::claimed(endpoint, "session".into(), exact.clone())
                .admit_model_request(ModelRequestAdmissionRequest {
                    run_id: exact.run_id,
                    thread_id: ThreadId("thread".into()),
                })
                .await
                .unwrap_err();
            assert_eq!(error.class(), expected);
            assert_ne!(error.code(), "unauthorized");
            assert_eq!(
                error.execution_evidence(),
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::Unknown
            );
        }
        assert_eq!(control.calls.lock().unwrap().len(), 1);
    }
}
