// @formal entrypoint background_task.single_fenced_attempt
// @formal entrypoint background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
// @formal evidence session_execution.one_physical_attempt_per_thread
//! BackgroundTask binding for Runtime's existing post-admission ownership gate.
//! This adapter reads committed truth; the extension remains the decision owner.

use std::sync::Arc;

use awaken_ext_background_task::{BackgroundTask, BackgroundTaskSupervisor, task_state_cell};
use awaken_runtime_contract::{AttemptOwnershipError, AttemptOwnershipVerifier, RuntimeRunContext};
use awaken_time::WallClockExt;

use super::BackgroundTaskTerminalObserver;
use awaken_session_run_delivery::CommittedAuthority;
#[cfg(test)]
use awaken_session_run_delivery::SessionCommit;

struct BackgroundTaskOwnership {
    expected: BackgroundTask,
    commit: CommittedAuthority<Arc<dyn crate::LocalCommit>>,
    supervisor: Arc<BackgroundTaskSupervisor>,
}

#[async_trait::async_trait]
impl AttemptOwnershipVerifier for BackgroundTaskOwnership {
    async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
        let commands = self
            .commit
            .committed_state(&self.expected.origin.thread_id)
            .await
            .map_err(AttemptOwnershipError::Unavailable)?;
        let state = awaken_agent_contract::agent::state::Store::rebuild(&commands);
        let current = task_state_cell(&self.expected.id)
            .load(&state)
            .map_err(|error| AttemptOwnershipError::Unavailable(error.to_string()))?
            .ok_or(AttemptOwnershipError::Lost)?;
        let expected = self.expected.attempt().ok_or(AttemptOwnershipError::Lost)?;
        if current.admits_execution_of(&self.expected, self.supervisor.clock().now_ms())
            && self
                .supervisor
                .is_active(&self.expected.id, &expected.tenure(&self.expected.id))
        {
            Ok(())
        } else {
            Err(AttemptOwnershipError::Lost)
        }
    }
}

impl BackgroundTaskTerminalObserver {
    pub(super) fn execution_context(
        &self,
        expected: &BackgroundTask,
        cancellation: awaken_runtime_contract::CancellationToken,
    ) -> RuntimeRunContext {
        let mut context = self.context.clone();
        context.cancellation = Some(cancellation);
        context.terminal_observers.clear();
        context.physical_quiescence = Default::default();
        // Detached work owns its own durable attempt, not the already-ended
        // foreground Run's dispatch claim. Keep one neutral Runtime verifier.
        context.ownership = Some(Arc::new(BackgroundTaskOwnership {
            expected: expected.clone(),
            commit: self.commit.clone(),
            supervisor: self.supervisor.clone(),
        }));
        context
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, sync::atomic::Ordering, time::Duration};

    use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
    use awaken_agent_contract::thread::commit::{
        coordinator::Coordinator,
        staged::{RunDisposition, ThreadCommit},
    };
    use awaken_ext_background_task::{
        BackgroundInvocation, BackgroundTaskConfig, BackgroundTaskEnd, BackgroundTaskId,
        BackgroundTaskLifecycle, BackgroundTaskOrigin, BackgroundTaskPlugin, BackgroundWait,
        TaskExecutionPolicy,
    };
    use awaken_lease_testkit::ManualClock;
    use awaken_runtime::{PreparedToolExecutor, Runtime};
    use awaken_tool_contract::tool::{ToolCall, ToolError, ToolRecoveryPolicy};
    use awaken_tool_contract::{ToolConcurrency, ToolExecutionAdmission, ToolExecutionPermit};

    use super::super::tests::{RecordingAfterToolPlugin, RemoteTaskTool, remote_snapshot};
    use crate::background::BackgroundRuns;

    #[test]
    fn separately_owned_background_attempts_do_not_inherit_foreground_uncertainty() {
        // Q1 foreground uncertainty must not veto detached A/B (new authority).
        // Q2 A's inline child reports uncertainty => A only, never B. Q3 A's
        // cancellation token cannot cancel B or the foreground token. Shared
        // Session concurrency is retained; it is not physical-attempt evidence.
        let mut foreground = RuntimeRunContext::new();
        let foreground_token = awaken_runtime_contract::CancellationToken::new();
        foreground.cancellation = Some(foreground_token.clone());
        foreground.physical_quiescence.mark_uncertain();
        let observer = BackgroundTaskTerminalObserver::new(
            Arc::new(Runtime::new()),
            remote_snapshot(),
            foreground,
            local_commit()
                .committed_authority()
                .expect("local test commit is authoritative"),
            Arc::new(BackgroundRuns::new()),
            Arc::new(BackgroundTaskSupervisor::new(
                "worker",
                awaken_time::system_clock(),
            )),
            "session".into(),
            "generation".into(),
            None,
        );
        let a = observer.execution_context(&task(0, &observer.supervisor), Default::default());
        let b = observer.execution_context(&task(0, &observer.supervisor), Default::default());
        assert!(a.physical_quiescence.can_acknowledge_return(), "Q1 A");
        assert!(b.physical_quiescence.can_acknowledge_return(), "Q1 B");
        a.for_child_run().physical_quiescence.mark_uncertain();
        a.cancellation.as_ref().unwrap().cancel();
        assert!(!a.physical_quiescence.can_acknowledge_return(), "Q2 A");
        assert!(b.physical_quiescence.can_acknowledge_return(), "Q2 B");
        assert!(!b.cancellation.as_ref().unwrap().is_cancelled(), "Q3 B");
        assert!(!foreground_token.is_cancelled(), "Q3 foreground");
    }

    #[tokio::test]
    async fn unknown_background_start_poll_and_cancel_terminate_without_observation_retry() {
        // U1 Running/start; U2 Waiting/poll; U3 Cancelling/cancel. Each concrete
        // executor returns typed unknown after one entry => one exact-fence
        // Indeterminate candidate. No poll retry, second start, or subsequent
        // cancel-as-proof. Committed task state remains the only durable owner.
        // U4 start returns Pending with malformed coordinates: use the same
        // Indeterminate branch, never normal Failed or a fabricated wait handle.
        for (boundary, malformed) in [(0, false), (1, false), (2, false), (0, true)] {
            let supervisor = Arc::new(BackgroundTaskSupervisor::new(
                "worker",
                awaken_time::system_clock(),
            ));
            let tool = Arc::new(RemoteTaskTool {
                uncertain_action: (!malformed).then_some(boundary),
                malformed_created_handle: malformed,
                ..Default::default()
            });
            let runtime = Arc::new(Runtime::new().with_tool(tool.clone()).with_plugin(Arc::new(
                BackgroundTaskPlugin::with_supervisor(
                    BackgroundTaskConfig {
                        tools: BTreeSet::from(["remote".into()]),
                        ..Default::default()
                    },
                    supervisor.clone(),
                ),
            )));
            let commit = local_commit();
            let background = Arc::new(BackgroundRuns::new());
            let observer = BackgroundTaskTerminalObserver::new(
                runtime,
                remote_snapshot(),
                RuntimeRunContext::new(),
                commit
                    .committed_authority()
                    .expect("local test commit is authoritative"),
                background.clone(),
                supervisor.clone(),
                "session".into(),
                "generation".into(),
                None,
            );
            let task = task(boundary, &supervisor);
            persist(&commit, &task, "initial").await;
            if boundary == 0 {
                observer.launch(task.clone()).await;
            } else {
                observer.launch_remote(task.clone()).await;
            }
            assert!(background.drain(Duration::from_secs(2)).await);
            let completion = supervisor.completion(&task.id).expect("terminal candidate");
            assert_eq!(completion.fence, task.tenure().unwrap());
            assert!(matches!(
                completion.end,
                BackgroundTaskEnd::Indeterminate { .. }
            ));
            assert!(
                supervisor.wait_candidate(&task.id).is_none(),
                "U1..U4 no wait"
            );
            if boundary == 0 {
                observer.launch(task.clone()).await;
            } else {
                observer.launch_remote(task.clone()).await;
            }
            assert!(background.drain(Duration::from_secs(2)).await);
            assert_eq!(
                tool.starts.load(Ordering::SeqCst),
                usize::from(boundary == 0)
            );
            assert_eq!(
                tool.polls.load(Ordering::SeqCst),
                usize::from(boundary == 1)
            );
            assert_eq!(
                tool.cancels.load(Ordering::SeqCst),
                usize::from(boundary == 2)
            );
        }
    }

    fn task(boundary: usize, supervisor: &BackgroundTaskSupervisor) -> BackgroundTask {
        let mut task = BackgroundTask::requested(
            BackgroundTaskId::new("guarded-task").unwrap(),
            BackgroundTaskOrigin {
                thread_id: ThreadId("thread".into()),
                run_id: RunId("origin".into()),
                operation_id: "operation".into(),
            },
            BackgroundInvocation {
                call: ToolCall {
                    call_id: "call".into(),
                    tool_id: "remote".into(),
                    arguments: serde_json::json!({}),
                },
            },
        );
        let fence = task
            .start(
                "worker",
                supervisor.clock().now_ms(),
                BackgroundTaskSupervisor::lease_ms(),
                TaskExecutionPolicy {
                    recovery: ToolRecoveryPolicy::default(),
                    concurrency: ToolConcurrency::Parallel,
                },
            )
            .unwrap();
        if boundary > 0 {
            task.wait(&fence, BackgroundWait::Remote(RemoteTaskTool::handle()))
                .unwrap();
        }
        if boundary == 2 {
            task.request_cancel().unwrap();
        }
        task
    }

    async fn persist(commit: &SessionCommit, task: &BackgroundTask, run: &str) {
        commit
            .commit(ThreadCommit::assemble(
                task.origin.thread_id.clone(),
                RunDisposition::ended(
                    RunId(run.into()),
                    awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                ),
                true,
                Vec::new(),
                vec![task_state_cell(&task.id).write(task).unwrap()],
                Vec::new(),
            ))
            .await
            .unwrap();
    }

    fn local_commit() -> Arc<SessionCommit> {
        Arc::new(SessionCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new(
                awaken_time::system_clock(),
            )),
        )))
    }

    struct ObservedAdmission {
        inner: Arc<dyn ToolExecutionAdmission>,
        entered: Arc<tokio::sync::Notify>,
        claims: Arc<std::sync::Mutex<Vec<ToolConcurrency>>>,
    }

    #[async_trait::async_trait]
    impl ToolExecutionAdmission for ObservedAdmission {
        async fn acquire(
            self: Arc<Self>,
            claim: ToolConcurrency,
        ) -> Result<Box<dyn ToolExecutionPermit>, ToolError> {
            // Observe entry only; the real shared admission still owns conflict
            // decisions and permits. No competing test scheduler is introduced.
            self.claims.lock().unwrap().push(claim.clone());
            self.entered.notify_one();
            self.inner.clone().acquire(claim).await
        }
    }

    #[tokio::test]
    async fn detached_effects_recheck_committed_claim_after_concurrency_admission() {
        // Causes: Start/Poll/Cancel x authority change while blocked behind a
        // real Serial permit: unchanged, cancel/reclaim, Ended, expired lease,
        // process replacement. R1 unchanged -> exactly one selected effect;
        // R2 each invalidation -> error and zero start/poll/cancel effects.
        // A Notify establishes the ordering, not sleeps. All three actions use
        // the production observer context and Runtime's single executor gate.
        // This checks invalidation before the gate's read; it does not claim an
        // atomic transaction spanning that read and a third-party network effect.
        // C3 the plugin narrows a Parallel target to Write(shared); the held
        // permit is that resource claim rather than Serial. R3 all four actions
        // pass the complete claim to the same real admission before the original
        // post-wait ownership check. R4 invoke is an ordinary configured target;
        // start/poll/cancel keep the original durable target and handle.
        for (boundary, constrained) in
            (0..4).flat_map(|boundary| [(boundary, false), (boundary, true)])
        {
            for change in 0..5 {
                let supervisor = Arc::new(BackgroundTaskSupervisor::new(
                    "worker",
                    awaken_time::system_clock(),
                ));
                let remote = Arc::new(RemoteTaskTool {
                    ordinary_invoke: boundary == 3,
                    ..Default::default()
                });
                let resource_claim = ToolConcurrency::Resources(vec![
                    awaken_tool_contract::tool::ToolResourceAccess::Write(
                        awaken_tool_contract::tool::ToolResource::new("plugin-test", "shared"),
                    ),
                ]);
                let claim = if constrained {
                    resource_claim.clone()
                } else {
                    ToolConcurrency::Parallel
                };
                let held_claim = if constrained {
                    resource_claim
                } else {
                    ToolConcurrency::Serial
                };
                let mut snapshot = remote_snapshot();
                snapshot
                    .resolved_spec
                    .plugin_ids
                    .push("test.after-tool".into());
                if boundary == 3 {
                    snapshot.resolved_spec.tool_descriptors[0].kind =
                        awaken_runtime_contract::resolved::ToolKind::Regular;
                }
                let runtime = Arc::new(
                    Runtime::new()
                        .with_tool(remote.clone())
                        .with_plugin(Arc::new(BackgroundTaskPlugin::with_supervisor(
                            BackgroundTaskConfig {
                                tools: BTreeSet::from(["remote".into()]),
                                ..Default::default()
                            },
                            supervisor.clone(),
                        )))
                        .with_plugin(Arc::new(RecordingAfterToolPlugin(
                            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                            claim.clone(),
                        ))),
                );
                let commit = local_commit();
                let background = Arc::new(BackgroundRuns::new());
                let admission = background
                    .tool_execution_admission("session", "generation")
                    .expect("healthy registry");
                let held = admission.clone().acquire(held_claim).await.unwrap();
                let entered = Arc::new(tokio::sync::Notify::new());
                let claims = Arc::new(std::sync::Mutex::new(Vec::new()));
                let context = RuntimeRunContext::new().with_tool_execution_admission(Arc::new(
                    ObservedAdmission {
                        inner: admission,
                        entered: entered.clone(),
                        claims: claims.clone(),
                    },
                ));
                let observer = BackgroundTaskTerminalObserver::new(
                    runtime.clone(),
                    snapshot.clone(),
                    context,
                    commit
                        .committed_authority()
                        .expect("local test commit is authoritative"),
                    background,
                    supervisor.clone(),
                    "session".into(),
                    "generation".into(),
                    None,
                );
                let mut task = task(if boundary == 3 { 0 } else { boundary }, &supervisor);
                persist(&commit, &task, "initial").await;
                let fence = task.tenure().unwrap();
                let token = supervisor.register(&task.id, &fence).unwrap();
                let prepared = PreparedToolExecutor::new(
                    runtime,
                    &snapshot,
                    observer.execution_context(&task, token),
                )
                .unwrap();
                let expected = task.clone();
                let execution = tokio::spawn(async move {
                    let state = awaken_agent_contract::agent::state::Store::new();
                    let run = RunId("detached".into());
                    let thread = &expected.origin.thread_id;
                    let call = &expected.invocation.call;
                    match boundary {
                        0 => prepared
                            .start_task(&run, thread, "start".into(), call, &state)
                            .await
                            .map(|_| ()),
                        1 => prepared
                            .poll_task(
                                &run,
                                thread,
                                "poll".into(),
                                call,
                                &RemoteTaskTool::handle(),
                                &state,
                            )
                            .await
                            .map(|_| ()),
                        2 => prepared
                            .cancel_task(
                                &run,
                                thread,
                                "cancel".into(),
                                call,
                                &RemoteTaskTool::handle(),
                                &state,
                            )
                            .await
                            .map(|_| ()),
                        _ => prepared
                            .invoke(&run, thread, "invoke".into(), call, &state)
                            .await
                            .map(|_| ()),
                    }
                });
                tokio::time::timeout(Duration::from_secs(2), entered.notified())
                    .await
                    .unwrap();
                assert_eq!(
                    claims.lock().unwrap().as_slice(),
                    &[claim],
                    "R3 complete admitted claim"
                );
                assert_eq!(
                    remote.starts.load(Ordering::SeqCst)
                        + remote.polls.load(Ordering::SeqCst)
                        + remote.cancels.load(Ordering::SeqCst)
                        + remote.invokes.load(Ordering::SeqCst),
                    0,
                    "R3 resource waiter has entered no effect"
                );
                match change {
                    1 if boundary == 0 || boundary == 3 => task.request_cancel().unwrap(),
                    1 => {
                        let expiry = task.attempt().unwrap().lease_expires_at_ms;
                        task.reclaim("replacement", expiry, BackgroundTaskSupervisor::lease_ms())
                            .unwrap();
                    }
                    2 => task.finish(&fence, BackgroundTaskEnd::Cancelled).unwrap(),
                    3 => match &mut task.lifecycle {
                        BackgroundTaskLifecycle::Running { attempt }
                        | BackgroundTaskLifecycle::Waiting { attempt, .. }
                        | BackgroundTaskLifecycle::Cancelling { attempt, .. } => {
                            attempt.lease_expires_at_ms = 1
                        }
                        _ => unreachable!(),
                    },
                    4 => {
                        let replacement = fence.reassigned(fence.holder().clone()).unwrap();
                        supervisor.register(&task.id, &replacement).unwrap();
                    }
                    _ => {}
                }
                if matches!(change, 1..=3) {
                    persist(&commit, &task, "changed").await;
                }
                drop(held);
                let result = tokio::time::timeout(Duration::from_secs(2), execution)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    result.is_ok(),
                    change == 0,
                    "boundary={boundary} change={change}: {result:?}"
                );
                assert_eq!(
                    remote.starts.load(Ordering::SeqCst),
                    usize::from(change == 0 && boundary == 0)
                );
                assert_eq!(
                    remote.polls.load(Ordering::SeqCst),
                    usize::from(change == 0 && boundary == 1)
                );
                assert_eq!(
                    remote.cancels.load(Ordering::SeqCst),
                    usize::from(change == 0 && boundary == 2)
                );
                assert_eq!(
                    remote.invokes.load(Ordering::SeqCst),
                    usize::from(change == 0 && boundary == 3)
                );
            }
        }
    }

    #[tokio::test]
    async fn authority_changes_after_last_read_cannot_authorize_a_second_effect_or_stale_fold() {
        // B1 start/poll/cancel has passed the final authoritative read and is
        // already inside the adapter; B2 takeover/Ended commits before that
        // adapter produces its response. One admitted external effect can still
        // complete: a local read cannot atomically fence a remote service.
        // B3 its old fence cannot fold into the newer durable task; B4 the next
        // action rereads authority and emits zero additional effects. The gate
        // sits INSIDE RawTool, after admission, unlike the pre-admission table.
        for boundary in 0..3 {
            for terminal in [false, true] {
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                let remote = Arc::new(RemoteTaskTool {
                    effect_gate: Some(gate.clone()),
                    ..Default::default()
                });
                let supervisor = Arc::new(BackgroundTaskSupervisor::new(
                    "worker",
                    awaken_time::system_clock(),
                ));
                let plugin = Arc::new(BackgroundTaskPlugin::with_supervisor(
                    BackgroundTaskConfig {
                        tools: BTreeSet::from(["remote".into()]),
                        ..Default::default()
                    },
                    supervisor.clone(),
                ));
                let runtime =
                    Arc::new(Runtime::new().with_plugin(plugin).with_tool(remote.clone()));
                let commit = local_commit();
                let observer = BackgroundTaskTerminalObserver::new(
                    runtime.clone(),
                    remote_snapshot(),
                    RuntimeRunContext::new(),
                    commit
                        .committed_authority()
                        .expect("local test commit is authoritative"),
                    Arc::new(BackgroundRuns::new()),
                    supervisor.clone(),
                    "session".into(),
                    "generation".into(),
                    None,
                );
                let mut current = task(boundary, &supervisor);
                persist(&commit, &current, "before-effect").await;
                let old = current.clone();
                let fence = old.tenure().unwrap();
                let token = supervisor.register(&old.id, &fence).unwrap();
                let prepared = Arc::new(
                    PreparedToolExecutor::new(
                        runtime,
                        &remote_snapshot(),
                        observer.execution_context(&old, token),
                    )
                    .unwrap(),
                );
                let invoke = |prepared: Arc<PreparedToolExecutor>, expected: BackgroundTask| async move {
                    let state = awaken_agent_contract::agent::state::Store::new();
                    let run = RunId("effect".into());
                    let call = &expected.invocation.call;
                    let thread = &expected.origin.thread_id;
                    match boundary {
                        0 => prepared
                            .start_task(&run, thread, "start".into(), call, &state)
                            .await
                            .map(|_| ()),
                        1 => prepared
                            .poll_task(
                                &run,
                                thread,
                                "poll".into(),
                                call,
                                &RemoteTaskTool::handle(),
                                &state,
                            )
                            .await
                            .map(|_| ()),
                        _ => prepared
                            .cancel_task(
                                &run,
                                thread,
                                "cancel".into(),
                                call,
                                &RemoteTaskTool::handle(),
                                &state,
                            )
                            .await
                            .map(|_| ()),
                    }
                };
                let pending = tokio::spawn(invoke(prepared.clone(), old.clone()));
                tokio::time::timeout(Duration::from_secs(2), remote.effect_entered.notified())
                    .await
                    .unwrap();
                if terminal {
                    current
                        .finish(&fence, BackgroundTaskEnd::Cancelled)
                        .unwrap();
                } else {
                    current
                        .reclaim(
                            "replacement",
                            current.attempt().unwrap().lease_expires_at_ms,
                            BackgroundTaskSupervisor::lease_ms(),
                        )
                        .unwrap();
                }
                persist(&commit, &current, "after-authority-change").await;
                gate.add_permits(1);
                pending.await.unwrap().unwrap();
                let before_fold = current.clone();
                assert!(
                    current
                        .finish(&fence, BackgroundTaskEnd::Cancelled)
                        .is_err(),
                    "B3"
                );
                assert_eq!(current, before_fold);
                assert!(invoke(prepared, old).await.is_err(), "B4");
                assert_eq!(
                    remote.starts.load(Ordering::SeqCst),
                    usize::from(boundary == 0)
                );
                assert_eq!(
                    remote.polls.load(Ordering::SeqCst),
                    usize::from(boundary == 1)
                );
                assert_eq!(
                    remote.cancels.load(Ordering::SeqCst),
                    usize::from(boundary == 2)
                );
            }
        }
    }

    #[tokio::test]
    async fn injected_host_clock_decides_the_exact_background_lease_boundary() {
        // Causes: C1 one committed Running attempt and its matching active
        // process installed Session; C2 the same injected Host clock reads just before,
        // exactly at, and after its persisted expiry. Effects: R1 C1+C2<expiry
        // admits; R2 C1+C2>=expiry loses ownership without changing the
        // committed cell or process installed Session. The verifier must never sample an
        // ambient clock that disagrees with the tool's submission stamp.
        let clock = Arc::new(ManualClock::new(1_000));
        let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker", clock.clone()));
        let expected = task(0, &supervisor);
        let expiry = expected.attempt().unwrap().lease_expires_at_ms;
        supervisor
            .register(&expected.id, &expected.tenure().unwrap())
            .unwrap();
        let commit = local_commit();
        persist(&commit, &expected, "injected-clock-lease").await;
        let authority = BackgroundTaskOwnership {
            expected: expected.clone(),
            commit: commit
                .committed_authority()
                .expect("local test commit is authoritative"),
            supervisor: supervisor.clone(),
        };
        clock.set(expiry - 1);
        assert_eq!(authority.verify_current().await, Ok(()), "R1");
        clock.set(expiry);
        assert_eq!(
            authority.verify_current().await,
            Err(AttemptOwnershipError::Lost),
            "R2 at expiry"
        );
        clock.set(expiry + 1);
        assert_eq!(
            authority.verify_current().await,
            Err(AttemptOwnershipError::Lost),
            "R2 after expiry"
        );
        assert!(supervisor.is_active(&expected.id, &expected.tenure().unwrap()));
    }

    #[tokio::test]
    async fn missing_malformed_and_projected_authorities_fail_closed() {
        // Adapter-owner rules C3/C6: relocation must preserve the live query-policy
        // error and keep the Worker projection unable to mint committed authority;
        // no new adapter may turn either refusal into cached success.
        // R1 missing cell -> Lost; R2 malformed committed cell -> Unavailable;
        // R3 Worker projection (not live authority) -> cannot mint the receipt
        // required to construct this verifier; R4 live authority read failure ->
        // exact Unavailable, no cached fallback. Neither a process token nor a
        // previously valid expected snapshot grants authority.
        let supervisor = Arc::new(BackgroundTaskSupervisor::new(
            "worker",
            awaken_time::system_clock(),
        ));
        let expected = task(0, &supervisor);
        supervisor
            .register(&expected.id, &expected.tenure().unwrap())
            .unwrap();
        let commit = local_commit();
        let authority = BackgroundTaskOwnership {
            expected: expected.clone(),
            commit: commit
                .committed_authority()
                .expect("local test commit is authoritative"),
            supervisor: supervisor.clone(),
        };
        assert!(matches!(
            authority.verify_current().await,
            Err(AttemptOwnershipError::Lost)
        ));
        let mut malformed = expected.clone();
        malformed.revision = 0;
        persist(&commit, &malformed, "malformed").await;
        assert!(matches!(
            authority.verify_current().await,
            Err(AttemptOwnershipError::Unavailable(_))
        ));
        let projected: SessionCommit =
            SessionCommit::Remote(awaken_session_run_delivery::WorkerRecoveryCommit::new());
        assert!(projected.committed_authority().is_none(), "R3");
        let unavailable_commit: SessionCommit =
            SessionCommit::Local(Arc::new(crate::LocalCommitAdapter::with_queries(
                Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
                    awaken_time::system_clock(),
                )),
                crate::authority::ScriptedStateQueries::new(Err("state read unavailable".into())),
            )));
        let unavailable = BackgroundTaskOwnership {
            expected,
            commit: unavailable_commit
                .committed_authority()
                .expect("R4 local test commit is authoritative"),
            supervisor,
        };
        assert_eq!(
            unavailable.verify_current().await,
            Err(AttemptOwnershipError::Unavailable(
                "state read unavailable".into()
            ))
        );
    }
}
