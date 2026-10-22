// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: The original Session slot owns every MCP preparation before external I/O.
// @formal reason: Exact root and allocation checks fence publication; the original activity remains visible to drain.
//! One preparation owner shared by HTTP, relay and sandbox-stdio attachments.

use crate::mcp::{SessionMcpStates, error::run_error};
use crate::mcp_material::{
    McpGenerationProjection, McpProjectionState, McpStagingActivity, McpTransportMaterial,
    McpWiring,
};
use awaken_session_configuration_contract::mcp::{McpRealizationReceipt, StageMcpAttachment};
use awaken_session_types::RunError;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(super) enum StagingAdmission {
    Direct,
    Assigned,
}

impl StagingAdmission {
    pub(super) fn admits(
        self,
        root: Option<bool>,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
        now: u64,
    ) -> bool {
        match self {
            Self::Assigned => root == Some(true),
            Self::Direct => root.unwrap_or_else(|| {
                awaken_session_realization_contract::realization_lease_is_live_at(
                    generation.lease_expires_at_unix_ms(),
                    now,
                )
            }),
        }
    }
}

pub(super) struct McpStagingOwner<S: SessionMcpStates> {
    slots: Arc<S>,
    projection: McpGenerationProjection,
    completed: bool,
}

impl<S: SessionMcpStates> McpStagingOwner<S> {
    pub(super) fn begin(
        slots: &Arc<S>,
        request: &StageMcpAttachment,
        admission: StagingAdmission,
        clock: &dyn awaken_time::WallClock,
    ) -> Result<Self, RunError> {
        if matches!(admission, StagingAdmission::Direct) {
            slots
                .update_mcp(request.generation.session_id().as_str(), |_| ())
                .map_err(run_error)?;
        }
        let projection = McpGenerationProjection {
            request: request.clone(),
            receipt: McpRealizationReceipt {
                generation: request.generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: request.selected_plaintext_holder.clone(),
                actual_realization_kind: None,
                receipt_fingerprint: request.fingerprint(),
            },
            server: None,
            native_wiring: None,
            mcp_process: None,
            staging: Some(McpStagingActivity::default()),
            drain: Arc::new(tokio::sync::Mutex::new(())),
            state: McpProjectionState::Staging,
        };
        slots
            .modify_mcp_with_root(&request.generation, |mcp, root| {
                if !admission.admits(
                    root,
                    &request.generation,
                    awaken_time::WallClockExt::now_ms(clock),
                ) {
                    return Err(stale_root());
                }
                if !mcp.admits_realization() {
                    return Err(quiescing());
                }
                if mcp
                    .projections
                    .iter()
                    .any(|p| p.request.generation == request.generation)
                {
                    return Err(RunError::unavailable_classified(
                        "mcp_generation_staging",
                        "MCP generation already has a preparation owner",
                    ));
                }
                mcp.projections.push(projection.clone());
                Ok(())
            })
            .map_err(run_error)?
            .ok_or_else(stale_root)??;
        Ok(Self {
            slots: slots.clone(),
            projection,
            completed: false,
        })
    }

    fn update<R>(
        &self,
        admission: StagingAdmission,
        clock: &dyn awaken_time::WallClock,
        update: impl FnOnce(&mut McpGenerationProjection) -> Result<R, RunError>,
    ) -> Result<R, RunError> {
        self.slots
            .modify_mcp_with_root(&self.projection.request.generation, |mcp, root| {
                if !admission.admits(
                    root,
                    &self.projection.request.generation,
                    awaken_time::WallClockExt::now_ms(clock),
                ) {
                    return Err(stale_root());
                }
                if !mcp.admits_realization() {
                    return Err(quiescing());
                }
                let selected = mcp
                    .projections
                    .iter_mut()
                    .find(|p| self.selects(p))
                    .ok_or_else(stale_owner)?;
                if selected.state != McpProjectionState::Staging {
                    return Err(draining());
                }
                update(selected)
            })
            .map_err(run_error)?
            .ok_or_else(stale_owner)?
    }

    fn selects(&self, projection: &McpGenerationProjection) -> bool {
        projection.request == self.projection.request
            && Arc::ptr_eq(&projection.drain, &self.projection.drain)
    }

    pub(super) fn authorize(
        &self,
        admission: StagingAdmission,
        clock: &dyn awaken_time::WallClock,
    ) -> Result<(), RunError> {
        self.update(admission, clock, |_| Ok(()))
    }

    pub(super) fn selection(&self) -> &McpGenerationProjection {
        &self.projection
    }

    pub(super) fn prepare_stdio(
        &mut self,
        admission: StagingAdmission,
        clock: &dyn awaken_time::WallClock,
        server: McpTransportMaterial,
        receipt: &McpRealizationReceipt,
    ) -> Result<(), RunError> {
        self.update(admission, clock, |projection| {
            projection.server = Some(server.clone());
            projection.receipt = receipt.clone();
            Ok(())
        })?;
        self.projection.server = Some(server);
        self.projection.receipt = receipt.clone();
        Ok(())
    }

    /// An already-issued spawn transfers its process to the original allocation,
    /// including when drain has since closed execution admission.
    pub(super) fn attach_process(
        &self,
        process: Arc<dyn awaken_provisioning_contract::ProcessHandle>,
        admission: StagingAdmission,
        clock: &dyn awaken_time::WallClock,
    ) -> Result<bool, RunError> {
        self.slots
            .modify_mcp_with_root(&self.projection.request.generation, |mcp, root| {
                let admitted = mcp.admits_realization()
                    && admission.admits(
                        root,
                        &self.projection.request.generation,
                        awaken_time::WallClockExt::now_ms(clock),
                    );
                let selected = mcp
                    .projections
                    .iter_mut()
                    .find(|p| self.selects(p))
                    .ok_or_else(stale_owner)?;
                if !matches!(
                    selected.state,
                    McpProjectionState::Staging | McpProjectionState::Draining
                ) || selected.mcp_process.is_some()
                {
                    return Err(stale_owner());
                }
                selected.mcp_process = Some(process);
                Ok(admitted && selected.state == McpProjectionState::Staging)
            })
            .map_err(run_error)?
            .ok_or_else(stale_owner)?
    }

    pub(super) fn publish(
        &mut self,
        admission: StagingAdmission,
        clock: &dyn awaken_time::WallClock,
        relay: Option<&crate::mcp_relay::McpRelay>,
        server: McpTransportMaterial,
        wiring: Option<McpWiring>,
        receipt: &McpRealizationReceipt,
    ) -> Result<(), RunError> {
        self.update(admission, clock, |projection| {
            if let Some(relay) = relay {
                relay
                    .stage_route_captured(&receipt.generation, &server)
                    .ok_or_else(|| {
                        RunError::classified(
                            "mcp_stale_generation",
                            "MCP relay route already exists",
                        )
                    })?;
            }
            projection.receipt = receipt.clone();
            projection.server = Some(server);
            projection.native_wiring = wiring;
            projection.staging = None;
            projection.state = McpProjectionState::Staged;
            Ok(())
        })?;
        self.completed = true;
        Ok(())
    }

    pub(super) fn finish_activity(&self) {
        if let Some(activity) = &self.projection.staging {
            activity.finish();
        }
    }
}

impl<S: SessionMcpStates> Drop for McpStagingOwner<S> {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.slots.modify_mcp(
                self.projection.request.generation.session_id().as_str(),
                |mcp| {
                    if let Some(selected) = mcp.projections.iter_mut().find(|p| self.selects(p))
                        && selected.state == McpProjectionState::Staging
                    {
                        if selected.mcp_process.is_some() || selected.native_wiring.is_some() {
                            selected.state = McpProjectionState::Draining;
                        } else {
                            selected.server = None;
                            selected.staging = None;
                            selected.state = McpProjectionState::Removed;
                        }
                    }
                },
            );
        }
        self.finish_activity();
    }
}

fn stale_root() -> RunError {
    RunError::classified(
        "mcp_stale_ownership",
        "MCP realization root is no longer current",
    )
}
fn stale_owner() -> RunError {
    RunError::unavailable_classified(
        "mcp_generation_owner_changed",
        "MCP preparation allocation changed",
    )
}
fn draining() -> RunError {
    RunError::unavailable_classified(
        "mcp_generation_draining",
        "MCP preparation admission is closed",
    )
}

fn quiescing() -> RunError {
    RunError::unavailable_classified(
        "session_environment_quiescing",
        "MCP staging is closed while the Session Environment quiesces",
    )
}
