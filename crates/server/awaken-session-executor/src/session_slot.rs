//! Process-local realization state for one Session.
//!
//! The frozen manifest remains the durable authority. This private slot owns every
//! projection materialized from it so registration and terminal cleanup share one
//! lifecycle boundary instead of coordinating parallel maps.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::host::PreparedMcpServer;
use crate::memory::BoundMemory;
use crate::provisioning::StagedResources;

#[derive(Default)]
pub(crate) struct SessionRuntimeSlot {
    /// Serializes first materialization/rebuild for this Session without a
    /// process-wide registry lock being held across I/O.
    pub lifecycle: Arc<tokio::sync::Mutex<()>>,
    pub runtime: Option<Arc<crate::host::SessionCtx>>,
    pub environment: Option<Arc<crate::session_environment::SessionEnvironment>>,
    pub workspace: Option<String>,
    pub model_ref: Option<String>,
    pub runtime_adapter: Option<String>,
    pub memory: Option<Arc<BoundMemory>>,
    pub mcp: Vec<PreparedMcpServer>,
    /// Claim-bound additions supplied by the embedding application. The host
    /// realizes these through the same Session environment as built-in resources.
    pub application: Option<crate::ApplicationSessionPlan>,
    /// Exact published delegation targets projected by a managed Session.
    pub delegates: Vec<String>,
    /// `Some([])` means the frozen manifest delivers no Skills; `None` means this
    /// embedded Session has no frozen Skill manifest.
    pub skills: Option<Vec<awaken_skill_store::SkillVersion>>,
    pub resources: StagedResources,
    pub manifest: Option<awaken_protocol_managed::SessionResourceManifest>,
    pub deny_egress: bool,
    pub sandbox: Option<awaken_provisioning_contract::SandboxOverride>,
}

#[derive(Clone, Default)]
pub(crate) struct SessionRuntimeSlots(Arc<Mutex<HashMap<String, SessionRuntimeSlot>>>);

impl SessionRuntimeSlots {
    pub fn read<R>(&self, session: &str, f: impl FnOnce(&SessionRuntimeSlot) -> R) -> Option<R> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .get(session)
            .map(f)
    }

    pub fn update<R>(&self, session: &str, f: impl FnOnce(&mut SessionRuntimeSlot) -> R) -> R {
        let mut slots = self.0.lock().expect("session runtime slots mutex poisoned");
        f(slots.entry(session.to_string()).or_default())
    }

    pub fn modify<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRuntimeSlot) -> R,
    ) -> Option<R> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .get_mut(session)
            .map(f)
    }

    pub fn remove(&self, session: &str) -> Option<SessionRuntimeSlot> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .remove(session)
    }

    #[cfg(test)]
    pub fn contains(&self, session: &str) -> bool {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .contains_key(session)
    }
}
