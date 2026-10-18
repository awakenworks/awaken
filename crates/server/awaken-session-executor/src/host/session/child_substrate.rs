// @formal entrypoint runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
//! Physical parent-Session capabilities borrowed by a separately dispatched child.

use super::*;

impl crate::RunAssembly {
    /// Realize only the parent Session's physical execution substrate for a
    /// separately dispatched child. Parent Agent plugins are runtime concerns
    /// and must never be constructed merely to borrow Environment/commit state.
    pub(crate) async fn session_child_execution_substrate(
        &self,
        thread: &str,
        frozen_parent: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<ChildExecutionSubstrate, HostError> {
        let realizations = self.installed_sessions.realizations();
        let lifecycle_guard = realizations.lock_lifecycle(thread).await?;
        self.realization_executor
            .environments()
            .retry_unpublished_session_environment_cleanup(thread)
            .await?;
        let warm_context = self
            .installed_sessions
            .read_run(thread, |run| run.context.clone())?
            .flatten();
        let retained = realizations
            .read_realization(thread, |r| r.environment().owner.resident())?
            .flatten();
        let environment = match retained {
            Some(existing) => existing,
            None => {
                let parent = frozen_parent
                    .cloned()
                    .map_or_else(
                        || realizations.published_snapshot(thread),
                        |parent| Ok(Some(parent)),
                    )?
                    .ok_or_else(|| {
                        HostError::internal(
                            "a cold child requires the frozen parent Session publication",
                        )
                    })?;
                if !awaken_session_realization_contract::requires_local_environment(
                    &parent.resolved_spec,
                ) {
                    return Err(HostError::internal(
                        "a delegated child has no parent Session environment",
                    ));
                }
                let provider = self
                    .realization_executor
                    .environments()
                    .projected_session_environment_provider(thread, None)?;
                self.realization_executor
                    .environments()
                    .create_reserved_session_environment_under_lifecycle(
                        lifecycle_guard.held(),
                        self.realization_executor.peers(),
                        thread,
                        provider,
                    )
                    .await?
            }
        };
        let environment = self
            .realization_executor
            .environments()
            .ensure_published_environment_reconciled_under_lifecycle(
                lifecycle_guard.held(),
                self.realization_executor.peers(),
                thread,
                environment,
            )
            .await?;
        drop(lifecycle_guard);
        self.child_execution_substrate_from_environment(thread, warm_context, environment)
            .await
    }

    async fn child_execution_substrate_from_environment(
        &self,
        thread: &str,
        warm_context: Option<Arc<SessionCtx>>,
        environment: Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<ChildExecutionSubstrate, HostError> {
        let parent_commit = match &warm_context {
            Some(context) => context.commit.clone(),
            None => self.thread_reads.commit_for_read(thread).await?,
        };
        let commit = parent_commit.for_independent_child();
        // Reconciliation may replace the physical owner. Even a warm Context
        // must capture the actual Resident Arc/generation, not retain an old
        // parent's label or infer origin from a current desired projection.
        let (_, resource_origin) = self
            .realization_executor
            .environments()
            .resident_environment_activity_identity(thread, &environment)?;
        let mut attempt_context = match warm_context {
            Some(context) => context.attempt_context.for_child_run(),
            None => {
                let workspace = self.capabilities.thread_workspace(thread)?;
                let mut context = awaken_runtime_contract::RuntimeRunContext::new()
                    .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                        awaken_tenancy::ScopeId::from(workspace),
                    ))
                    .with_tool_executor(environment.agent_tools().tool_executor())
                    .with_tool_output_spiller(Arc::new(
                        crate::tool_output_spill::SandboxToolOutputSpiller::new(
                            environment.clone(),
                        ),
                    ));
                for projection in self.active_mcp_projections(thread)? {
                    if let Some(wiring) = projection.native_wiring {
                        for plugin in wiring.plugins {
                            context = context.with_session_plugin(plugin);
                        }
                    }
                }
                if self
                    .installed_sessions
                    .read_run(thread, |run| run.session_dispatch)?
                    .unwrap_or(false)
                    && let Some(endpoint) = self.capabilities.coordination_endpoint()
                {
                    context = context.with_model_request_gate(Arc::new(
                        crate::coordination::HostModelRequestGate::new(endpoint, thread),
                    ));
                }
                context
            }
        };
        // Settlement owns claimed terminal observation. Inheriting a warm
        // parent's observers would enqueue the child through the wrong owner.
        attempt_context.terminal_observers.clear();
        attempt_context.resource_origin = resource_origin;
        Ok(ChildExecutionSubstrate {
            environment,
            commit,
            attempt_context,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, eager_environment, empty_frozen_projection_for_snapshot, test_activation,
    };
    use awaken_dispatch::WorkerResolver as _;
    use awaken_dispatch_contract::DispatchClaims;
    use awaken_dispatch_contract::DispatchEnqueue;
    use awaken_runtime_contract::resolved::ModelBinding;

    #[tokio::test]
    async fn cold_child_borrows_only_the_parent_physical_substrate() {
        // Cause/effect graph: C1 a child claim names a distinct parent Session;
        // C2 the frozen parent selects a custom auxiliary absent from the child
        // closure; C3 no parent Runtime is resident. Effects: E1 resolve the child
        // from its own empty exact closure; E2 materialize/reuse only the parent's
        // Environment and commit substrate; E3 never construct parent plugins or
        // a child Session slot. Rule S1=C1+C2+C3=>E1+E2+E3. The absent parent-only
        // auxiliary makes any accidental full parent context construction fail.
        // C4 installs the parent's complete Runtime projection and the child
        // dispatch's exact empty-Resource envelope through their canonical ports;
        // the child must not carry a competing Runtime projection for its parent.
        let parent_thread = "cold-physical-parent";
        let child_thread = "cold-physical-child";
        let parent = awaken_runtime_contract::ExecutableAgentSnapshot::builder("parent")
            .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()])
            .plugin_config([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                serde_json::json!({
                    "binding_id": "parent-only-memory",
                    "agent_id": "parent-only-extractor",
                    "recall_enabled": false
                }),
            )])
            .model(ModelBinding::new("test", "model", "native"))
            .build();
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(awaken_time::system_clock())
                .await
                .expect("dispatch store"),
        );
        let roots = crate::host::tests::ManagedSessionRootFixture::new().await;
        let host = roots
            .configure(
                crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
                    .await
                    .with_ephemeral_dispatch(store.clone()),
            )
            .build()
            .expect("validated Host dependencies");
        let managed = host.clone();
        let mut initial = empty_frozen_projection_for_snapshot(
            host.local_workspace(),
            eager_environment(),
            &parent,
        );
        initial.agent_publication = Some(parent.clone());
        let projection = roots
            .install_projection(&managed, parent_thread, initial)
            .await
            .expect("S1 actual parent Root and current lease");
        let transition = projection
            .resource_transition(
                awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
            )
            .unwrap();
        // C4 requires real parent authority while preserving the child-only
        // publication closure; never instantiate the parent's auxiliary plugins.
        let request = awaken_dispatch_contract::RunDispatch::new(test_activation(
            child_thread,
            "run-cold-physical-child",
        ))
        .for_session(ThreadId(parent_thread.into()))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from(projection.workspace_id.as_str()),
        ))
        .with_session_resources(
            awaken_dispatch_execution::session_envelope::resource_envelope(transition.desired())
                .unwrap(),
        );
        store.enqueue(request).await.expect("enqueue S1");
        let claimed = store
            .claim("worker-a", 30_000, &Default::default())
            .await
            .expect("claim S1")
            .expect("S1 available");
        crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
            .expect("Host dispatch resolver")
            .worker_for_claimed(&claimed)
            .await
            .expect("S1/E1 child resolve");
        let physical = host
            .run_assembly
            .installed_sessions
            .read(parent_thread, |slot| {
                (
                    slot.realization.environment().owner.is_resident(),
                    slot.run.context.is_some(),
                )
            })
            .expect("installed Sessions")
            .expect("S1 parent physical slot");
        assert_eq!(physical, (true, false), "S1/E2,E3");
        assert!(
            host.run_assembly
                .installed_sessions
                .read(child_thread, |_| ())
                .expect("installed Sessions")
                .is_none(),
            "S1/E3"
        );
    }
}
