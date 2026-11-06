// @formal entrypoint model_route.fallback_preserves_the_complete_publication_pin
//! Compiles one dispatch envelope from a correlated snapshot of installed inputs.
use crate::AdmissionError;
use crate::placement::{
    InferencePlaintextHolderDecision, inference_plaintext_holder_decision, remote_worker_placement,
    self_hosted_holder_for_boundary, worker_local_credentials,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::RunDispatch;
use awaken_runtime_contract::RunActivation;

/// One correlated read of the installed inputs a dispatch envelope carries.
#[derive(Default)]
pub struct InstalledDispatchInputs {
    pub resources: Option<awaken_session_configuration_contract::resource::SessionResourceManifest>,
    pub runtime_projection: Option<awaken_session_configuration_contract::SessionRuntimeProjection>,
    /// The Session entered through the Session application.
    pub session_dispatch: bool,
}

/// The installed Session as dispatch admission reads it: the existing Session
/// slot, never another registry or authority.
pub trait DispatchAdmissionReads: Send + Sync {
    /// The registered execution Workspace, or `fallback` for flat protocols.
    fn execution_workspace(&self, thread: &str, fallback: &str) -> Result<String, AdmissionError>;
    /// The registered execution Workspace; an unknown Thread has none.
    fn registered_workspace(&self, thread: &str) -> Result<Option<String>, AdmissionError>;
    /// The credential profile of the frozen Environment.
    fn credential_realization(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_credential_contract::CredentialRealizationProfile>, AdmissionError>;
    /// One correlated read of Resources, Session identity and Runtime/MCP inputs.
    fn installed_dispatch_inputs(
        &self,
        thread: &str,
    ) -> Result<InstalledDispatchInputs, AdmissionError>;
}

/// A borrowed Session registry reads as itself.
impl<T: DispatchAdmissionReads + ?Sized> DispatchAdmissionReads for &T {
    fn execution_workspace(&self, thread: &str, fallback: &str) -> Result<String, AdmissionError> {
        (**self).execution_workspace(thread, fallback)
    }
    fn registered_workspace(&self, thread: &str) -> Result<Option<String>, AdmissionError> {
        (**self).registered_workspace(thread)
    }
    fn credential_realization(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_credential_contract::CredentialRealizationProfile>, AdmissionError>
    {
        (**self).credential_realization(thread)
    }
    fn installed_dispatch_inputs(
        &self,
        thread: &str,
    ) -> Result<InstalledDispatchInputs, AdmissionError> {
        (**self).installed_dispatch_inputs(thread)
    }
}

/// Stable, non-provider identity of the model of a process with no executable
/// model. A Run that selects it can never be materialized by a Worker, so
/// admission refuses it with actionable guidance before it is enqueued.
pub const UNCONFIGURED_MODEL_REF: &str = "unconfigured";

enum PublicationInputs<'a> {
    Source(Option<&'a dyn awaken_runtime_contract::PublishedAgentSnapshotSource>),
    /// Original retained request data, never a role or caller-selected catalog.
    Frozen(Vec<awaken_runtime_contract::ExecutableAgentSnapshot>),
}

/// Values read from the existing Session slot, never another registry or authority.
pub struct DispatchPreparation<'a> {
    activation: RunActivation,
    sessions: Box<dyn DispatchAdmissionReads + 'a>,
    local_workspace: &'a str,
    agent_publications: PublicationInputs<'a>,
    publication_closure: awaken_runtime_contract::auxiliary_agents::RunPublicationClosure<'static>,
    local_execution: bool,
    model_configured: bool,
}

pub fn inference_plaintext_holder(
    activation: &RunActivation,
    profile: impl FnOnce() -> Result<
        Option<awaken_credential_contract::CredentialRealizationProfile>,
        AdmissionError,
    >,
) -> Result<Option<awaken_agent_contract::credential::PlaintextHolder>, AdmissionError> {
    Ok(match inference_plaintext_holder_decision(activation)? {
        InferencePlaintextHolderDecision::NotRequired => None,
        InferencePlaintextHolderDecision::Exact(holder) => Some(holder),
        InferencePlaintextHolderDecision::Boundary(boundary) => {
            let holder = profile()?.map_or_else(
                || self_hosted_holder_for_boundary(boundary),
                |p| p.inference_holder,
            );
            if holder.boundary != boundary {
                return Err(AdmissionError::rejected(
                    "the Session Environment credential holder conflicts with the selected model backend",
                ));
            }
            Some(holder)
        }
    })
}
impl<'a> DispatchPreparation<'a> {
    /// Retain the original readers without touching a slot. Admission owns the
    /// read order below; notably an unconfigured model never probes the registry.
    pub fn new(
        activation: RunActivation,
        sessions: Box<dyn DispatchAdmissionReads + 'a>,
        local_workspace: &'a str,
        agent_publications: Option<&'a dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
        publication_closure: awaken_runtime_contract::auxiliary_agents::RunPublicationClosure<
            'static,
        >,
        local_execution: bool,
    ) -> Self {
        let model_configured = activation.effective_model_ref() != UNCONFIGURED_MODEL_REF;
        Self {
            activation,
            sessions,
            local_workspace,
            agent_publications: PublicationInputs::Source(agent_publications),
            publication_closure,
            local_execution,
            model_configured,
        }
    }

    /// Use the original admitted bundle for a Task callback. The same resolve
    /// kernel validates it with exact_source and preserves its values verbatim.
    #[must_use]
    pub fn with_frozen_publications(
        mut self,
        publications: Vec<awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Self {
        self.agent_publications = PublicationInputs::Frozen(publications);
        self
    }

    pub fn fresh(self) -> Result<RunDispatch, AdmissionError> {
        self.resolve(awaken_observability::current_traceparent(), true, true)
    }
    pub fn resume(self) -> Result<RunDispatch, AdmissionError> {
        self.resolve(awaken_observability::current_traceparent(), true, false)
    }
    pub fn extension(self) -> Result<RunDispatch, AdmissionError> {
        self.resolve(awaken_observability::current_traceparent(), false, true)
    }
    /// A trusted caller already froze every activation field. Associate its
    /// Session without replacing even absent inherited-toolset evidence.
    pub fn prepared(self, traceparent: Option<String>) -> Result<RunDispatch, AdmissionError> {
        self.resolve(traceparent, true, false)
    }
    pub fn with_traceparent(
        self,
        traceparent: Option<String>,
    ) -> Result<RunDispatch, AdmissionError> {
        self.resolve(traceparent, true, true)
    }
    fn resolve(
        self,
        traceparent: Option<String>,
        associate_prepared_session: bool,
        fresh_activation: bool,
    ) -> Result<RunDispatch, AdmissionError> {
        // `UNCONFIGURED_MODEL_REF` is a Coordinator-owned guidance executor, not a
        // remotely materializable model.  In a coordinator-only/all-in-one
        // deployment the registered Worker deliberately does not advertise the
        // generic host-executor capability, so enqueueing this activation can
        // never make progress.  Reject it before durable enqueue instead of
        // leaving a permanently pending run and reporting a misleading 60-second
        // dispatch-pool timeout.
        if !self.model_configured {
            return Err(AdmissionError::rejected(
                "No model is configured. Connect a provider and select a model in Author > Quickstart before running this Agent.",
            ));
        }
        let thread = self.activation.thread_id.0.clone();
        let workspace = self
            .sessions
            .execution_workspace(&thread, self.local_workspace)?;
        let agent_publications = match self.agent_publications {
            PublicationInputs::Source(source) => self
                .publication_closure
                .freeze(&self.activation.snapshot, source, &workspace)
                .map_err(|error| {
                    AdmissionError::rejected(format!(
                        "cannot freeze Agent execution publications: {error}"
                    ))
                })?,
            PublicationInputs::Frozen(publications) => {
                self.publication_closure
                    .exact_source(&self.activation.snapshot, &publications, &workspace)
                    .map_err(|error| {
                        AdmissionError::Run(awaken_runtime_contract::runs::RunError::unavailable(
                            format!("retained Agent execution publications are invalid: {error}"),
                        ))
                    })?;
                publications
            }
        };
        let InstalledDispatchInputs {
            resources,
            runtime_projection,
            session_dispatch: is_session_dispatch,
        } = self.sessions.installed_dispatch_inputs(&thread)?;
        let activation = self.activation;
        let activation = if fresh_activation {
            crate::placement::bind_root_tool_configuration_format(activation, is_session_dispatch)
        } else {
            activation
        };
        // `SessionRuns::install_session_projection` is the sole
        // Coordinator-side owner that installs this frozen runtime projection.
        // Preserve that existing
        // ownership fact in the dispatch contract so a registered Worker enters
        // the canonical Control-owned realization path before it constructs the
        // execution context. Resource manifests alone are deliberately
        // insufficient: ordinary Runs may carry one without being Sessions.
        let environment_snapshot = runtime_projection
            .as_ref()
            .map(|projection| &projection.environment);
        let inference_holder = inference_plaintext_holder(&activation, || {
            self.sessions.credential_realization(&thread)
        })?;
        // A mixed deployment may have both the local pool and remote workers.
        // Any carried manifest still needs capability admission: an explicit empty
        // successor can be the operation that removes a prior projection.
        let worker_local = !worker_local_credentials(&activation.snapshot.resolved_spec).is_empty();
        let placement = (!self.local_execution || resources.is_some() || worker_local).then(|| {
            remote_worker_placement(
                &activation.snapshot.resolved_spec,
                environment_snapshot,
                resources.as_ref(),
                !self.local_execution,
            )
        });
        let mut request = RunDispatch::new(activation)
            .with_execution_scope(awaken_agent_contract::ExecutionScopeRef(
                awaken_agent_contract::ScopeId::from(resources.as_ref().map_or_else(
                    || workspace.clone(),
                    |manifest| manifest.workspace_id.clone(),
                )),
            ))
            .with_traceparent(traceparent)
            .with_agent_publications(agent_publications);
        // Thread ownership survives an environment-free auxiliary Run. A
        // resource envelope is one carrier of Scope, not a prerequisite for it.
        if let Some(scope) = self.sessions.registered_workspace(&thread)? {
            if resources
                .as_ref()
                .is_some_and(|resources| resources.workspace_id != scope)
            {
                return Err(AdmissionError::rejected(
                    "Dispatch resource Workspace conflicts with its Thread",
                ));
            }
            request = request
                .with_execution_scope(awaken_agent_contract::ExecutionScopeRef(scope.into()));
        }
        if associate_prepared_session && is_session_dispatch {
            request = request.for_session(ThreadId(thread.clone()));
        }
        if let Some(holder) = inference_holder {
            request = request.with_inference_plaintext_holder(holder);
        }
        if let Some(resources) = resources {
            let envelope = crate::session_envelope::resource_envelope(&resources)
                .map_err(AdmissionError::ResourceEnvelope)?;
            request = request.with_session_resources(envelope);
        }
        let mut session_mcp_credentials = None;
        if let Some(projection) = runtime_projection {
            // Remote warm capacity is container-backed and therefore preserves
            // the frozen network policy. Derive the preference from the same
            // canonical SandboxSpec identity used by Worker pool receipts.
            let preferred_shape =
                awaken_session_configuration_contract::environment_projection::environment_capacity_projection(
                    &projection.environment,
                    true,
                )
                .shape_id;
            // The Session validates its projection before the Run is durable and
            // publishes the MCP credentials a claim must satisfy; claim admission
            // compares them and never decodes the envelope.
            let (projection_json, requirements) =
                projection.dispatch().map_err(AdmissionError::from)?;
            session_mcp_credentials = Some(requirements);
            request = request
                .with_session_runtime(awaken_dispatch_contract::SessionRuntimeEnvelope::new(
                    projection_json,
                ))
                .with_preferred_environment_shape(preferred_shape);
        }
        if let Some(placement) = placement {
            request = request.with_placement(placement);
        }
        // Published after the placement is pinned: `with_placement` replaces it whole.
        request.placement.session_mcp_credentials = session_mcp_credentials;
        if request.activation.inherited_toolsets().is_some() {
            request.placement.required_capabilities.insert(
                awaken_runtime_contract::activation::INHERITED_TOOLSETS_CAPABILITY.to_string(),
            );
        }
        Ok(request)
    }
}
