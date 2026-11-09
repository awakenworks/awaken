// @formal surface obligation=delegation.independent_child_scheduling_lease
// @formal reason: This sole child assembly selects the existing combined or split
// @formal reason: Worker constructor and installs one child-scoped recovery prefix.
// @formal reason: Claim, renewal, drive and settlement remain in Ingress; the
// @formal reason: existing child scheduling proof and behavior tests own evidence.
//! The one Host-side assembly for a durable child dispatch Worker.
//!
//! The Ingress Worker remains the only claim/lease/drive implementation. This
//! module selects its existing combined or split committed authority and installs
//! child-specific attempt wiring for parent delivery, cold recovery and cancel.

use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_credential_contract::CredentialRealizationCapabilities;
use awaken_dispatch_contract::Claimed;
use awaken_dispatch_erasure::{AnyDispatchStore, DispatchBinding};
use awaken_dispatch_execution::{DispatchWorker, Error, RecoveryProjection};
use awaken_runtime::Runtime;
use awaken_runtime_contract::execution::RunAttemptExecutor;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use super::RunScheduler;
use awaken_session_run_delivery::SessionCommit;

/// Assembles one child Worker from the already-selected dispatch and committed
/// authority. The three callers differ in their authority inputs and effects,
/// but never construct a second claim driver or recovery projection policy.
pub(crate) struct ChildWorkerFactory;

/// Exact execution wiring selected from the child's frozen publication and
/// parent Session substrate before the claimed Worker is assembled.
pub(crate) struct ClaimedChildAttempt {
    pub(crate) executor: Arc<dyn RunAttemptExecutor>,
    pub(crate) context: RuntimeRunContext,
    pub(crate) credentials: CredentialRealizationCapabilities,
    pub(crate) terminal_reads: awaken_dispatch_execution::ClaimedTerminalReads,
}

fn isolated_child_recovery_projection(
    binding: &DispatchBinding,
) -> Option<Arc<RecoveryProjection>> {
    (binding.local_run_commits().is_some()
        || matches!(
            binding.placement(),
            awaken_dispatch_erasure::DispatchTopology::Remote { .. }
        ))
    .then(|| Arc::new(RecoveryProjection::new()))
}

impl ChildWorkerFactory {
    /// Install one child-scoped observer over the same physical Session/commit,
    /// codec supervisor and claim read shared with this exact DispatchWorker.
    /// Parent observers are never inherited as child authority.
    pub(crate) fn background_context(
        runtime: Arc<Runtime>,
        snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
        commit: Option<&Arc<SessionCommit>>,
        mut context: RuntimeRunContext,
        adapters: &super::child_attempt::ChildExecutionAdapters,
    ) -> Result<
        (
            RuntimeRunContext,
            awaken_dispatch_execution::ClaimedTerminalReads,
        ),
        super::AgentRunError,
    > {
        let reads = awaken_dispatch_execution::ClaimedTerminalReads::default();
        context
            .terminal_observers
            .retain(|observer| observer.observer_id() != crate::background_task::OBSERVER_ID);
        if !snapshot
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID)
        {
            return Ok((context, reads));
        }
        let missing = |what: &str| {
            super::AgentRunError::Configuration(format!("child BackgroundTask requires {what}"))
        };
        let session_thread = adapters
            .session_thread_id
            .as_ref()
            .map(|thread| thread.0.as_str())
            .filter(|thread| !thread.trim().is_empty())
            .ok_or_else(|| missing("the selected physical Session identity"))?;
        let commit = commit.ok_or_else(|| missing("the original Session commit boundary"))?;
        let supervisor = adapters
            .background_supervisor
            .clone()
            .ok_or_else(|| missing("its original supervisor"))?;
        let background = adapters
            .background_runs
            .clone()
            .ok_or_else(|| missing("the original Session background lifetime"))?;
        let attention = adapters
            .background_attention
            .clone()
            .ok_or_else(|| missing("its Session attention route"))?;
        let generation = adapters
            .background_generation
            .clone()
            .ok_or_else(|| missing("the current physical Session generation"))?;
        let committed = crate::background_task::TaskCommittedReads::of(
            commit,
            &reads,
            adapters.task_control.as_ref(),
            session_thread,
        )
        .map_err(|message| super::AgentRunError::Configuration(message.into()))?;
        let observer = crate::background_task::BackgroundTaskTerminalObserver::new(
            runtime,
            snapshot.clone(),
            context.clone(),
            committed,
            background,
            supervisor,
            session_thread.to_owned(),
            generation,
            Some(attention),
        );
        context.terminal_observers.push(Arc::new(observer));
        Ok((context, reads))
    }

    pub(crate) async fn deliver_local_terminal(
        context: &RuntimeRunContext,
        commit: Option<&Arc<SessionCommit>>,
        thread: &ThreadId,
        run: &awaken_agent_contract::RunId,
    ) -> Result<(), super::AgentRunError> {
        if context.terminal_observers.is_empty() {
            return Ok(());
        }
        let error = |message: String| super::AgentRunError::Configuration(message);
        let commit = commit.filter(|commit| commit.committed_authority().is_some())
            .ok_or_else(|| error("direct child terminal observers require the original local committed authority".into()))?;
        let record = commit
            .authoritative_run(run)
            .await
            .map_err(error)?
            .ok_or_else(|| error("direct child terminal is not committed".into()))?;
        if &record.thread_id != thread {
            return Err(error(
                "direct child terminal belongs to another Thread".into(),
            ));
        }
        if let awaken_agent_contract::agent::run::RunState::Ended(cause) = record.state {
            let terminal = awaken_runtime_contract::terminal::CommittedTerminalRun {
                run_id: run.clone(),
                thread_id: thread.clone(),
                cause,
            };
            let failures = awaken_runtime_contract::terminal::deliver_committed_terminal(
                &context.terminal_observers,
                &terminal,
            )
            .await;
            if !failures.is_empty() {
                return Err(error(format!(
                    "direct child terminal delivery failed: {failures:?}"
                )));
            }
        }
        Ok(())
    }

    fn bound(
        runtime: Arc<Runtime>,
        binding: DispatchBinding,
        commit: Arc<SessionCommit>,
        reader: Arc<dyn CommittedThreadView>,
        owner: String,
    ) -> DispatchWorker<AnyDispatchStore> {
        if binding.local_run_commits().is_some() {
            DispatchWorker::new_bound(runtime, binding, commit, owner)
        } else {
            DispatchWorker::from_binding(runtime, binding, commit, reader, owner)
                .expect("compatibility binding supplies its commit authority")
        }
    }

    /// Parent-mediated start/resume. A remote parent's projection may only
    /// identify its own Run, so mint the child's separate projection before its
    /// claim; local execution instead reuses the Session-wide committed reader.
    pub(crate) fn parent_execution(
        runtime: Arc<Runtime>,
        scheduler: &RunScheduler,
        reader: Arc<dyn CommittedThreadView>,
        context: RuntimeRunContext,
        credentials: CredentialRealizationCapabilities,
        executor: Arc<dyn RunAttemptExecutor>,
        terminal_reads: awaken_dispatch_execution::ClaimedTerminalReads,
    ) -> (
        DispatchWorker<AnyDispatchStore>,
        Arc<dyn CommittedThreadView>,
    ) {
        let child_projection = isolated_child_recovery_projection(&scheduler.binding);
        let worker_reader: Arc<dyn CommittedThreadView> = child_projection
            .as_ref()
            .map_or_else(|| reader.clone(), |projection| projection.clone());
        let mut worker = Self::bound(
            runtime,
            scheduler.binding.clone(),
            scheduler.commit.clone(),
            worker_reader.clone(),
            scheduler.owner.clone(),
        )
        .with_context(context.clone())
        .with_claimed_terminal_reads(terminal_reads)
        .with_local_credential_capabilities(credentials);
        if context
            .model_request_gate
            .as_ref()
            .is_some_and(|gate| gate.process_bound())
            && let Some(factory) = &scheduler.claimed_model_request_gate
        {
            worker = worker.with_claimed_model_request_gate_factory(factory.clone());
        }
        worker.install_attempt_executor(executor);
        if let Some(projection) = child_projection {
            worker = worker
                .with_recovery_projection(projection)
                .expect("bound Worker supplies its required recovery source");
        }
        (worker, worker_reader)
    }

    /// A replacement cancels through the same durable child authority. It does
    /// not borrow the original child's runtime or trigger Session realization.
    pub(crate) fn cancellation(
        runtime: Arc<Runtime>,
        scheduler: &RunScheduler,
    ) -> DispatchWorker<AnyDispatchStore> {
        Self::bound(
            runtime,
            scheduler.binding.clone(),
            scheduler.commit.clone(),
            scheduler.reader.clone(),
            scheduler.owner.clone(),
        )
    }

    /// Cold claimed recovery starts from the existing combined SessionCommit
    /// boundary Worker. Its constructor installs the local RunRecoverySource;
    /// replacing it with `from_parts` would lose committed-snapshot recovery.
    pub(crate) async fn claimed_recovery(
        host: &crate::RunAssembly,
        claimed: &Claimed,
        commit: Arc<SessionCommit>,
        session_thread_id: &ThreadId,
        attempt: ClaimedChildAttempt,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
        let workspace_id = claimed.request.execution_scope.as_ref().map_or_else(
            || host.local_workspace().to_owned(),
            |scope| scope.0.0.clone(),
        );
        let context = attempt.context.with_model_content_materializer(Arc::new(
            crate::model_content_materializer::ResourceModelContentMaterializer::new(
                host.resources.file_content_source.clone(),
                workspace_id,
                claimed.request.thread_id().0.clone(),
                Some(awaken_worker_contract::RunClaim::from(&claimed.lease)),
            )
            .with_reader_session(Some(session_thread_id.0.clone())),
        ));
        let has_model_request_gate = context.model_request_gate.is_some();
        let commit_boundaries = crate::host::boundary_run::commit_boundary_workers(host);
        let mut worker = commit_boundaries
            .worker(claimed, commit, Some(context))
            .await?;
        // A cold/replacement child must resolve the same publication-pinned
        // model as its first attempt. The Worker's host fallback can be empty.
        if let Some(materializer) = host.worker_inference_materializer() {
            worker = worker.with_inference_materializer(materializer);
        }
        worker = worker.with_claimed_terminal_reads(attempt.terminal_reads);
        worker.install_attempt_executor(attempt.executor);
        worker = worker.with_local_credential_capabilities(attempt.credentials);
        if has_model_request_gate && let Some(endpoint) = host.capabilities.coordination_endpoint()
        {
            worker = worker.with_claimed_model_request_gate_factory(
                crate::coordination::claimed_model_request_gate_factory(
                    endpoint,
                    session_thread_id.0.clone(),
                ),
            );
        }
        Ok(Arc::new(worker))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_store_inmem::MemoryCommitCoordinator;

    #[tokio::test]
    async fn parent_child_assembly_keeps_local_truth_and_isolates_remote_prefix() {
        // Causes: C1 parent has local committed authority or a remote
        // claim-scoped projection; C2 the child is independently claimed.
        // Effects: E1 local child retains the exact Session-wide reader; E2
        // remote child receives a distinct empty projection and leaves the
        // parent's projection unchanged. Constraint: neither route mints
        // another durable commit store. Decision table: R1=local+C2=>E1;
        // R2=remote+C2=>E2. A shared remote projection would replace the
        // parent's claim prefix and fence its next otherwise-valid commit.
        let clock = awaken_time::system_clock();
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("test dispatch store"),
        );
        let commit = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
        let local_reader: Arc<dyn CommittedThreadView> = commit.clone();
        let runtime = Arc::new(Runtime::new());
        let executor: Arc<dyn RunAttemptExecutor> = runtime.clone();
        let scheduler = |binding, reader| RunScheduler {
            clock: clock.clone(),
            binding,
            commit: Arc::new(SessionCommit::Local(Arc::new(
                crate::LocalCommitAdapter::projected_shared(commit.clone()),
            ))),
            reader,
            owner: "child-owner".to_string(),
            session_resources: None,
            publication_source: None,
            claimed_model_request_gate: None,
        };
        let (_, selected_local) = ChildWorkerFactory::parent_execution(
            runtime.clone(),
            &scheduler(DispatchBinding::local(store.clone()), local_reader.clone()),
            local_reader.clone(),
            RuntimeRunContext::new(),
            Default::default(),
            executor.clone(),
            Default::default(),
        );
        assert!(Arc::ptr_eq(&selected_local, &local_reader), "R1/E1");

        let parent = Arc::new(RecoveryProjection::new());
        let parent_reader: Arc<dyn CommittedThreadView> = parent.clone();
        let (_, selected_remote) = ChildWorkerFactory::parent_execution(
            runtime,
            &scheduler(
                DispatchBinding::remote(
                    Arc::new(awaken_coordinator_client::HttpDispatchQueue::new(
                        "http://127.0.0.1:9",
                        awaken_worker_contract::WorkerIdentity::new("child", "boot", 1),
                    )),
                    Arc::new(awaken_coordinator_client::RemoteClaimedRunCommit::new(
                        "http://127.0.0.1:9",
                        awaken_worker_contract::WorkerIdentity::new("child", "boot", 1),
                    )),
                ),
                parent_reader.clone(),
            ),
            parent_reader.clone(),
            RuntimeRunContext::new(),
            Default::default(),
            executor,
            Default::default(),
        );
        assert!(!Arc::ptr_eq(&selected_remote, &parent_reader), "R2/E2");
        assert!(parent.current().is_none(), "R2 parent unchanged");
    }
}
