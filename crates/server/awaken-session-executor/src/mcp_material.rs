// @formal surface obligation=credential.mcp_adopted_revocation_removes_accepting_generation_before_new_calls
// @formal surface obligation=credential.mcp_delivery_mode_is_frozen_and_never_falls_back
// @formal reason: Per-generation MCP material, wiring and projection state held by the Session slot; `mcp` owns staging and drain.
//! Private per-generation MCP material and the wiring discovered from it. The
//! Session slot holds one [`SessionMcpState`]; `mcp` builds and drives it and
//! `mcp_relay` serves its routes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use awaken_ext_mcp::CredentialRefresher;
use awaken_runtime_contract::plugin::Plugin;

/// Private Worker-side material for one exact MCP generation. It is never an
/// authoring or desired-state value and cannot cross the Runtime Host boundary.
#[derive(Clone)]
pub(crate) struct McpTransportMaterial {
    pub name: String,
    pub prompts_as_skills: bool,
    pub transport: McpTransportMaterialKind,
}

#[derive(Clone)]
pub(crate) enum McpTransportMaterialKind {
    Http {
        url: String,
        bearer: Option<awaken_agent_contract::RedactedString>,
        refresh: Option<Box<McpRefreshMaterial>>,
    },
    SandboxStdio {
        command: String,
        args: Vec<String>,
    },
}

pub(crate) type HttpTransportMaterialRef<'a> = (
    &'a str,
    &'a Option<awaken_agent_contract::RedactedString>,
    &'a Option<Box<McpRefreshMaterial>>,
);

impl McpTransportMaterial {
    pub(crate) fn http(&self) -> Option<HttpTransportMaterialRef<'_>> {
        match &self.transport {
            McpTransportMaterialKind::Http {
                url,
                bearer,
                refresh,
            } => Some((url, bearer, refresh)),
            McpTransportMaterialKind::SandboxStdio { .. } => None,
        }
    }

    pub(crate) fn bearer(&self) -> Option<&awaken_agent_contract::RedactedString> {
        self.http().and_then(|(_, bearer, _)| bearer.as_ref())
    }

    pub(crate) fn http_url(&self) -> Option<&str> {
        self.http().map(|(url, _, _)| url)
    }

    pub(crate) fn refresh(&self) -> Option<Box<McpRefreshMaterial>> {
        self.http().and_then(|(_, _, refresh)| refresh.clone())
    }
}

/// The already-authorized refresh half of one prepared MCP server. Coordinator
/// authority owns the Vault and constructs this transport adapter; Runtime only
/// invokes the neutral refresh contract after an auth challenge.
#[derive(Clone)]
pub(crate) struct McpRefreshMaterial(pub(crate) Arc<dyn CredentialRefresher>);

/// The discovered surface of one thread's staged MCP servers. Each server is
/// represented by the canonical live [`awaken_ext_mcp::McpPlugin`], so model
/// descriptors and executable tools have one source of truth. The initial exact
/// ids are also retained for the Session permission gate.
#[derive(Clone)]
pub(crate) struct McpWiring {
    pub notification_transports: Vec<(String, Arc<dyn awaken_ext_mcp::McpToolTransport>)>,
    pub plugins: Vec<Arc<dyn Plugin>>,
    pub tool_ids: Vec<String>,
    pub skill_registries: Vec<Arc<dyn awaken_ext_skills::SkillRegistry>>,
    /// Exact-generation local call fences. The durable Session generation is
    /// still authoritative; drain closes these projections before acknowledging
    /// its receipt so cloned Runtime tools cannot continue using old material.
    pub call_fences: Vec<awaken_ext_mcp::transport::McpCallFence>,
}

impl McpWiring {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            notification_transports: Vec::new(),
            plugins: Vec::new(),
            tool_ids: Vec::new(),
            skill_registries: Vec::new(),
            call_fences: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpProjectionState {
    /// The exact generation owner is installed before process spawn/connect.
    /// Cancellation may leave this state temporarily, but never an unowned
    /// process; the owned staging task completes or drains it canonically.
    Staging,
    Staged,
    Active,
    Draining,
    Removed,
}

#[derive(Clone, Default)]
pub(crate) struct McpStagingActivity {
    finished: Arc<AtomicBool>,
    changed: Arc<tokio::sync::Notify>,
}

impl McpStagingActivity {
    pub(crate) fn guard(&self) -> McpStagingActivityGuard {
        McpStagingActivityGuard(self.clone())
    }

    pub(crate) fn finish(&self) {
        self.finished.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub(crate) async fn wait(&self) {
        while !self.finished.load(Ordering::Acquire) {
            let changed = self.changed.notified();
            if self.finished.load(Ordering::Acquire) {
                break;
            }
            changed.await;
        }
    }
}

pub(crate) struct McpStagingActivityGuard(McpStagingActivity);

impl Drop for McpStagingActivityGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// Process-local effect for one exact durable MCP generation. This is live
/// material/connection state only; desired state remains in the Session
/// aggregate and is never reconstructed from this slot.
#[derive(Clone)]
pub(crate) struct McpGenerationProjection {
    /// The one canonical, secret-free effect input. Keep it intact so durable
    /// dispatch can replay the exact generation on the execution Worker without
    /// reconstructing a parallel MCP configuration from live transport state.
    pub request: awaken_session_mcp_contract::StageMcpAttachment,
    /// Exact immutable realization identity with only renewable expiry/key
    /// excluded. It prevents a lease extension from changing target,
    /// credential, holder, owner incarnation, epoch, or logical generation.
    pub receipt: awaken_session_mcp_contract::McpRealizationReceipt,
    pub server: Option<crate::mcp_material::McpTransportMaterial>,
    pub native_wiring: Option<crate::mcp_material::McpWiring>,
    /// Session-owned process for a Native sandbox-stdio MCP generation. ACP
    /// starts its stdio child itself and HTTP generations have no sandbox
    /// process, so both leave this empty.
    pub mcp_process: Option<Arc<dyn awaken_provisioning_contract::ProcessHandle>>,
    /// Owned spawn/connect activity. Drain waits for it before proving that a
    /// process cannot appear after cleanup has completed.
    pub staging: Option<McpStagingActivity>,
    /// One cancellation-safe cleanup owner for this exact generation.
    pub drain: Arc<tokio::sync::Mutex<()>>,
    pub state: McpProjectionState,
}

/// Process-local admission fence for one exact durable suspend operation. It
/// lives beside the MCP effect owners in the canonical Session slot; it is not
/// durable lifecycle truth and cannot authorize checkpoint or restore.
///
/// Its identity is the whole operation coordinate: two fences are the same
/// only when every field matches, so a re-close is idempotent only for the
/// exact operation, source and generation (see [`Self::eq`]).
#[derive(Clone, Debug)]
pub struct McpQuiescenceAdmissionFence {
    pub operation_effect_id: String,
    pub activity_epoch: u64,
    pub source_effect_id: String,
    pub source_binding: String,
    pub generation: awaken_session_state_contract::SandboxGeneration,
}

impl PartialEq for McpQuiescenceAdmissionFence {
    /// Every coordinate is part of the identity; none is a label.
    fn eq(&self, other: &Self) -> bool {
        self.operation_effect_id == other.operation_effect_id
            && self.activity_epoch == other.activity_epoch
            && self.source_effect_id == other.source_effect_id
            && self.source_binding == other.source_binding
            && self.generation == other.generation
    }
}

impl Eq for McpQuiescenceAdmissionFence {}

impl McpQuiescenceAdmissionFence {
    pub fn new(
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_state_contract::SandboxGeneration,
    ) -> Self {
        Self {
            operation_effect_id: operation.effect_id.clone(),
            activity_epoch: operation.activity_epoch,
            source_effect_id: source_effect_id.to_owned(),
            source_binding: source_binding.to_owned(),
            generation: generation.clone(),
        }
    }

    pub fn restored_by(
        &self,
        environment: &awaken_session_state_contract::SessionEnvironmentState,
    ) -> bool {
        match environment {
            awaken_session_state_contract::SessionEnvironmentState::Resident {
                binding,
                effect_id: Some(effect_id),
                generation: Some(generation),
                ..
            } => {
                generation == &self.generation
                    && binding != &self.source_binding
                    && effect_id != &self.source_effect_id
                    && effect_id != &self.operation_effect_id
            }
            _ => false,
        }
    }
}

/// Everything one Session holds about MCP in its process-local slot: the
/// exact-generation effect owners, the quiescence admission fence, and whether
/// the Control-frozen projection names a nonterminal attachment. The slot
/// holds only this value; every other Host module reads or changes it through
/// the methods below.
#[derive(Default)]
pub struct SessionMcpState {
    pub(crate) projections: Vec<McpGenerationProjection>,
    /// Closed before quiescence snapshots MCP owners. Only the atomic durable
    /// projection install can reopen after proving a new exact Resident owner,
    /// or source-free Unmaterialized plus complete local cleanup.
    pub(crate) quiescence_fence: Option<McpQuiescenceAdmissionFence>,
    /// Whether the last Control-frozen projection contains a nonterminal MCP
    /// attachment. This supports lease supervision when effects live behind an
    /// injected realizer and therefore are not stored in `projections` locally.
    pub(crate) has_frozen_attachment: bool,
}

impl SessionMcpState {
    /// New MCP effects are admitted only while no quiescence fence is closed.
    #[must_use]
    pub fn admits_realization(&self) -> bool {
        self.quiescence_fence.is_none()
    }

    #[must_use]
    pub fn quiescence_fence(&self) -> Option<&McpQuiescenceAdmissionFence> {
        self.quiescence_fence.as_ref()
    }

    /// Close admission for one exact quiescence operation. Re-closing with the
    /// same fence is an idempotent `false`; another fence is a conflict.
    pub fn close_admission(
        &mut self,
        fence: McpQuiescenceAdmissionFence,
    ) -> Result<bool, crate::mcp::McpError> {
        match &self.quiescence_fence {
            Some(current) if current == &fence => Ok(false),
            Some(_) => Err(crate::mcp::McpError::internal(
                "another Session Environment quiescence fence is already installed",
            )),
            None => {
                self.quiescence_fence = Some(fence);
                Ok(true)
            }
        }
    }

    /// Reopen admission. The caller has proved the authoritative durable
    /// projection that alone may consume the fence.
    pub fn reopen_admission(&mut self) {
        self.quiescence_fence = None;
    }

    /// Every exact generation has been removed and owns no material, wiring,
    /// process, or staging activity.
    #[must_use]
    pub fn local_effects_cleared(&self) -> bool {
        self.projections.iter().all(|projection| {
            projection.state == McpProjectionState::Removed
                && projection.server.is_none()
                && projection.native_wiring.is_none()
                && projection.mcp_process.is_none()
                && projection.staging.is_none()
        })
    }

    /// The state that survives revocation of process-local realization: only
    /// an already-closed quiescence fence, so only the canonical durable
    /// projection proof may reopen MCP effects.
    #[must_use]
    pub fn take_retained_on_revocation(&mut self) -> Self {
        Self {
            quiescence_fence: self.quiescence_fence.take(),
            ..Self::default()
        }
    }

    /// The exact stage requests of every visible generation, as durable
    /// dispatch replays them on the execution Worker.
    #[must_use]
    pub fn active_stage_requests(&self) -> Vec<awaken_session_mcp_contract::StageMcpAttachment> {
        self.projections
            .iter()
            .filter(|projection| projection.state == McpProjectionState::Active)
            .map(|projection| projection.request.clone())
            .collect()
    }

    #[cfg(test)]
    #[must_use]
    pub fn has_frozen_attachment(&self) -> bool {
        self.has_frozen_attachment
    }

    pub fn set_has_frozen_attachment(&mut self, has_frozen_attachment: bool) {
        self.has_frozen_attachment = has_frozen_attachment;
    }

    /// Number of process-local generation owners, including tombstones.
    #[cfg(test)]
    #[must_use]
    pub fn projection_count(&self) -> usize {
        self.projections.len()
    }

    /// Test fixtures drive an owner into a state no production path reaches
    /// directly (for example a Staging owner with a live activity).
    #[cfg(test)]
    pub fn projections_mut(&mut self) -> &mut Vec<McpGenerationProjection> {
        &mut self.projections
    }
}
