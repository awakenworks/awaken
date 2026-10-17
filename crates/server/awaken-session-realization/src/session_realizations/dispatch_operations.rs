// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
//! The named operations dispatch hand-off performs on an installed Session.
//!
//! Dispatch decides; these operations read or change the installed Session
//! under its one lock, so no caller outside the realization writes a field of
//! an installed Session directly.

use super::{SessionRealizationAccess, SessionRealizations};
use crate::realization_error::RealizationError;
use awaken_dispatch_contract::RunClaim;
use awaken_session_configuration_contract::SessionRuntimeProjection;
use awaken_session_configuration_contract::resource::{
    SessionRealizationLease, SessionResourceManifest,
};
use std::sync::Arc;

/// One correlated read of the installed inputs a dispatch envelope carries.
#[derive(Default)]
pub struct InstalledDispatchInputs {
    /// The Resource manifest the Run carries, if any.
    pub resources: Option<SessionResourceManifest>,
    /// The frozen runtime projection (Environment, tools, MCP stages).
    pub runtime_projection: Option<SessionRuntimeProjection>,
    /// The Session entered through the Session application, so its dispatch
    /// crosses the claimed Session realization boundary.
    pub session_dispatch: bool,
}

/// What a claimed resolve observes of the installed projection, and the
/// identity of the installation it observed.
pub struct InstalledProjection {
    /// The Session application published its dispatch marker.
    pub session_dispatch: bool,
    /// The Control-frozen baseline is installed.
    pub has_frozen_baseline: bool,
    /// The installation observed; a later bind requires the same one.
    pub installation: Arc<tokio::sync::Mutex<()>>,
}

/// The realization lease a retry of the same claim continues: the previous
/// exact claim (same Run and owner) with a live, matching lease proof. Read
/// during the claim's atomic admission, never after it.
pub fn claimed_realization_continuation(
    installed: &SessionRealizationAccess<'_>,
    claim: &RunClaim,
) -> Option<SessionRealizationLease> {
    installed
        .run
        .dispatch_claim()
        .filter(|previous| previous.run_id == claim.run_id && previous.owner == claim.owner)
        .and_then(|_| installed.realization.execution().authority.remaining())
        .and_then(|_| installed.realization.execution().authority.lease())
        .filter(|lease| lease.runtime_incarnation == claim.owner)
        .cloned()
}

impl SessionRealizations {
    /// Read the dispatch inputs of one installed Session; an absent Session
    /// has none.
    pub fn installed_dispatch_inputs(
        &self,
        session: &str,
    ) -> Result<InstalledDispatchInputs, RealizationError> {
        Ok(self
            .modify_session(session, |installed| {
                let runtime_projection =
                    installed
                        .inputs
                        .environment_snapshot
                        .clone()
                        .map(|environment| SessionRuntimeProjection {
                            environment,
                            tools: installed.inputs.tools.clone(),
                            mcp_stages: Some(
                                installed.realization.staging().mcp.active_stage_requests(),
                            ),
                        });
                let session_dispatch = installed.run.session_dispatch();
                InstalledDispatchInputs {
                    resources: installed
                        .realization
                        .staging()
                        .run_manifest(session_dispatch),
                    runtime_projection,
                    session_dispatch,
                }
            })?
            .unwrap_or_default())
    }

    /// Acquire the claimed resolve's realization permit and read, in the same
    /// admission, the lease a retry of the same claim continues.
    pub async fn acquire_claimed_realization(
        &self,
        session: &str,
        claim: &RunClaim,
    ) -> (
        awaken_session_realization_contract::SessionRealizationPermit,
        Option<SessionRealizationLease>,
    ) {
        self.acquire_realization_with(session, |installed| {
            claimed_realization_continuation(&installed, claim)
        })
        .await
    }

    /// Observe the installed projection; an absent Session is `None`.
    pub fn installed_projection(
        &self,
        session: &str,
    ) -> Result<Option<InstalledProjection>, RealizationError> {
        self.modify_session(session, |installed| InstalledProjection {
            session_dispatch: installed.run.session_dispatch(),
            has_frozen_baseline: installed.inputs.baseline.is_some(),
            installation: installed.realization.execution().phase.clone(),
        })
    }

    /// Install `claim` as the dispatch claim the Session serves, only while
    /// the observed installation is still the installed one and, when
    /// `require_installed_projection`, its frozen projection is still
    /// complete. Returns whether the claim was bound.
    pub fn bind_claim_if_projection_unchanged(
        &self,
        session: &str,
        observed: &Arc<tokio::sync::Mutex<()>>,
        require_installed_projection: bool,
        claim: RunClaim,
    ) -> Result<bool, RealizationError> {
        Ok(self
            .modify_session(session, |installed| {
                let unchanged = Arc::ptr_eq(observed, &installed.realization.execution().phase)
                    && (!require_installed_projection
                        || (installed.run.session_dispatch()
                            && installed.inputs.baseline.is_some()));
                if unchanged {
                    installed.run.install_dispatch_claim(claim);
                }
                unchanged
            })?
            .unwrap_or(false))
    }

    /// Bind the Session's execution Workspace. Returns false, changing
    /// nothing, when a different Workspace is already bound. This is an
    /// install path.
    pub fn bind_execution_workspace(
        &self,
        session: &str,
        workspace: &str,
    ) -> Result<bool, RealizationError> {
        let mut bound = true;
        self.replace_frozen_with(session, |next| {
            if next
                .workspace
                .as_deref()
                .is_some_and(|existing| existing != workspace)
            {
                bound = false;
            } else {
                next.workspace = Some(workspace.to_owned());
            }
        })?;
        Ok(bound)
    }

    /// Pin the Agent, backend and (when carried) Workspace a compatibility
    /// dispatch delivered. This is an install path.
    pub fn pin_dispatched_identity(
        &self,
        session: &str,
        agent_id: &str,
        backend_ref: &str,
        workspace: Option<&str>,
    ) -> Result<(), RealizationError> {
        self.replace_frozen_with(session, |next| {
            next.agent_id = Some(agent_id.to_owned());
            next.backend_ref = crate::frozen_inputs::backend_projection(backend_ref);
            if let Some(workspace) = workspace {
                next.workspace = Some(workspace.to_owned());
            }
        })
    }
}
