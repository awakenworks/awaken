//! Physical parent-Session capabilities borrowed by a separately dispatched child.

use super::*;

impl SharedHost {
    /// Realize only the parent Session's physical execution substrate for a
    /// separately dispatched child. Parent Agent plugins are runtime concerns
    /// and must never be constructed merely to borrow Environment/commit state.
    pub(crate) async fn session_child_execution_substrate(
        &self,
        thread: &str,
        mut adopted: Option<crate::session_environment::SessionEnvironment>,
        frozen_parent: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<ChildExecutionSubstrate, HostError> {
        let lifecycle = self
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let lifecycle_guard = lifecycle.lock().await;
        self.retry_unpublished_session_environment_cleanup(thread)
            .await?;
        let warm_context = self
            .session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten();
        let retained = self
            .session_slots
            .read(thread, |slot| slot.environment_owner.resident())
            .flatten();
        let prepared = self.prepared_session_environment(thread);
        if let (Some(prepared), Some(extra)) = (&prepared, adopted.as_ref()) {
            if prepared.environment.handle() != extra.handle() {
                return Err(HostError::internal(format!(
                    "thread {thread} is already preparing a different sandbox"
                )));
            }
            extra
                .stop_bound_processes()
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            adopted = None;
        }
        if let Some(candidate) = prepared {
            let environment = self
                .complete_prepared_session_environment(thread, &candidate, true)
                .await?;
            drop(lifecycle_guard);
            return self
                .child_execution_substrate_from_environment(thread, warm_context, environment)
                .await;
        }
        let environment = match (retained, adopted) {
            (Some(existing), Some(adopted)) => {
                if existing.handle() != adopted.handle() {
                    return Err(HostError::internal(format!(
                        "thread {thread} is already bound to a different sandbox"
                    )));
                }
                adopted
                    .stop_bound_processes()
                    .await
                    .map_err(|error| HostError::internal(error.to_string()))?;
                existing
            }
            (Some(existing), None) => existing,
            (None, Some(adopted)) => {
                let environment = Arc::new(adopted);
                let candidate =
                    self.begin_session_environment_adoption(thread, environment.clone())?;
                self.complete_prepared_session_environment(thread, &candidate, false)
                    .await?
            }
            (None, None) => {
                if !self.session_environment_owner_is_vacant(thread) {
                    return Err(HostError::internal(
                        "a prior parent Session Environment transition requires lifecycle retry",
                    ));
                }
                let parent = frozen_parent
                    .cloned()
                    .or_else(|| {
                        self.session_slots
                            .read(thread, |slot| slot.published_snapshot.clone())
                            .flatten()
                    })
                    .ok_or_else(|| {
                        HostError::internal(
                            "a cold child requires the frozen parent Session publication",
                        )
                    })?;
                if !super::super::completion::requires_local_environment(&parent.resolved_spec) {
                    return Err(HostError::internal(
                        "a delegated child has no parent Session environment",
                    ));
                }
                let provider = self.session_environment_provider(
                    parent.resolved_spec.model_binding.provisioning(),
                )?;
                let environment = Arc::new(
                    self.create_session_environment(provider, &self.sandbox_spec(thread))
                        .await?,
                );
                let candidate =
                    self.begin_session_environment_preparation(thread, environment.clone())?;
                self.complete_prepared_session_environment(thread, &candidate, true)
                    .await?
            }
        };
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
        let commit = match &warm_context {
            Some(context) => context.commit.clone(),
            None => self.commit_for_read(thread).await?,
        };
        let mut attempt_context = match warm_context {
            Some(context) => context.attempt_context.for_child_run(),
            None => {
                let workspace = self.thread_workspace(thread);
                let mut context = awaken_runtime_contract::RuntimeRunContext::new()
                    .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                        awaken_tenancy::ScopeId::from(workspace),
                    ))
                    .with_tool_executor(environment.tool_executor())
                    .with_tool_output_spiller(Arc::new(
                        crate::tool_output_spill::SandboxToolOutputSpiller::new(
                            environment.clone(),
                        ),
                    ));
                for projection in self.active_mcp_projections(thread) {
                    if let Some(wiring) = projection.native_wiring {
                        for plugin in wiring.plugins {
                            context = context.with_session_plugin(plugin);
                        }
                    }
                }
                if self
                    .session_slots
                    .read(thread, |slot| slot.session_dispatch)
                    .unwrap_or(false)
                    && let Some(endpoint) = self.coordination_endpoint()
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
    use crate::host::worker_resolver::test_support::{AdoptionModel, test_activation};
    use awaken_run_ingress::{Clock as _, DispatchQueue as _, WorkerResolver as _};
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
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
        );
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        host.session_slots.update(parent_thread, |slot| {
            slot.published_snapshot = Some(parent);
            slot.agent_id = Some("parent".into());
        });
        let request = awaken_run_ingress::RunDispatch::new(test_activation(
            child_thread,
            "run-cold-physical-child",
        ))
        .for_session(ThreadId(parent_thread.into()));
        store.enqueue(request).await.expect("enqueue S1");
        let claimed = store
            .claim(
                "worker-a",
                30_000,
                awaken_run_ingress::SystemClock.now_ms(),
                &Default::default(),
            )
            .await
            .expect("claim S1")
            .expect("S1 available");
        HostWorkerResolver {
            host: Arc::downgrade(&host),
        }
        .worker_for_claimed(&claimed)
        .await
        .expect("S1/E1 child resolve");
        let physical = host
            .session_slots
            .read(parent_thread, |slot| {
                (slot.environment_owner.is_resident(), slot.runtime.is_some())
            })
            .expect("S1 parent physical slot");
        assert_eq!(physical, (true, false), "S1/E2,E3");
        assert!(
            host.session_slots.read(child_thread, |_| ()).is_none(),
            "S1/E3"
        );
    }
}
