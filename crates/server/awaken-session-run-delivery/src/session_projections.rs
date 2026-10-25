// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The Coordinator's projection of each Session's frozen root projection into the descriptors a
// @formal reason: dispatch envelope carries; it orders projections by the aggregate's receipt law and stages no
// @formal reason: Resource, Skill or MCP effect. Classification, not a formal-proof claim.
//! [`SessionProjections`]: the frozen dispatch projection of every Session
//! this Coordinator admits Runs for.
//!
//! The Session application owns each Session's frozen root projection. Before
//! a Run of the Session is admitted, it installs that projection here; Run
//! admission then lowers it into the descriptors a dispatch envelope carries:
//! the Resource manifest of the desired generation (whose Skill bindings pin
//! each bundle by its content digest), the Session runtime projection, and
//! the Session's effective Agent publication. Nothing is staged: the claiming
//! Worker's realization fetches every pinned Resource and Skill bundle from
//! the content-addressed store and refuses a missing or mismatched one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_dispatch_execution::AdmissionError;
use awaken_dispatch_execution::session_dispatch::{
    DispatchAdmissionReads, InstalledDispatchInputs,
};
use awaken_runtime_contract::resolved::ResolvedModelCandidate;
use awaken_runtime_contract::{ExecutableAgentSnapshot, PublishedAgentSnapshotSource};
use awaken_session_configuration_contract::resource::SessionResourceManifest;
use awaken_session_configuration_contract::{
    SessionBaseline, SessionRuntimeProjection, SessionToolConfiguration, environment_projection,
};
use awaken_session_realization_contract::{
    CanonicalSessionProjection, FrozenProjectionReceipt, FrozenSessionProjection,
};
use awaken_session_types::RunError;

use crate::DeliveryLock;

/// One Session's frozen projection as dispatch admission reads it.
#[derive(Clone, Debug)]
pub struct DispatchProjection {
    receipt: FrozenProjectionReceipt,
    workspace: String,
    baseline: SessionBaseline,
    publication: Option<ExecutableAgentSnapshot>,
    model_candidate: Option<ResolvedModelCandidate>,
    tools: SessionToolConfiguration,
    request_context: Vec<awaken_agent_contract::agent::message::Message>,
    manifest: SessionResourceManifest,
}

impl DispatchProjection {
    /// The Session's execution Workspace.
    #[must_use]
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// The Control-frozen baseline.
    #[must_use]
    pub fn baseline(&self) -> &SessionBaseline {
        &self.baseline
    }

    /// The Session's effective Agent publication, when its coordinates pin one.
    #[must_use]
    pub fn publication(&self) -> Option<&ExecutableAgentSnapshot> {
        self.publication.as_ref()
    }

    /// The exact model candidate the publication or model override selects.
    #[must_use]
    pub fn model_candidate(&self) -> Option<&ResolvedModelCandidate> {
        self.model_candidate.as_ref()
    }

    /// The Session's complete tool replacement.
    #[must_use]
    pub fn tools(&self) -> &SessionToolConfiguration {
        &self.tools
    }

    /// Request-only context materialized from the baseline's transcript prefix.
    #[must_use]
    pub fn request_context(&self) -> &[awaken_agent_contract::agent::message::Message] {
        &self.request_context
    }

    /// The Resource manifest of the desired generation a Run carries.
    #[must_use]
    pub fn manifest(&self) -> &SessionResourceManifest {
        &self.manifest
    }

    /// The runtime projection a Run carries. MCP attachments are staged by the
    /// claiming Worker's realization from the aggregate's realization
    /// directives, so the Coordinator stages none.
    #[must_use]
    pub fn runtime_projection(&self) -> SessionRuntimeProjection {
        SessionRuntimeProjection {
            environment: self.baseline.environment.clone(),
            tools: Some(self.tools.clone()),
            mcp_stages: Some(Vec::new()),
        }
    }
}

/// The frozen dispatch projections of this process's Sessions. Consumers:
/// the Session application, which installs each projection before it admits
/// a Run, and Run admission, which reads them.
pub struct SessionProjections {
    local_workspace: String,
    publications: Option<Arc<dyn PublishedAgentSnapshotSource>>,
    sessions: Mutex<HashMap<String, Arc<DispatchProjection>>>,
}

impl SessionProjections {
    /// Projections whose publications resolve from `publications`; a flat
    /// Thread with no projection runs in `local_workspace`.
    #[must_use]
    pub fn new(
        local_workspace: impl Into<String>,
        publications: Option<Arc<dyn PublishedAgentSnapshotSource>>,
    ) -> Self {
        Self {
            local_workspace: local_workspace.into(),
            publications,
            sessions: Mutex::default(),
        }
    }

    /// Install `projection` for `session`, replacing the held one only by
    /// the aggregate's receipt law.
    ///
    /// Decision table (causes → effect):
    /// | Rule | cause | effect |
    /// |---|---|---|
    /// | I1 | first projection with a pinned publication and a valid layout | installed: Workspace, desired manifest, runtime projection, effective publication |
    /// | I2 | the same projection again | an exact replay; the held projection stays |
    /// | I3 | an older revision | refused as stale (retryable) |
    /// | I4 | the same revision with other coordinates | refused as a conflict |
    /// | I5 | a newer revision with another baseline | refused: the baseline is frozen |
    /// | I6 | a newer revision with another publication | refused: the publication is immutable |
    /// | I7 | an empty baseline fingerprint | refused before anything is read |
    /// | I8 | a layout whose Repository nests in a mount | refused with the structural kernel's reason |
    /// | I9 | a pinned publication that is unavailable or mismatched | refused by the canonical selection |
    pub fn install(
        &self,
        session: &str,
        projection: FrozenSessionProjection,
    ) -> Result<(), RunError> {
        if projection.baseline.fingerprint.0.trim().is_empty() {
            return Err(RunError::internal(
                "frozen Session baseline fingerprint must not be empty",
            ));
        }
        let receipt = FrozenProjectionReceipt::from_projection(&projection)
            .map_err(|error| RunError::internal(error.to_string()))?;
        let held = self.projection(session)?;
        let replaces =
            FrozenProjectionReceipt::admit(held.as_ref().map(|held| &held.receipt), &receipt)?;
        if !replaces {
            return Ok(());
        }
        let (publication, model_candidate) =
            awaken_session_realization_contract::resolve_canonical_session_projection(
                self.publications.as_deref(),
                &projection.workspace_id,
                CanonicalSessionProjection::Baseline(&projection.baseline),
                projection.agent_publication.clone(),
                Some(projection.resources.skills()),
            )?;
        if let Some(held) = &held {
            if held.baseline.fingerprint != projection.baseline.fingerprint {
                return Err(RunError::internal(format!(
                    "thread {session} is already bound to a different frozen Session baseline"
                )));
            }
            if held.publication.is_some() && held.publication != publication {
                return Err(RunError::internal(
                    "Session cannot replace its immutable Agent publication",
                ));
            }
        }
        let environment =
            environment_projection::project_environment(&projection.baseline.environment);
        awaken_session_configuration_contract::sandbox_layout::validate_structural_sandbox_layout(
            session,
            projection
                .resources
                .inputs()
                .iter()
                .map(awaken_session_configuration_contract::managed_layout::resolved_input_layout_projection),
            &projection.baseline.mounts,
            &projection.baseline.env,
            Some(&environment),
        )
        .map_err(|error| RunError::bad_request(error.to_string()))?;
        let manifest = SessionResourceManifest::at_revision(
            projection.workspace_id.clone(),
            projection.resource_revision,
            projection.resources.clone(),
        );
        let installed = Arc::new(DispatchProjection {
            receipt,
            workspace: projection.workspace_id,
            baseline: projection.baseline,
            publication,
            model_candidate,
            tools: projection.tools,
            request_context: projection.request_context,
            manifest,
        });
        self.sessions
            .lock()
            .map_err(|_| DeliveryLock::SessionProjections)?
            .insert(session.to_owned(), installed);
        Ok(())
    }

    /// The projection installed for `session`, if any. A poisoned registry is
    /// refused, never read as an absent projection.
    pub fn projection(&self, session: &str) -> Result<Option<Arc<DispatchProjection>>, RunError> {
        Ok(self
            .sessions
            .lock()
            .map_err(|_| DeliveryLock::SessionProjections)?
            .get(session)
            .cloned())
    }

    /// Forget `session`'s projection (the Session ended).
    pub fn forget(&self, session: &str) -> Result<(), RunError> {
        self.sessions
            .lock()
            .map_err(|_| DeliveryLock::SessionProjections)?
            .remove(session);
        Ok(())
    }

    fn admission_projection(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<DispatchProjection>>, AdmissionError> {
        self.projection(thread).map_err(AdmissionError::Run)
    }
}

/// Dispatch admission reads the installed projection: an unknown Thread is a
/// flat-protocol Thread of the local Workspace with no Session inputs.
impl DispatchAdmissionReads for SessionProjections {
    fn execution_workspace(&self, thread: &str, fallback: &str) -> Result<String, AdmissionError> {
        Ok(self.admission_projection(thread)?.map_or_else(
            || fallback.to_owned(),
            |projection| projection.workspace.clone(),
        ))
    }

    fn registered_workspace(&self, thread: &str) -> Result<Option<String>, AdmissionError> {
        Ok(self
            .admission_projection(thread)?
            .map(|projection| projection.workspace.clone()))
    }

    fn credential_realization(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_credential_contract::CredentialRealizationProfile>, AdmissionError>
    {
        Ok(self.admission_projection(thread)?.map(|projection| {
            projection
                .baseline
                .environment
                .credential_realization
                .clone()
        }))
    }

    fn installed_dispatch_inputs(
        &self,
        thread: &str,
    ) -> Result<InstalledDispatchInputs, AdmissionError> {
        Ok(match self.admission_projection(thread)? {
            Some(projection) => InstalledDispatchInputs {
                resources: Some(projection.manifest.clone()),
                runtime_projection: Some(projection.runtime_projection()),
                session_dispatch: true,
            },
            None => InstalledDispatchInputs::default(),
        })
    }
}

impl SessionProjections {
    /// The Workspace a flat-protocol Thread runs in.
    #[must_use]
    pub fn local_workspace(&self) -> &str {
        &self.local_workspace
    }

    /// The Agent publications projections resolve their pins from.
    #[must_use]
    pub fn publications(&self) -> Option<&dyn PublishedAgentSnapshotSource> {
        self.publications.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{projection, publication};
    use awaken_runtime_contract::StaticPublishedAgentSnapshots;

    #[test]
    fn projection_installation_follows_its_decision_table() {
        let projections = SessionProjections::new("local", None);
        // D1: an unknown Thread is a flat Thread of the local Workspace.
        assert_eq!(
            projections.execution_workspace("session", "local").unwrap(),
            "local",
            "D1"
        );
        let unknown = projections.installed_dispatch_inputs("session").unwrap();
        assert!(
            unknown.resources.is_none()
                && unknown.runtime_projection.is_none()
                && !unknown.session_dispatch,
            "D1"
        );

        // I1: the first projection installs every dispatch descriptor.
        let published = publication("agent");
        projections
            .install(
                "session",
                projection(1, "provider/model", Some(published.clone())),
            )
            .expect("I1 install");
        let inputs = projections.installed_dispatch_inputs("session").unwrap();
        assert!(inputs.session_dispatch, "I1 Session dispatch");
        assert_eq!(
            inputs
                .resources
                .as_ref()
                .map(|manifest| manifest.workspace_id.as_str()),
            Some("workspace"),
            "I1 desired manifest"
        );
        let runtime = inputs.runtime_projection.expect("I1 runtime projection");
        assert_eq!(
            runtime.mcp_stages,
            Some(Vec::new()),
            "I1 no Coordinator MCP stage"
        );
        assert_eq!(
            runtime.tools,
            Some(SessionToolConfiguration::default()),
            "I1 tools"
        );
        let installed = projections
            .projection("session")
            .unwrap()
            .expect("I1 installed");
        assert_eq!(installed.publication(), Some(&published), "I1 publication");
        assert_eq!(
            installed.model_candidate(),
            Some(&published.resolved_spec.model_binding),
            "I1 candidate"
        );
        assert_eq!(
            projections
                .registered_workspace("session")
                .unwrap()
                .as_deref(),
            Some("workspace"),
            "I1 Workspace"
        );

        // I2: an exact replay keeps the held projection.
        projections
            .install(
                "session",
                projection(1, "provider/model", Some(published.clone())),
            )
            .expect("I2 replay");
        // I3: an older revision is a retryable stale read.
        let mut older = projection(0, "provider/model", Some(published.clone()));
        older.revision = awaken_session_types::SessionRevision(0);
        let stale = projections.install("session", older).expect_err("I3 stale");
        assert_eq!(stale.code, "session_projection_stale", "I3");
        // I4: the same revision with other coordinates conflicts.
        let mut conflicting = projection(1, "provider/model", Some(published.clone()));
        conflicting.resource_revision = 7;
        let conflict = projections
            .install("session", conflicting)
            .expect_err("I4 conflict");
        assert_eq!(
            conflict.code, "session_realization_invariant_violated",
            "I4"
        );
        // I5: a newer revision may not change the frozen baseline.
        let rebased = projections
            .install(
                "session",
                projection(2, "provider/other", Some(published.clone())),
            )
            .expect_err("I5 baseline");
        assert!(
            rebased
                .message
                .contains("different frozen Session baseline"),
            "I5"
        );
        // I6: a newer revision may not change the immutable publication.
        let mut replaced = publication("agent");
        replaced.fingerprint =
            awaken_runtime_contract::resolved::CatalogFingerprint("other".into());
        let immutable = projections
            .install("session", projection(2, "provider/model", Some(replaced)))
            .expect_err("I6 publication");
        assert!(
            immutable.message.contains("immutable Agent publication"),
            "I6"
        );

        // I7: an empty baseline fingerprint is refused before anything else.
        let mut unfingerprinted = projection(1, "provider/model", Some(published.clone()));
        unfingerprinted.baseline.fingerprint.0.clear();
        assert!(projections.install("empty", unfingerprinted).is_err(), "I7");
        assert!(
            projections.projection("empty").unwrap().is_none(),
            "I7 nothing installed"
        );

        // I8 is the structural kernel's own table
        // (`structural_layout_validation_follows_its_decision_table`); this
        // registry only routes the frozen layout through it.

        // I9: a pinned revision with no publication available is refused.
        let catalog = StaticPublishedAgentSnapshots::try_new([publication("agent")]).unwrap();
        let pinned = SessionProjections::new("local", Some(Arc::new(catalog)));
        let mut unavailable = projection(1, "provider/model", None);
        unavailable.baseline.agent_revision = Some(9);
        let refused = pinned.install("pinned", unavailable).expect_err("I9");
        assert_eq!(refused.code, "session_realization_invariant_violated", "I9");
    }
}
