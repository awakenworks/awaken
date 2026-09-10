//! BackgroundTask binding for Runtime's existing post-admission ownership gate.
//! This adapter reads committed truth; the extension remains the decision owner.

use std::sync::Arc;

use awaken_ext_background_task::{BackgroundTask, BackgroundTaskSupervisor, task_state_cell};
use awaken_runtime_contract::{AttemptOwnershipError, AttemptOwnershipVerifier, RuntimeRunContext};

use super::BackgroundTaskTerminalObserver;
use crate::store::HostCommit;

struct BackgroundTaskOwnership {
    expected: BackgroundTask,
    commit: Arc<HostCommit>,
    supervisor: Arc<BackgroundTaskSupervisor>,
}

#[async_trait::async_trait]
impl AttemptOwnershipVerifier for BackgroundTaskOwnership {
    async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
        // Worker recovery projections are not live authority. Startup already
        // excludes this placement for BackgroundTask; enforce that boundary
        // here too, rather than accepting cached state as a fresh lease check.
        let HostCommit::Local(commit) = self.commit.as_ref() else {
            return Err(AttemptOwnershipError::Unavailable(
                "background execution requires a live local commit authority".into(),
            ));
        };
        let commands = commit
            .authoritative_committed_state(&self.expected.origin.thread_id)
            .await
            .map_err(AttemptOwnershipError::Unavailable)?;
        let state = awaken_runtime_contract::Store::rebuild(&commands);
        let current = task_state_cell(&self.expected.id)
            .load(&state)
            .map_err(|error| AttemptOwnershipError::Unavailable(error.to_string()))?
            .ok_or(AttemptOwnershipError::Lost)?;
        let expected = self.expected.attempt().ok_or(AttemptOwnershipError::Lost)?;
        if current.admits_execution_of(&self.expected, BackgroundTaskSupervisor::now_ms())
            && self
                .supervisor
                .is_active(&self.expected.id, &expected.fence())
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
    use awaken_runtime::{PreparedToolExecutor, Runtime};
    use awaken_runtime_contract::tool::{ToolCall, ToolError, ToolRecoveryPolicy};
    use awaken_runtime_contract::{ToolConcurrency, ToolExecutionAdmission, ToolExecutionPermit};

    use super::super::tests::{RemoteTaskTool, remote_snapshot};
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
            local_commit(),
            Arc::new(BackgroundRuns::new()),
            Arc::new(BackgroundTaskSupervisor::new("worker")),
            "session".into(),
            "generation".into(),
            None,
        );
        let a = observer.execution_context(&task(0), Default::default());
        let b = observer.execution_context(&task(0), Default::default());
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
            let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker"));
            let tool = Arc::new(RemoteTaskTool {
                uncertain_action: (!malformed).then_some(boundary),
                malformed_created_handle: malformed,
                ..Default::default()
            });
            let runtime = Arc::new(Runtime::new().with_tool(tool.clone()).with_plugin(Arc::new(
                BackgroundTaskPlugin::with_supervisor(
                    BackgroundTaskConfig {
                        tools: BTreeSet::from(["remote".into()]),
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
                commit.clone(),
                background.clone(),
                supervisor.clone(),
                "session".into(),
                "generation".into(),
                None,
            );
            let task = task(boundary);
            persist(&commit, &task, "initial").await;
            if boundary == 0 {
                observer.launch(task.clone()).await;
            } else {
                observer.launch_remote(task.clone()).await;
            }
            assert!(background.drain(Duration::from_secs(2)).await);
            let completion = supervisor.completion(&task.id).expect("terminal candidate");
            assert_eq!(completion.fence, task.attempt().unwrap().fence());
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

    fn task(boundary: usize) -> BackgroundTask {
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
                BackgroundTaskSupervisor::now_ms(),
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

    async fn persist(commit: &HostCommit, task: &BackgroundTask, run: &str) {
        commit
            .commit(ThreadCommit::assemble(
                task.origin.thread_id.clone(),
                RunDisposition::ended(
                    RunId(run.into()),
                    awaken_runtime_contract::EndCause::NaturalEnd,
                ),
                true,
                Vec::new(),
                vec![task_state_cell(&task.id).write(task).unwrap()],
                Vec::new(),
            ))
            .await
            .unwrap();
    }

    fn local_commit() -> Arc<HostCommit> {
        Arc::new(HostCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new()),
        )))
    }

    struct ObservedAdmission {
        inner: Arc<dyn ToolExecutionAdmission>,
        entered: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl ToolExecutionAdmission for ObservedAdmission {
        async fn acquire(
            self: Arc<Self>,
            claim: ToolConcurrency,
        ) -> Result<Box<dyn ToolExecutionPermit>, ToolError> {
            // Observe entry only; the real shared admission still owns conflict
            // decisions and permits. No competing test scheduler is introduced.
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
        for boundary in 0..3 {
            for change in 0..5 {
                let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker"));
                let remote = Arc::new(RemoteTaskTool::default());
                let runtime = Arc::new(Runtime::new().with_tool(remote.clone()).with_plugin(
                    Arc::new(BackgroundTaskPlugin::with_supervisor(
                        BackgroundTaskConfig {
                            tools: BTreeSet::from(["remote".into()]),
                        },
                        supervisor.clone(),
                    )),
                ));
                let commit = local_commit();
                let background = Arc::new(BackgroundRuns::new());
                let admission = background.tool_execution_admission("session", "generation");
                let held = admission
                    .clone()
                    .acquire(ToolConcurrency::Serial)
                    .await
                    .unwrap();
                let entered = Arc::new(tokio::sync::Notify::new());
                let context = RuntimeRunContext::new().with_tool_execution_admission(Arc::new(
                    ObservedAdmission {
                        inner: admission,
                        entered: entered.clone(),
                    },
                ));
                let observer = BackgroundTaskTerminalObserver::new(
                    runtime.clone(),
                    remote_snapshot(),
                    context,
                    commit.clone(),
                    background,
                    supervisor.clone(),
                    "session".into(),
                    "generation".into(),
                    None,
                );
                let mut task = task(boundary);
                persist(&commit, &task, "initial").await;
                let fence = task.attempt().unwrap().fence();
                let token = supervisor.register(&task.id, &fence).unwrap();
                let prepared = PreparedToolExecutor::new(
                    runtime,
                    &remote_snapshot(),
                    observer.execution_context(&task, token),
                )
                .unwrap();
                let expected = task.clone();
                let execution = tokio::spawn(async move {
                    let state = awaken_runtime_contract::Store::new();
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
                });
                tokio::time::timeout(Duration::from_secs(2), entered.notified())
                    .await
                    .unwrap();
                match change {
                    1 if boundary == 0 => task.request_cancel().unwrap(),
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
                        let mut replacement = fence.clone();
                        replacement.epoch += 1;
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
                let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker"));
                let plugin = Arc::new(BackgroundTaskPlugin::with_supervisor(
                    BackgroundTaskConfig {
                        tools: BTreeSet::from(["remote".into()]),
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
                    commit.clone(),
                    Arc::new(BackgroundRuns::new()),
                    supervisor.clone(),
                    "session".into(),
                    "generation".into(),
                    None,
                );
                let mut current = task(boundary);
                persist(&commit, &current, "before-effect").await;
                let old = current.clone();
                let fence = old.attempt().unwrap().fence();
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
                    let state = awaken_runtime_contract::Store::new();
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
    async fn missing_malformed_and_projected_authorities_fail_closed() {
        // R1 missing cell -> Lost; R2 malformed committed cell -> Unavailable;
        // R3 Worker projection (not live authority) -> Unavailable; R4 live
        // authority read failure -> exact Unavailable, no cached fallback. Neither a
        // process token nor previously valid expected snapshot grants authority.
        let expected = task(0);
        let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker"));
        supervisor
            .register(&expected.id, &expected.attempt().unwrap().fence())
            .unwrap();
        let commit = local_commit();
        let authority = BackgroundTaskOwnership {
            expected: expected.clone(),
            commit: commit.clone(),
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
        let projected = BackgroundTaskOwnership {
            expected: expected.clone(),
            commit: Arc::new(HostCommit::Remote(crate::store::RemoteHostCommit::new())),
            supervisor: supervisor.clone(),
        };
        assert!(matches!(
            projected.verify_current().await,
            Err(AttemptOwnershipError::Unavailable(_))
        ));
        let unavailable = BackgroundTaskOwnership {
            expected,
            commit: Arc::new(HostCommit::Local(Arc::new(
                crate::LocalCommitAdapter::with_queries(
                    Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new()),
                    crate::authority::ScriptedStateQueries::new(Err(
                        "state read unavailable".into()
                    )),
                ),
            ))),
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
