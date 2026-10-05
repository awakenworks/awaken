// @formal surface obligation=credential.mcp_adopted_revocation_removes_accepting_generation_before_new_calls
// @formal surface obligation=credential.mcp_delivery_mode_is_frozen_and_never_falls_back
// @formal reason: Per-generation MCP material, wiring and projection state held by the Session slot; `mcp` owns staging and drain.
//! Private per-generation MCP material and the wiring discovered from it. The
//! Session slot holds these values; `mcp` builds them and `mcp_relay` serves them.

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
