// @formal surface obligation=session_execution.terminal_state_never_reopens
// @formal surface obligation=session_runtime_interval.one_nonoverlapping_interval_closes_with_exact_fact
// @formal surface obligation=checkpoint.terminal_completion_clears_resume
// @formal reason: Terminal reconciliation from committed Run truth.
//! Direct committed-terminal repair: a Run already committed as terminal
//! delivers its terminal observation before ordinary Session construction may
//! reach the Environment lifecycle (dispatch side: `host::dispatch_reconciliation`).

use super::*;

/// Deliver one direct cold-recovery fact from the Runtime contract's sole
/// committed-terminal predicate. The observer factory is deliberately lazy:
/// nonterminal or conflicting identity cannot bind resources, probe an outbox,
/// or enter any later Session Environment effect.
async fn reconcile_direct_terminal_from_committed_truth<F, Fut>(
    reader: &dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView,
    run_id: &RunId,
    thread_id: &ThreadId,
    observer_factory: F,
) -> Result<bool, HostError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<
            Output = Result<
                Vec<Arc<dyn awaken_runtime_contract::terminal::RunTerminalObserver>>,
                HostError,
            >,
        >,
{
    let terminal = match awaken_runtime_contract::terminal::committed_terminal_projection(
        reader, run_id, thread_id,
    ) {
        awaken_runtime_contract::terminal::CommittedTerminalProjection::Nonterminal => {
            return Ok(false);
        }
        awaken_runtime_contract::terminal::CommittedTerminalProjection::IdentityConflict => {
            return Err(HostError::internal(
                "stable Run identity belongs to another Thread",
            ));
        }
        awaken_runtime_contract::terminal::CommittedTerminalProjection::Exact(terminal) => terminal,
    };
    let observers = observer_factory().await?;
    let failures =
        awaken_runtime_contract::terminal::deliver_committed_terminal(&observers, &terminal).await;
    if failures.is_empty() {
        return Ok(true);
    }
    Err(HostError::internal(format!(
        "direct committed-terminal observation failed: {}",
        failures
            .into_iter()
            .map(|failure| format!("{}: {}", failure.observer_id, failure.error))
            .collect::<Vec<_>>()
            .join("; ")
    )))
}

impl SharedHost {
    /// Repair direct-ingress terminal Memory work before ordinary Session
    /// construction can enter the physical Environment lifecycle. This is a
    /// cold recovery wake only: resident contexts already own their observer and
    /// durable/remote delivery has a guarded settlement owner.
    pub(in crate::host) async fn reconcile_direct_terminal_before_environment(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<bool, HostError> {
        if self.dispatch.durable_ingress()
            || !self.authority.observes_terminal_memory_locally()
            || self
                .installed_sessions
                .read_run(thread, |run| run.context.is_some())
                .unwrap_or(false)
        {
            return Ok(false);
        }
        let thread_id = ThreadId(thread.to_string());
        let commit = self.commit_for_read(thread).await?;
        let Some(latest) = commit.latest_run(&thread_id) else {
            return Ok(false);
        };
        reconcile_direct_terminal_from_committed_truth(
            commit.as_ref(),
            &latest.id,
            &thread_id,
            || async {
                let (workspace, _, installed, _) = self
                    .realizer
                    .publication
                    .resolve_session_publication(thread, agent, None)?;
                let frozen_model_ref = installed
                    .as_ref()
                    .map_or(self.agent.model_ref.as_str(), |root| {
                        root.resolved_spec.model_binding.identity().1
                    });
                let effective_model_ref = self
                    .agent
                    .inference_routing
                    .model_ref(thread, frozen_model_ref);
                let frozen = installed
                    .as_ref()
                    .map(|root| {
                        awaken_dispatch_execution::run_publications::freeze_run_publications(
                            root,
                            self.agent.agent_publications.as_deref(),
                            &workspace,
                        )
                    })
                    .transpose()
                    .map_err(HostError::bad_request)?
                    .unwrap_or_default();
                let source =
                    awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new(frozen)
                        .map_err(|error| {
                            HostError::bad_request(format!(
                                "invalid Agent publication closure: {error}"
                            ))
                        })?;
                self.direct_memory_terminal_observer(
                    thread,
                    installed.as_ref(),
                    &effective_model_ref,
                    &source,
                    commit.clone(),
                )
                .await
                .map(|observer| observer.into_iter().collect())
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::worker_resolver::test_support::AdoptionModel;
    use async_trait::async_trait;
    use awaken_agent_contract::agent::awaiting::ResumeTicket;
    use awaken_agent_contract::agent::message::Message;
    use awaken_agent_contract::agent::run::Record as RunRecord;
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TerminalProjectionView(Option<RunRecord>);

    impl CommittedThreadView for TerminalProjectionView {
        fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
            Vec::new()
        }

        fn run(&self, _run_id: &RunId) -> Option<RunRecord> {
            self.0.clone()
        }

        fn latest_run(&self, _thread_id: &ThreadId) -> Option<RunRecord> {
            self.0.clone()
        }

        fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
            None
        }

        fn open_wait_for_thread(&self, _thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
            // This terminal-only projection intentionally carries no awaiting
            // ticket, so it has no open wait to expose.
            None
        }
    }

    struct CountingTerminalObserver(Arc<AtomicUsize>);

    #[async_trait]
    impl awaken_runtime_contract::terminal::RunTerminalObserver for CountingTerminalObserver {
        fn observer_id(&self) -> &str {
            "counting-direct-terminal"
        }

        async fn observe(
            &self,
            _terminal: &awaken_runtime_contract::terminal::CommittedTerminalRun,
        ) -> Result<(), awaken_runtime_contract::terminal::RunTerminalObserverError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FailingTerminalExtractionRepository;

    #[async_trait]
    impl awaken_ext_memory::MemoryExtractionRepository for FailingTerminalExtractionRepository {
        async fn put_extraction_if_absent(
            &self,
            _intent: awaken_ext_memory::MemoryExtractionIntent,
        ) -> Result<
            awaken_ext_memory::PutMemoryExtractionOutcome,
            awaken_ext_memory::MemoryExtractionError,
        > {
            Err(awaken_ext_memory::MemoryExtractionError::Invalid(
                "scripted terminal observer failure".into(),
            ))
        }

        async fn get_extraction(
            &self,
            _intent_id: &str,
        ) -> Result<
            Option<awaken_ext_memory::MemoryExtractionIntent>,
            awaken_ext_memory::MemoryExtractionError,
        > {
            Err(awaken_ext_memory::MemoryExtractionError::Invalid(
                "scripted terminal observer failure".into(),
            ))
        }

        async fn extraction_cursor(
            &self,
            _thread_id: &str,
        ) -> Result<usize, awaken_ext_memory::MemoryExtractionError> {
            Ok(0)
        }

        async fn recoverable_extractions(
            &self,
            _limit: usize,
        ) -> Result<
            Vec<awaken_ext_memory::MemoryExtractionIntent>,
            awaken_ext_memory::MemoryExtractionError,
        > {
            Ok(Vec::new())
        }

        async fn compare_and_swap_extraction(
            &self,
            _expected_revision: u64,
            _intent: awaken_ext_memory::MemoryExtractionIntent,
        ) -> Result<(), awaken_ext_memory::MemoryExtractionError> {
            Err(awaken_ext_memory::MemoryExtractionError::Invalid(
                "scripted terminal observer failure".into(),
            ))
        }
    }

    struct CountingRejectedEnvironmentProvider(Arc<AtomicUsize>);

    #[async_trait]
    impl awaken_container_contract::ContainerEnvironmentProvider
        for CountingRejectedEnvironmentProvider
    {
        fn adoption_layout(
            &self,
            handle: &awaken_provisioning_contract::SandboxHandle,
        ) -> Result<
            awaken_container_contract::ContainerAdoptionLayout,
            awaken_provisioning_contract::SandboxError,
        > {
            awaken_sandbox_container::container_adoption_layout(handle)
        }

        fn physical_incarnation(
            &self,
            handle: &awaken_provisioning_contract::SandboxHandle,
        ) -> Result<String, awaken_provisioning_contract::SandboxError> {
            awaken_sandbox_container::container_physical_incarnation(handle)
        }

        async fn create_environment_for_effect(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
            _effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
            _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
            _intent: awaken_container_contract::EnvironmentRealizationIntent,
        ) -> Result<
            std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not implement fenced Environment creation",
            ))
        }
        async fn observe_environment(
            &self,
            _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
            _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
        ) -> Result<
            awaken_provisioning_contract::SandboxObservation,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not observe Environments",
            ))
        }
        async fn adopt_environment_for_effect(
            &self,
            _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
            _effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
            _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        ) -> Result<
            std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not implement fenced Environment adoption",
            ))
        }
        async fn prepare_terminal_environment_for_effect(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
            _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
            _expected_effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
            _terminal_effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
            _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        ) -> Result<
            Option<std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not implement fenced terminal preparation",
            ))
        }

        async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
            Ok(())
        }

        async fn create_unattached_environment(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
        ) -> Result<
            Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(awaken_provisioning_contract::SandboxError::backend(
                "create_environment",
                "scripted Environment entry",
            ))
        }

        async fn adopt_unattached_environment(
            &self,
            _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
        ) -> Result<
            Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::backend(
                "adopt_environment",
                "scripted Environment adoption",
            ))
        }
    }

    /// Direct cold-recovery cause/effect decision table:
    ///
    /// | Rule | committed Run | exact Thread identity | Expected effect |
    /// |------|---------------|-----------------------|-----------------|
    /// | R1 | absent/running/awaiting | any | do not construct or call an observer |
    /// | R2 | Ended | different | fail closed without constructing/calling an observer |
    /// | R3 | Ended | exact | construct once and deliver the exact committed fact |
    ///
    /// Constraint: `committed_terminal_projection` remains the only terminal
    /// predicate. This orchestration may sequence effects but cannot reinterpret
    /// Run state or caller-supplied Thread identity.
    #[tokio::test]
    async fn direct_terminal_recovery_constructs_observers_only_for_exact_ended_truth() {
        let run_id = RunId("run-direct-recovery".into());
        let thread_id = ThreadId("thread-direct-recovery".into());

        for state in [None, Some(RunState::Running), Some(RunState::Awaiting)] {
            let factory_calls = Arc::new(AtomicUsize::new(0));
            let observer_calls = Arc::new(AtomicUsize::new(0));
            let view = TerminalProjectionView(state.map(|state| RunRecord {
                id: run_id.clone(),
                thread_id: thread_id.clone(),
                state,
            }));
            let factory_counter = factory_calls.clone();
            let observer_counter = observer_calls.clone();
            let recovered = reconcile_direct_terminal_from_committed_truth(
                &view,
                &run_id,
                &thread_id,
                move || async move {
                    factory_counter.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![Arc::new(CountingTerminalObserver(observer_counter))
                        as Arc<
                            dyn awaken_runtime_contract::terminal::RunTerminalObserver,
                        >])
                },
            )
            .await
            .expect("R1 nonterminal projection");
            assert!(!recovered, "R1");
            assert_eq!(factory_calls.load(Ordering::SeqCst), 0, "R1 factory");
            assert_eq!(observer_calls.load(Ordering::SeqCst), 0, "R1 observer");
        }

        let factory_calls = Arc::new(AtomicUsize::new(0));
        let observer_calls = Arc::new(AtomicUsize::new(0));
        let foreign = TerminalProjectionView(Some(RunRecord {
            id: run_id.clone(),
            thread_id: ThreadId("foreign-thread".into()),
            state: RunState::Ended(EndCause::NaturalEnd),
        }));
        let factory_counter = factory_calls.clone();
        let observer_counter = observer_calls.clone();
        let conflict = reconcile_direct_terminal_from_committed_truth(
            &foreign,
            &run_id,
            &thread_id,
            move || async move {
                factory_counter.fetch_add(1, Ordering::SeqCst);
                Ok(vec![Arc::new(CountingTerminalObserver(observer_counter))
                    as Arc<
                        dyn awaken_runtime_contract::terminal::RunTerminalObserver,
                    >])
            },
        )
        .await
        .expect_err("R2 identity conflict");
        assert!(
            conflict.message.contains("another Thread"),
            "R2: {conflict}"
        );
        assert_eq!(factory_calls.load(Ordering::SeqCst), 0, "R2 factory");
        assert_eq!(observer_calls.load(Ordering::SeqCst), 0, "R2 observer");

        let factory_calls = Arc::new(AtomicUsize::new(0));
        let observer_calls = Arc::new(AtomicUsize::new(0));
        let exact = TerminalProjectionView(Some(RunRecord {
            id: run_id.clone(),
            thread_id: thread_id.clone(),
            state: RunState::Ended(EndCause::NaturalEnd),
        }));
        let factory_counter = factory_calls.clone();
        let observer_counter = observer_calls.clone();
        let recovered = reconcile_direct_terminal_from_committed_truth(
            &exact,
            &run_id,
            &thread_id,
            move || async move {
                factory_counter.fetch_add(1, Ordering::SeqCst);
                Ok(vec![Arc::new(CountingTerminalObserver(observer_counter))
                    as Arc<
                        dyn awaken_runtime_contract::terminal::RunTerminalObserver,
                    >])
            },
        )
        .await
        .expect("R3 exact terminal projection");
        assert!(recovered, "R3");
        assert_eq!(factory_calls.load(Ordering::SeqCst), 1, "R3 factory");
        assert_eq!(observer_calls.load(Ordering::SeqCst), 1, "R3 observer");
    }

    /// Cause/effect decision rule: C1 an exact committed terminal Run, C2 a
    /// writable direct Memory binding, and C3 terminal observer delivery fails.
    /// Effects: E1 `ctx_for` returns the observer error; E2 Session Environment
    /// creation is never entered. Constraint: an already-committed Run is not
    /// rewritten, and retry remains possible through the same observer/outbox.
    #[tokio::test]
    async fn direct_terminal_observer_failure_precedes_every_environment_effect() {
        use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};

        let creates = Arc::new(AtomicUsize::new(0));
        let host = (crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_session_container_provider(
                Arc::new(CountingRejectedEnvironmentProvider(creates.clone())),
                Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            )
            .with_memory_extraction_repository(Arc::new(FailingTerminalExtractionRepository)))
        .build()
        .expect("validated Host dependencies");
        let _managed = crate::host::tests::install_test_dispatch_runtime(&host);
        crate::host::tests::bind_test_memory(
            &host,
            "direct-terminal-observer-failure",
            "observer-failure-store",
            true,
        );
        let commit = host
            .authority
            .open_commit("direct-terminal-observer-failure")
            .await
            .expect("commit authority");
        commit_run(
            &commit,
            &ThreadId("direct-terminal-observer-failure".into()),
            RunDisposition::ended(
                RunId("direct-terminal-observer-failure-run".into()),
                EndCause::NaturalEnd,
            ),
            Vec::new(),
            Vec::new(),
        )
        .await
        .expect("terminal truth");

        let error = match host.ctx_for("direct-terminal-observer-failure", None).await {
            Ok(_) => panic!("C3/E1 terminal observer failure must abort recovery"),
            Err(error) => error,
        };
        assert!(
            error.message.contains("scripted terminal observer failure"),
            "E1: {error:?}"
        );
        assert_eq!(creates.load(Ordering::SeqCst), 0, "E2");
    }
}
