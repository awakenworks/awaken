// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: The original Session slot owns every MCP preparation before external I/O.
// @formal reason: Exact root and allocation checks fence publication; the original activity remains visible to drain.
//! One preparation owner shared by HTTP, relay and sandbox-stdio attachments.

use crate::mcp::{SessionMcpStates, error::run_error};
use crate::mcp_material::{
    McpGenerationProjection, McpProjectionState, McpStagingActivity, McpTransportMaterial,
    McpWiring,
};
use awaken_runtime_contract::runs::RunError;
use awaken_session_configuration_contract::mcp::{McpRealizationReceipt, StageMcpAttachment};
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
                awaken_session_configuration_contract::resource::realization_lease_is_live_at(
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

/// The existing owned preparation task retains its effect until the original
/// slot can own cleanup or the exact allocation acknowledges compensation.
/// This is an operation capture, not another registry or generation authority.
pub(super) struct PlatformMcpPreparation {
    allocation: Arc<dyn awaken_credential_materializer::PlatformMcpEgressAllocation>,
    generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    drain: Arc<tokio::sync::Mutex<()>>,
    activity: crate::mcp_material::McpStagingActivityGuard,
}

impl PlatformMcpPreparation {
    pub(super) async fn finish<S: SessionMcpStates>(self, slots: Arc<S>) -> Result<(), RunError> {
        let handoff = slots
            .read_mcp(self.generation.session_id().as_str(), |mcp| {
                mcp.projections
                    .iter()
                    .any(|projection| Arc::ptr_eq(&projection.drain, &self.drain))
            })
            .map_err(run_error)
            .and_then(|retained| match retained {
                Some(true) => Ok(()),
                _ => Err(stale_owner()),
            });
        if handoff.is_ok() {
            return handoff;
        }
        // A failed observation cannot authorize a receipt or mean absence.
        // Close the original visible projection if its write port remains
        // available, then compensate before reporting the original refusal.
        let _ = slots.modify_mcp(self.generation.session_id().as_str(), |mcp| {
            if let Some(projection) = mcp
                .projections
                .iter_mut()
                .find(|projection| Arc::ptr_eq(&projection.drain, &self.drain))
            {
                projection.state = McpProjectionState::Draining;
            }
        });
        drop(slots);
        let Self {
            allocation,
            activity,
            ..
        } = self;
        let _activity = activity;
        // The caller can cancel its waiter, but cannot discard this original
        // owned task. Without the old slot, completion must await release.
        // Each attempt is bounded; failures retain only this allocation and
        // activity, never the Host or installed-Session registry.
        let mut retry = std::time::Duration::from_millis(100);
        loop {
            if matches!(
                tokio::time::timeout(
                    crate::mcp::sessions::MCP_GENERATION_CALL_QUIESCENCE_TIMEOUT,
                    allocation.revoke_and_wait(),
                )
                .await,
                Ok(Ok(()))
            ) {
                return handoff;
            }
            tracing::warn!("original Platform MCP preparation is awaiting release acknowledgement");
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(std::time::Duration::from_secs(30));
        }
    }
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
            platform_egress: None,
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

    /// Install the original release capability before provision can issue a
    /// route. A response lost or refused after the wait retains that owner.
    pub(super) async fn provision_platform(
        &mut self,
        provisioner: &dyn awaken_credential_materializer::PlatformMcpEgressProvisioner,
        admission: StagingAdmission,
        clock: &dyn awaken_time::WallClock,
        preparation: &mut Option<PlatformMcpPreparation>,
    ) -> Result<awaken_credential_materializer::MediatedMcpRoute, RunError> {
        let request = &self.projection.request;
        let (Some(access), Some(holder), Some(upstream_url)) = (
            request.credential.as_ref(),
            request.selected_plaintext_holder.as_ref(),
            request.target.http_url(),
        ) else {
            return Err(RunError::classified(
                "mcp_credential_binding_invalid",
                "Platform MCP preparation requires its original HTTP credential and holder",
            ));
        };
        let allocation =
            provisioner.prepare(awaken_credential_materializer::PlatformMcpEgressRequest {
                holder,
                credential: &access.credential,
                workspace_id: &request.workspace_id,
                session_id: request.generation.session_id().as_str(),
                attachment_id: &request.generation.attachment_id().0,
                generation: request.generation.generation().version(),
                realization_id: &request.realization_id,
                stage_idempotency_key: &request.stage_idempotency_key,
                runtime_incarnation: request.generation.runtime_incarnation(),
                lease_epoch: request.generation.lease_epoch().get(),
                upstream_url,
            });
        let owner = crate::mcp_material::McpPlatformEgress::new(allocation.clone());
        self.update(admission, clock, |projection| {
            if projection.platform_egress.is_some() {
                return Err(stale_owner());
            }
            projection.platform_egress = Some(owner.clone());
            Ok(())
        })?;
        self.projection.platform_egress = Some(owner);
        *preparation = Some(PlatformMcpPreparation {
            allocation: allocation.clone(),
            generation: self.projection.request.generation.clone(),
            drain: self.projection.drain.clone(),
            activity: self
                .projection
                .staging
                .take()
                .expect("original preparation activity")
                .guard(),
        });
        let route = allocation.provision().await.map_err(|error| {
            RunError::unavailable_classified(
                "mcp_gateway_route_unavailable",
                format!(
                    "mcp server `{}` mediated route is unavailable: {error}",
                    self.projection.request.name
                ),
            )
        })?;
        // Capturing cleanup material is valid after admission closes. Only the
        // exact original allocation may acquire this late physical result;
        // publication still crosses the ordinary authorization check below.
        self.slots
            .modify_mcp(
                self.projection.request.generation.session_id().as_str(),
                |mcp| {
                    let selected = mcp
                        .projections
                        .iter_mut()
                        .find(|p| self.selects(p))
                        .ok_or_else(stale_owner)?;
                    if !matches!(
                        selected.state,
                        McpProjectionState::Staging | McpProjectionState::Draining
                    ) {
                        return Err(stale_owner());
                    }
                    let owner = selected.platform_egress.as_mut().ok_or_else(stale_owner)?;
                    if owner
                        .route
                        .as_ref()
                        .is_some_and(|existing| existing != &route)
                    {
                        return Err(stale_owner());
                    }
                    owner.route = Some(route.clone());
                    Ok(())
                },
            )
            .map_err(run_error)?
            .ok_or_else(stale_owner)??;
        if let Some(owner) = self.projection.platform_egress.as_mut() {
            owner.route = Some(route.clone());
        }
        Ok(route)
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
                        if selected.mcp_process.is_some()
                            || selected.native_wiring.is_some()
                            || selected.platform_egress.is_some()
                        {
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
