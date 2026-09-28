// @formal surface exclude=projection_vocabulary
// @formal owner crates/contract/awaken-session-configuration-contract/src/lib.rs
// @formal owner crates/contract/awaken-session-execution-contract/src/session/roles.rs
// @formal owner crates/server/awaken-session-executor/src/host.rs
// @formal reason: SessionInit and SessionSandboxLayout are passive, secret-free typed projections
// @formal reason: assembled from already-authorized frozen Session configuration and Resource bindings.
// @formal reason: They define no transition, admission, fallback, persistence, retry, or physical effect;
// @formal reason: runtime roles and the Runtime Host remain the behavior owners.
use awaken_provisioning_contract::{EnvVar, MountRequirement};
use awaken_resource_contract::InputBinding;
use awaken_session_resource_contract::ResolvedSessionResources;

use crate::{
    EnvironmentSnapshot, SessionModelOverride, SessionRuntimePlacement, SessionToolConfiguration,
};

#[derive(Debug, Clone)]
pub struct SessionInit {
    pub workspace_id: String,
    pub agent_id: String,
    pub delegate_ids: Vec<String>,
    pub tools: Option<SessionToolConfiguration>,
    pub resource_revision: u64,
    pub resources: ResolvedSessionResources,
    pub model: Option<String>,
    pub runtime: Option<String>,
    pub environment: EnvironmentSnapshot,
}

#[derive(Debug, Clone)]
pub struct SessionSandboxLayout {
    pub workspace_id: String,
    pub agent_id: String,
    pub agent_revision: Option<u64>,
    pub runtime_placement: SessionRuntimePlacement,
    pub model_override: Option<SessionModelOverride>,
    pub runtime: Option<String>,
    pub mounts: Vec<MountRequirement>,
    pub env: Vec<EnvVar>,
    pub environment: EnvironmentSnapshot,
    pub resources: Vec<InputBinding>,
}
