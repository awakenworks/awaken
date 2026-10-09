// @formal entrypoint model_route.fallback_preserves_the_complete_publication_pin
//! Frozen dispatch-envelope resolution and exact root tool-format binding.

use super::placement::{
    InferencePlaintextHolderDecision, inference_plaintext_holder_decision, remote_worker_placement,
    self_hosted_holder_for_boundary, worker_local_credentials,
};
use super::*;

impl SharedHost {
    /// Resolve the one exact inference plaintext holder used by both direct and
    /// dispatched attempts from the immutable candidate publication.
    pub(crate) fn inference_plaintext_holder(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<awaken_runtime_contract::PlaintextHolder>, HostError> {
        Ok(match inference_plaintext_holder_decision(activation)? {
            InferencePlaintextHolderDecision::NotRequired => None,
            InferencePlaintextHolderDecision::Exact(holder) => Some(holder),
            InferencePlaintextHolderDecision::Boundary(boundary) => {
                let holder = self
                    .thread_credential_realization(&activation.thread_id.0)
                    .map_or_else(
                        || self_hosted_holder_for_boundary(boundary),
                        |profile| profile.inference_holder,
                    );
                if holder.boundary != boundary {
                    return Err(HostError::bad_request(
                        "the Session Environment credential holder conflicts with the selected model backend",
                    ));
                }
                Some(holder)
            }
        })
    }

    pub(crate) fn resolved_dispatch(
        &self,
        activation: RunActivation,
    ) -> Result<RunDispatch, HostError> {
        self.resolved_dispatch_with_traceparent_and_affinity(
            activation,
            awaken_observability::current_traceparent(),
            true,
            true,
        )
    }

    /// Resume the exact committed execution format. The ticket, rather than
    /// current Session admission, owns inherited configuration for this Run.
    pub(crate) fn resolved_resume_dispatch(
        &self,
        activation: RunActivation,
    ) -> Result<RunDispatch, HostError> {
        self.resolved_dispatch_with_traceparent_and_affinity(
            activation,
            awaken_observability::current_traceparent(),
            true,
            false,
        )
    }

    /// Bind the same fresh-root format before direct execution can commit an
    /// Awaiting ticket. Its later durable resume reads that frozen ticket.
    pub(crate) fn bind_fresh_root_tool_configuration(
        &self,
        activation: RunActivation,
    ) -> RunActivation {
        let is_session_dispatch = self
            .session_slots
            .read(&activation.thread_id.0, |slot| slot.session_dispatch)
            .unwrap_or(false);
        super::placement::bind_root_tool_configuration_format(activation, is_session_dispatch)
    }

    /// Decorate an extension-owned ordinary Run that shares a prepared
    /// Session's Thread and frozen execution inputs without converting it into
    /// a second Session-root admission. Outcome owns these Runs and their
    /// lifecycle; the Session aggregate remains only a conservative recovery
    /// candidate and must not acquire a parallel activity state machine.
    pub(crate) fn resolved_thread_extension_dispatch(
        &self,
        activation: RunActivation,
    ) -> Result<RunDispatch, HostError> {
        self.resolved_dispatch_with_traceparent_and_affinity(
            activation,
            awaken_observability::current_traceparent(),
            false,
            true,
        )
    }

    /// Decorate one activation using an explicit admission trace source. Session
    /// Event recovery supplies the context frozen in root provenance (including
    /// an explicit absence); ordinary foreground admission delegates here with
    /// its current validated OpenTelemetry context.
    pub(crate) fn resolved_dispatch_with_traceparent(
        &self,
        activation: RunActivation,
        traceparent: Option<String>,
    ) -> Result<RunDispatch, HostError> {
        self.resolved_dispatch_with_traceparent_and_affinity(activation, traceparent, true, true)
    }

    fn resolved_dispatch_with_traceparent_and_affinity(
        &self,
        activation: RunActivation,
        traceparent: Option<String>,
        associate_prepared_session: bool,
        fresh_activation: bool,
    ) -> Result<RunDispatch, HostError> {
        // `UNCONFIGURED_MODEL_REF` is a Coordinator-owned guidance executor, not a
        // remotely materializable model.  In a coordinator-only/all-in-one
        // deployment the registered Worker deliberately does not advertise the
        // generic host-executor capability, so enqueueing this activation can
        // never make progress.  Reject it before durable enqueue instead of
        // leaving a permanently pending run and reporting a misleading 60-second
        // dispatch-pool timeout.
        if activation.effective_model_ref() == crate::UNCONFIGURED_MODEL_REF {
            return Err(HostError::bad_request(
                "No model is configured. Connect a provider and select a model in Author > Quickstart before running this Agent.",
            ));
        }
        let thread = activation.thread_id.0.clone();
        let workspace = self.thread_workspace(&thread);
        let agent_publications = crate::agent_catalog::freeze_run_publications(
            &activation.snapshot,
            self.agent.agent_publications.as_deref(),
            &workspace,
        )
        .map_err(|error| {
            HostError::bad_request(format!(
                "cannot freeze Agent execution publications: {error}"
            ))
        })?;
        let (resources, runtime_projection, is_session_dispatch) = self
            .session_slots
            .read(&thread, |slot| {
                let runtime_projection = slot.environment_snapshot.clone().map(|environment| {
                    let mcp_stages = slot.mcp.active_stage_requests();
                    (environment, slot.tools.clone(), mcp_stages)
                });
                // Resource generation, Session identity, and runtime projection
                // come from one slot snapshot so dispatch cannot mix transition
                // generations. The slot owns the sole desired/active selector;
                // an Environment projection alone never promotes an ordinary
                // Run to Session identity.
                let is_session_dispatch = slot.session_dispatch;
                let resources = slot.dispatch_resource_manifest();
                (resources, runtime_projection, is_session_dispatch)
            })
            .unwrap_or_default();
        let activation = if fresh_activation {
            super::placement::bind_root_tool_configuration_format(activation, is_session_dispatch)
        } else {
            activation
        };
        // `SessionRuntime::install_session_projection` is the sole
        // Coordinator-side owner that installs this frozen runtime projection.
        // Preserve that existing
        // ownership fact in the dispatch contract so a registered Worker enters
        // the canonical Control-owned realization path before it constructs the
        // execution context. Resource manifests alone are deliberately
        // insufficient: ordinary Runs may carry one without being Sessions.
        let environment_snapshot = runtime_projection
            .as_ref()
            .map(|(environment, _, _)| environment);
        let inference_holder = self.inference_plaintext_holder(&activation)?;
        // A mixed deployment may have both the local pool and remote workers.
        // Any carried manifest still needs capability admission: an explicit empty
        // successor can be the operation that removes a prior projection.
        let worker_local = !worker_local_credentials(&activation.snapshot.resolved_spec).is_empty();
        let placement = (self.deployment.disable_local_pool || resources.is_some() || worker_local)
            .then(|| {
                remote_worker_placement(
                    &activation.snapshot.resolved_spec,
                    environment_snapshot,
                    resources.as_ref(),
                    self.deployment.disable_local_pool,
                )
            });
        let mut request = RunDispatch::new(activation)
            .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                awaken_tenancy::ScopeId::from(resources.as_ref().map_or_else(
                    || workspace.clone(),
                    |manifest| manifest.workspace_id.clone(),
                )),
            ))
            .with_traceparent(traceparent)
            .with_agent_publications(agent_publications);
        // Thread ownership survives an environment-free auxiliary Run. A
        // resource envelope is one carrier of Scope, not a prerequisite for it.
        if let Some(scope) = self.registered_thread_workspace(&thread) {
            if resources
                .as_ref()
                .is_some_and(|resources| resources.workspace_id != scope)
            {
                return Err(HostError::bad_request(
                    "Dispatch resource Workspace conflicts with its Thread",
                ));
            }
            request = request.with_execution_scope(awaken_tenancy::ExecutionScopeRef(scope.into()));
        }
        if associate_prepared_session && is_session_dispatch {
            request = request.for_session(ThreadId(thread.clone()));
        }
        if let Some(holder) = inference_holder {
            request = request.with_inference_plaintext_holder(holder);
        }
        if let Some(resources) = resources {
            let envelope =
                awaken_dispatch_contract::SessionResourceEnvelope::from_manifest(&resources)
                    .map_err(|error| {
                        HostError::internal(format!("serialize Session resource manifest: {error}"))
                    })?;
            request = request.with_session_resources(envelope);
        }
        if let Some((environment, tools, mcp_stages)) = runtime_projection {
            // Remote warm capacity is container-backed and therefore preserves
            // the frozen network policy. Derive the preference from the same
            // canonical SandboxSpec identity used by Worker pool receipts.
            let preferred_shape =
                crate::provisioning::environment_capacity_projection(&environment, true).shape_id;
            let envelope = awaken_dispatch_contract::SessionRuntimeEnvelope::from_projection(
                environment,
                tools,
                mcp_stages,
            )
            .map_err(|error| {
                HostError::internal(format!("serialize Session runtime projection: {error}"))
            })?;
            request = request
                .with_session_runtime(envelope)
                .with_preferred_environment_shape(preferred_shape);
        }
        if let Some(placement) = placement {
            request = request.with_placement(placement);
        }
        if request.activation.inherited_toolsets().is_some() {
            request.placement.required_capabilities.insert(
                awaken_runtime_contract::activation::INHERITED_TOOLSETS_CAPABILITY.to_string(),
            );
        }
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use crate::NoModelConfiguredExecutor;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime_contract::resolved::ModelBinding;
    use std::sync::Arc;

    #[tokio::test]
    async fn fresh_and_resumed_root_dispatch_use_the_ticket_format() {
        use awaken_agent_contract::agent::run::Id as RunId;
        use awaken_runtime_contract::RunActivation;
        use awaken_runtime_contract::activation::{
            INHERITED_TOOLSETS_CAPABILITY, InheritedToolsets,
        };

        // Causes: C1 a fresh Session root can author an awaiting ticket; C2 a
        // resumed Run carries frozen V1 sources; C3 a legacy ticket has none.
        // Effects: E1 direct and queued fresh roots bind the same marker; E2
        // resume preserves C2 and its Worker capability; E3 C3 stays unmarked
        // instead of silently changing an already-committed Run. The actual
        // await/reply/continuation is covered by Coordinator Outcome HITL.
        // | Rule | fresh | ticket sources | effect |
        // | R1 | yes | absent at admission | E1 |
        // | R2 | no | V1 empty | E2 |
        // | R3 | no | none | E3 |
        let host =
            (crate::HostDependencies::ephemeral(Arc::new(NoModelConfiguredExecutor), "model")
                .await)
                .build()
                .expect("validated Host dependencies");
        host.session_slots
            .update("format-root", |slot| slot.session_dispatch = true);
        let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
            .model(ModelBinding::parse("test", "model", "native").expect("valid fixture"))
            .build();
        let activation = RunActivation::new(
            RunId("format-run".into()),
            ThreadId("format-root".into()),
            snapshot,
            Vec::new(),
        );
        let direct = host.bind_fresh_root_tool_configuration(activation.clone());
        let queued = host
            .resolved_dispatch(activation.clone())
            .expect("R1 queue");
        assert_eq!(
            direct.inherited_toolsets(),
            queued.activation.inherited_toolsets(),
            "R1 direct and queued fresh paths freeze one format"
        );
        let frozen = InheritedToolsets::V1(Vec::new());
        assert_eq!(direct.inherited_toolsets(), Some(&frozen), "R1 marker");
        let resumed = host
            .resolved_resume_dispatch(activation.clone().with_inherited_toolsets(Some(frozen)))
            .expect("R2 resume");
        assert_eq!(
            resumed.activation.inherited_toolsets(),
            direct.inherited_toolsets(),
            "R2 frozen sources"
        );
        assert!(
            resumed
                .placement
                .required_capabilities
                .contains(INHERITED_TOOLSETS_CAPABILITY),
            "R2 Worker must understand the marker"
        );
        let legacy = host
            .resolved_resume_dispatch(activation)
            .expect("R3 legacy resume");
        assert_eq!(legacy.activation.inherited_toolsets(), None, "R3");
        assert!(
            !legacy
                .placement
                .required_capabilities
                .contains(INHERITED_TOOLSETS_CAPABILITY),
            "R3 must not invent a new ticket format"
        );
    }
}
