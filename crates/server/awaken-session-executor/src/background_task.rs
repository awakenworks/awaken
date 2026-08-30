//! Product post-commit execution of the state-owned BackgroundTask aggregate.
//!
//! This adapter observes committed Running claims and invokes their canonical
//! ordinary tool through Runtime. It writes no database and creates no hidden
//! Run. Completion stays process-local until the extension's StepStart hook
//! folds it into the next ordinary Thread commit.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::state::Store;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_ext_background_task::{
    BackgroundTaskCompletion, BackgroundTaskEnd, BackgroundTaskId, BackgroundTaskLifecycle,
    BackgroundTaskSupervisor, TaskFence, tasks_from_state,
};
use awaken_runtime::{PreparedToolExecutor, Runtime};
use awaken_runtime_contract::ExecutableAgentSnapshot;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::terminal::{
    CommittedTerminalRun, RunTerminalObserver, RunTerminalObserverError,
};
use awaken_runtime_contract::tool::{ToolConcurrency, ToolOutput};

use crate::background::{BackgroundRuns, BackgroundWorkClass};
use crate::store::HostCommit;

const OBSERVER_ID: &str = "awaken.background-task.executor.v1";

#[derive(Default)]
struct BackgroundAdmission {
    /// Sequence allocation and active-claim admission share one critical
    /// section. Checking compatibility and publishing the admitted claim must
    /// be atomic; separate locks allow two incompatible arrivals to both
    /// observe an empty active set before either inserts.
    state: Mutex<(u64, BTreeMap<u64, ToolConcurrency>)>,
    changed: tokio::sync::Notify,
}

impl BackgroundAdmission {
    async fn acquire(self: &Arc<Self>, claim: ToolConcurrency) -> AdmissionGuard {
        loop {
            let notified = self.changed.notified();
            {
                let mut state = self
                    .state
                    .lock()
                    .expect("background admission mutex poisoned");
                if state
                    .1
                    .values()
                    .all(|active| active.compatible_with(&claim))
                {
                    state.0 = state
                        .0
                        .checked_add(1)
                        .expect("background admission exhausted");
                    let id = state.0;
                    state.1.insert(id, claim);
                    return AdmissionGuard {
                        admission: self.clone(),
                        id,
                    };
                }
            }
            notified.await;
        }
    }
}

struct AdmissionGuard {
    admission: Arc<BackgroundAdmission>,
    id: u64,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.admission
            .state
            .lock()
            .expect("background admission mutex poisoned")
            .1
            .remove(&self.id);
        self.admission.changed.notify_waiters();
    }
}

pub(crate) struct BackgroundTaskTerminalObserver {
    runtime: Arc<Runtime>,
    snapshot: ExecutableAgentSnapshot,
    context: RuntimeRunContext,
    commit: Arc<HostCommit>,
    background: Arc<BackgroundRuns>,
    supervisor: Arc<BackgroundTaskSupervisor>,
    session_id: String,
    environment_generation: String,
    admission: Arc<BackgroundAdmission>,
}

impl BackgroundTaskTerminalObserver {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        runtime: Arc<Runtime>,
        snapshot: ExecutableAgentSnapshot,
        context: RuntimeRunContext,
        commit: Arc<HostCommit>,
        background: Arc<BackgroundRuns>,
        supervisor: Arc<BackgroundTaskSupervisor>,
        session_id: String,
        environment_generation: String,
    ) -> Self {
        Self {
            runtime,
            snapshot,
            context,
            commit,
            background,
            supervisor,
            session_id,
            environment_generation,
            admission: Arc::new(BackgroundAdmission::default()),
        }
    }

    fn completion(
        fence: TaskFence,
        output: Result<ToolOutput, String>,
    ) -> BackgroundTaskCompletion {
        let end = match output {
            Ok(output) if output.state.is_empty() => BackgroundTaskEnd::Completed {
                content: output.content,
                is_error: output.is_error,
            },
            Ok(_) => BackgroundTaskEnd::Failed {
                message: "detached tool returned State commands; only its owning Runtime commit path may apply them".into(),
            },
            Err(message) => BackgroundTaskEnd::Failed { message },
        };
        BackgroundTaskCompletion { fence, end }
    }

    async fn launch(
        &self,
        task_id: BackgroundTaskId,
        fence: TaskFence,
        call: awaken_runtime_contract::tool::ToolCall,
        thread_id: awaken_runtime_contract::ThreadId,
        expected: awaken_ext_background_task::TaskExecutionPolicy,
    ) {
        let Some(cancellation) = self.supervisor.register(&task_id) else {
            return;
        };
        let mut context = self.context.clone();
        context.cancellation = Some(cancellation.clone());
        context.terminal_observers.clear();
        let prepared =
            match PreparedToolExecutor::new(self.runtime.clone(), &self.snapshot, context) {
                Ok(prepared) => Arc::new(prepared),
                Err(error) => {
                    self.supervisor
                        .complete(task_id, Self::completion(fence, Err(error.to_string())));
                    return;
                }
            };
        let resolved = match prepared.execution(&call) {
            Ok(resolved) => resolved,
            Err(error) => {
                self.supervisor
                    .complete(task_id, Self::completion(fence, Err(error.to_string())));
                return;
            }
        };
        if resolved.recovery != expected.recovery || resolved.concurrency != expected.concurrency {
            self.supervisor.complete(
                task_id,
                Self::completion(
                    fence,
                    Err("canonical tool execution facts changed after the committed claim".into()),
                ),
            );
            return;
        }
        let state = Store::rebuild(&self.commit.committed_state(&thread_id));
        let supervisor = self.supervisor.clone();
        let admission = self.admission.clone();
        let class = BackgroundWorkClass::SharedEnvironment {
            session_id: self.session_id.clone(),
            generation_id: self.environment_generation.clone(),
        };
        self.background
            .spawn(class, async move {
                let _admission = admission.acquire(expected.concurrency).await;
                let run_id =
                    awaken_runtime_contract::RunId(format!("background-{}", task_id.as_str()));
                let invocation = prepared.invoke(
                    &run_id,
                    &thread_id,
                    format!("background:{}", task_id.as_str()),
                    &call,
                    &state,
                );
                let result = tokio::select! {
                    result = invocation => result.map_err(|error| error.to_string()),
                    _ = cancellation.cancelled() => Ok(ToolOutput::error(
                        &call.call_id,
                        "background task cancelled",
                    )),
                };
                let completion = if cancellation.is_cancelled() {
                    BackgroundTaskCompletion {
                        fence,
                        end: BackgroundTaskEnd::Cancelled,
                    }
                } else {
                    Self::completion(fence, result)
                };
                supervisor.complete(task_id, completion);
            })
            .await;
    }

    async fn reconcile(&self, terminal: &CommittedTerminalRun) -> Result<(), String> {
        let state = Store::rebuild(&self.commit.committed_state(&terminal.thread_id));
        for task in tasks_from_state(&state).map_err(|error| error.to_string())? {
            match &task.lifecycle {
                BackgroundTaskLifecycle::Running { attempt }
                    if attempt.worker_id == self.supervisor.worker_id() =>
                {
                    self.launch(
                        task.id,
                        attempt.fence(),
                        task.invocation.call,
                        task.origin.thread_id,
                        attempt.policy.clone(),
                    )
                    .await;
                }
                BackgroundTaskLifecycle::Cancelling { attempt }
                    if attempt.worker_id == self.supervisor.worker_id() =>
                {
                    self.supervisor.cancel(&task.id);
                }
                BackgroundTaskLifecycle::Ended { .. } => self.supervisor.retire(&task.id),
                _ => {}
            }
        }
        Ok(())
    }
}

#[async_trait]
impl RunTerminalObserver for BackgroundTaskTerminalObserver {
    fn observer_id(&self) -> &str {
        OBSERVER_ID
    }

    async fn observe(
        &self,
        terminal: &CommittedTerminalRun,
    ) -> Result<(), RunTerminalObserverError> {
        self.reconcile(terminal)
            .await
            .map_err(RunTerminalObserverError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::commit::coordinator::Coordinator;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_ext_background_task::{
        BackgroundInvocation, BackgroundTask, BackgroundTaskConfig, BackgroundTaskOrigin,
        BackgroundTaskPlugin, TaskExecutionPolicy, task_state_cell,
    };
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ToolDescriptor};
    use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshotId};
    use awaken_runtime_contract::tool::{
        RawTool, ToolCall, ToolError, ToolOutputSpiller, ToolRecoveryPolicy, ToolResource,
        ToolResourceAccess,
    };

    fn write_claim(resource: &str) -> ToolConcurrency {
        ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource::new(
            "sandbox", resource,
        ))])
    }

    #[tokio::test]
    async fn admission_check_and_claim_publication_are_one_atomic_transition() {
        // Cause/effect decision table:
        // R1 no active claim + write(A) -> admit G1;
        // R2 G1 active + write(A) -> conflicting G2 remains blocked;
        // R3 G1 active + write(B) -> compatible G3 is admitted concurrently;
        // R4 G1 released -> G2 is admitted exactly once.
        // Constraint: compatibility check and active-map insertion are one
        // linearization point; otherwise two R1 arrivals can both observe an
        // empty set and violate R2.
        let admission = Arc::new(BackgroundAdmission::default());
        let first = admission.acquire(write_claim("a")).await;

        let mut conflicting = {
            let admission = admission.clone();
            tokio::spawn(async move { admission.acquire(write_claim("a")).await })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut conflicting)
                .await
                .is_err(),
            "R2: a conflicting claim must not pass the active owner"
        );

        let compatible = admission.acquire(write_claim("b")).await;
        drop(compatible);
        assert!(!conflicting.is_finished(), "R3 preserves the R2 conflict");

        drop(first);
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), conflicting)
            .await
            .expect("R4: release wakes the conflicting waiter")
            .expect("R4: waiter task does not panic");
        drop(second);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_conflicting_arrivals_never_overlap() {
        // Race rule R5: N write(A) arrivals are released together while the
        // active set is empty. Effect: every arrival eventually enters, but the
        // observed maximum active critical sections is exactly one. This is the
        // regression case for the former check-unlock-insert TOCTOU; the
        // sequential-owner test above covers blocking and wake behavior.
        let admission = Arc::new(BackgroundAdmission::default());
        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut arrivals = Vec::new();
        for _ in 0..16 {
            let admission = admission.clone();
            let barrier = barrier.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            arrivals.push(tokio::spawn(async move {
                barrier.wait().await;
                let guard = admission.acquire(write_claim("same")).await;
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }));
        }
        barrier.wait().await;
        for arrival in arrivals {
            arrival.await.expect("R5 arrival task");
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 1, "R5");
    }

    struct CountingTool(AtomicUsize);

    #[async_trait]
    impl RawTool for CountingTool {
        fn id(&self) -> &str {
            "count"
        }

        async fn invoke(
            &self,
            call: ToolCall,
        ) -> Result<ToolOutput, awaken_runtime_contract::tool::ToolError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::ok(call.call_id, "done"))
        }
    }

    struct SpillProbe(Arc<Mutex<Vec<(String, String, String)>>>);

    #[async_trait]
    impl ToolOutputSpiller for SpillProbe {
        async fn spill(
            &self,
            run_id: &RunId,
            call_id: &str,
            content: String,
        ) -> Result<String, ToolError> {
            self.0.lock().expect("spill probe mutex poisoned").push((
                run_id.0.clone(),
                call_id.to_string(),
                content,
            ));
            Ok(format!("artifact://{}/{call_id}", run_id.0))
        }
    }

    struct RecordingAfterTool(Arc<AtomicUsize>);

    #[async_trait]
    impl awaken_runtime_contract::plugin::PhaseHook for RecordingAfterTool {
        fn point(&self) -> awaken_runtime_contract::plugin::PhaseHookPoint {
            awaken_runtime_contract::plugin::PhaseHookPoint::AfterTool
        }

        async fn on_phase(
            &self,
            _ctx: &awaken_runtime_contract::plugin::PhaseContext,
            _conversation: &[awaken_runtime_contract::Message],
            _state: &Store,
        ) -> awaken_runtime_contract::plugin::HookReaction {
            self.0.fetch_add(1, Ordering::SeqCst);
            awaken_runtime_contract::plugin::HookReaction::default()
        }
    }

    struct RecordingAfterToolPlugin(Arc<AtomicUsize>);

    impl awaken_runtime_contract::plugin::Plugin for RecordingAfterToolPlugin {
        fn manifest(&self) -> awaken_runtime_contract::plugin::PluginManifest {
            awaken_runtime_contract::plugin::PluginManifest {
                id: "test.after-tool".into(),
                bound: awaken_runtime_contract::plugin::CapabilityBound {
                    phase_hooks: vec![awaken_runtime_contract::plugin::PhaseHookPoint::AfterTool],
                    ..Default::default()
                },
                ..Default::default()
            }
        }

        fn resolve(&self) -> awaken_runtime_contract::plugin::Contributions {
            let mut contributions =
                awaken_runtime_contract::plugin::Contributions::new("test.after-tool");
            contributions
                .phase_hooks
                .push(Arc::new(RecordingAfterTool(self.0.clone())));
            contributions
        }
    }

    fn snapshot() -> ExecutableAgentSnapshot {
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("background-snapshot".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("background-agent".into()),
            resolved_spec: awaken_runtime_contract::ResolvedSpec {
                catalog_fingerprint: CatalogFingerprint("background-catalog".into()),
                instructions: String::new(),
                max_steps: 1,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "native"),
                ),
                model_candidates: Vec::new(),
                tool_descriptors: vec![ToolDescriptor::pinned(
                    "test",
                    "count",
                    "Count once.",
                    serde_json::json!({"type":"object"}),
                )],
                plugin_ids: vec![awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.into()],
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("background-catalog".into()),
        }
    }

    #[tokio::test]
    async fn committed_claim_launches_once_and_records_completion_without_a_second_store() {
        // Cause graph: C1 Running task is absent from committed State -> observer
        // sees nothing; C2 the exact claim is committed -> duplicate observer
        // deliveries race; C3 supervisor registration wins once -> E1 one tool
        // effect, E2 one process-local completion, E3 no observer-side commit;
        // C4 another plugin owns an AfterTool workflow hook -> E4 detached target
        // execution never advances that foreground-only workflow event;
        // C5 the canonical output spiller is bound -> E5 completion retains only
        // its stable materialized result reference, not a parallel output store.
        let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker-test"));
        let plugin = Arc::new(BackgroundTaskPlugin::with_supervisor(
            BackgroundTaskConfig {
                tools: BTreeSet::from(["count".into()]),
            },
            supervisor.clone(),
        ));
        let counter = Arc::new(CountingTool(AtomicUsize::new(0)));
        let spills = Arc::new(Mutex::new(Vec::new()));
        let after_tool_calls = Arc::new(AtomicUsize::new(0));
        let runtime = Arc::new(
            Runtime::new()
                .with_plugin(plugin)
                .with_plugin(Arc::new(RecordingAfterToolPlugin(after_tool_calls.clone())))
                .with_tool(counter.clone()),
        );
        let memory = awaken_store_inmem::MemoryCommitCoordinator::new();
        let commit = Arc::new(HostCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(memory),
        )));
        let background = Arc::new(BackgroundRuns::new());
        let mut snapshot = snapshot();
        snapshot
            .resolved_spec
            .plugin_ids
            .push("test.after-tool".into());
        let observer = BackgroundTaskTerminalObserver::new(
            runtime,
            snapshot,
            RuntimeRunContext::new().with_tool_output_spiller(Arc::new(SpillProbe(spills.clone()))),
            commit.clone(),
            background.clone(),
            supervisor.clone(),
            "session".into(),
            "generation".into(),
        );
        let thread_id = ThreadId("thread".into());
        let origin_run = RunId("origin".into());
        let terminal = CommittedTerminalRun {
            run_id: origin_run.clone(),
            thread_id: thread_id.clone(),
            cause: awaken_runtime_contract::EndCause::NaturalEnd,
        };
        observer.observe(&terminal).await.expect("C1 observer");
        assert_eq!(counter.0.load(Ordering::SeqCst), 0, "C1");

        let mut task = BackgroundTask::requested(
            BackgroundTaskId::new("task-test").expect("task id"),
            BackgroundTaskOrigin {
                thread_id: thread_id.clone(),
                run_id: origin_run.clone(),
                operation_id: "operation".into(),
            },
            BackgroundInvocation {
                call: ToolCall {
                    call_id: "call".into(),
                    tool_id: "count".into(),
                    arguments: serde_json::json!({}),
                },
            },
        );
        let fence = task
            .start(
                supervisor.worker_id(),
                BackgroundTaskSupervisor::now_ms(),
                BackgroundTaskSupervisor::lease_ms(),
                TaskExecutionPolicy {
                    recovery: ToolRecoveryPolicy::default(),
                    concurrency: ToolConcurrency::Parallel,
                },
            )
            .expect("claim");
        commit
            .commit(ThreadCommit::assemble(
                thread_id,
                RunDisposition::ended(origin_run, awaken_runtime_contract::EndCause::NaturalEnd),
                true,
                Vec::new(),
                vec![
                    task_state_cell(&task.id)
                        .write(&task)
                        .expect("task command"),
                ],
                Vec::new(),
            ))
            .await
            .expect("C2 commit");
        observer.observe(&terminal).await.expect("first delivery");
        observer
            .observe(&terminal)
            .await
            .expect("duplicate delivery");
        assert!(background.drain(std::time::Duration::from_secs(2)).await);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1, "C3/E1");
        assert_eq!(after_tool_calls.load(Ordering::SeqCst), 0, "C4/E4");
        let completion = supervisor.completion(&task.id).expect("C3/E2 completion");
        assert_eq!(completion.fence, fence);
        assert!(matches!(
            &completion.end,
            BackgroundTaskEnd::Completed { content, is_error: false }
                if content == &vec![awaken_runtime_contract::ContentBlock::text(
                    "artifact://background-task-test/call"
                )]
        ));
        assert_eq!(
            spills
                .lock()
                .expect("spill probe mutex poisoned")
                .as_slice(),
            &[(
                "background-task-test".to_string(),
                "call".to_string(),
                "done".to_string(),
            )],
            "C5/E5: detached execution reuses the canonical stable spill identity"
        );
        assert!(
            supervisor.register(&task.id).is_none(),
            "completion remains a deduplication guard until durable Ended truth"
        );
    }
}
