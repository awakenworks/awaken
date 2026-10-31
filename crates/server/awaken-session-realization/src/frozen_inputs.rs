//! Frozen Session values every runtime projection reads: the process-local
//! projections of the Session baseline and Environment snapshot. The installed Session holds them; provisioning, resource
//! layout and prompt rendering read them without reaching into the installed Session.

use crate::realization_error::RealizationError;
use awaken_session_configuration_contract::environment_projection::FrozenEnvironmentInputs;

/// Process-local reference to the exact Control-frozen baseline. Keeping the
/// domain value intact avoids a second partial publication/layout projection:
/// context, provider selection, cold replay, and cleanup all consume the same
/// immutable coordinates.
pub type FrozenBaselineInputs = awaken_session_configuration_contract::SessionBaseline;

/// One Session's frozen inputs as this process holds them. The installed Session
/// keeps one `Arc` of it and every change publishes a complete replacement, so
/// no reader observes a partly installed value; removing the installed Session drops it.
#[derive(Clone, Default)]
pub struct FrozenSessionInputs {
    pub workspace: Option<String>,
    /// Exact Agent identity from the frozen baseline. History/recovery calls
    /// carry no wire Agent and resolve the publication through it.
    pub agent_id: Option<String>,
    /// Exact immutable Agent publication supplied by the claimed activation;
    /// lease-only realization retains it because the catalog may lack it.
    pub published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
    pub model_ref: Option<String>,
    /// Process-local copy of the backend frozen in the Session baseline. It is
    /// validated against the immutable publication before runtime construction.
    pub backend_ref: Option<String>,
    /// Exact Environment projection shared by Native and ACP realization.
    pub environment_projection: Option<FrozenEnvironmentInputs>,
    /// Original secret-free Control snapshot retained solely for durable
    /// dispatch to another Worker. Runtime provisioning consumes the decoded
    /// projection above; this value is never a second configuration authority.
    pub environment_snapshot: Option<awaken_session_configuration_contract::EnvironmentSnapshot>,
    /// Exact Control-frozen baseline projected for realization. It is never
    /// authored or mutated locally.
    pub baseline: Option<FrozenBaselineInputs>,
    /// Exact published delegation targets projected by a managed Session.
    pub delegates: Vec<String>,
    /// Complete Session-local tool replacement; `None` inherits publication.
    pub tools: Option<awaken_session_configuration_contract::SessionToolConfiguration>,
    /// Rebuildable model-only context materialized from the Session baseline's
    /// immutable transcript-prefix reference. Never committed to this Thread.
    pub request_context: Vec<awaken_agent_contract::agent::message::Message>,
    /// The Coordinator's projection of the Registry-recorded process end of
    /// the Worker incarnation holding the Resident source. Read only through
    /// [`crate::session_realizations::SessionRealizations::recorded_source_end`].
    pub bound_source_end: Option<awaken_session_realization_contract::BoundSourceEnd>,
}

impl FrozenSessionInputs {
    /// A Session keeps the Agent publication it was frozen with; a different
    /// one is refused rather than replacing it.
    pub fn validate_publication(
        &self,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<(), RealizationError> {
        if publication.is_some_and(|publication| {
            self.published_snapshot
                .as_ref()
                .is_some_and(|retained| retained != publication)
        }) {
            return Err(RealizationError::invariant(
                "Session cannot replace its immutable Agent publication",
            ));
        }
        Ok(())
    }

    /// The one lowering of validated `SessionInit` coordinates into the next
    /// value, shared by the legacy preparation entrance and complete fenced
    /// projection publication.
    pub fn lower_session_init(
        &mut self,
        init: &awaken_session_configuration_contract::SessionInit,
        environment: FrozenEnvironmentInputs,
    ) {
        self.lower_environment(&init.environment, environment);
        self.workspace = Some(init.workspace_id.clone());
        self.agent_id = Some(init.agent_id.clone());
        if let Some(model) = &init.model {
            self.model_ref = Some(model.clone());
        }
        if let Some(backend) = &init.runtime {
            self.backend_ref = backend_projection(backend);
        }
        self.delegates = init.delegate_ids.clone();
        self.tools = init.tools.clone();
    }

    /// The first decoded Environment projection wins; callers reject a
    /// different fingerprint before reaching here.
    pub fn lower_environment(
        &mut self,
        snapshot: &awaken_session_configuration_contract::EnvironmentSnapshot,
        projection: FrozenEnvironmentInputs,
    ) {
        self.environment_projection.get_or_insert(projection);
        self.environment_snapshot = Some(snapshot.clone());
    }

    /// What revocation keeps: a retained Environment owner is retryable, so its
    /// provider selection, SandboxSpec inputs and harvest Workspace survive.
    pub fn retained_on_revocation(&self) -> Self {
        Self {
            workspace: self.workspace.clone(),
            published_snapshot: self.published_snapshot.clone(),
            environment_projection: self.environment_projection.clone(),
            environment_snapshot: self.environment_snapshot.clone(),
            baseline: self.baseline.clone(),
            // A durable Registry fact about the source's incarnation; the
            // retained owner's retry may still need it.
            bound_source_end: self.bound_source_end.clone(),
            ..Self::default()
        }
    }
}

/// The backend coordinate a Session pins; `default` inherits the publication.
pub fn backend_projection(backend_ref: &str) -> Option<String> {
    (backend_ref != "default").then(|| backend_ref.to_string())
}

impl FrozenSessionInputs {
    pub fn validate_environment(
        &self,
        thread: &str,
        projection: &FrozenEnvironmentInputs,
    ) -> Result<(), RealizationError> {
        if self
            .environment_projection
            .as_ref()
            .is_some_and(|existing| existing.fingerprint != projection.fingerprint)
        {
            return Err(RealizationError::invariant(format!(
                "thread {thread} is already bound to a different frozen Environment"
            )));
        }
        Ok(())
    }
}
