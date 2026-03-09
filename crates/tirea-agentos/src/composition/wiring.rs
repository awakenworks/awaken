use std::collections::HashMap;
use std::sync::Arc;

use super::registry_set::RegistryBundle;
use super::AgentDefinition;
use super::AgentOsWiringError;
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::runtime::AgentBehavior;

/// Generic wiring interface for extension subsystems.
///
/// Each `SystemWiring` implementation encapsulates the tools, behaviors,
/// and registry bundles contributed by one extension subsystem. The
/// orchestrator iterates over registered wirings during `wire_into()`
/// instead of hardcoding calls per extension.
pub trait SystemWiring: Send + Sync {
    /// Unique identifier for this wiring (e.g. `"skills"`).
    fn id(&self) -> &str;

    /// Behavior IDs reserved by this subsystem. Users cannot reference these
    /// in `AgentDefinition.behavior_ids`.
    fn reserved_behavior_ids(&self) -> &[&'static str] {
        &[]
    }

    /// Produce wiring bundles for a specific agent definition.
    ///
    /// Called during `wire_into()`. The implementation receives the resolved
    /// user-defined behaviors and existing tool map for conflict detection.
    fn wire(
        &self,
        ctx: &WiringContext<'_>,
    ) -> Result<Vec<Arc<dyn RegistryBundle>>, AgentOsWiringError>;
}

/// Context passed to [`SystemWiring::wire`].
pub struct WiringContext<'a> {
    pub resolved_behaviors: &'a [Arc<dyn AgentBehavior>],
    pub existing_tools: &'a HashMap<String, Arc<dyn Tool>>,
    pub agent_definition: &'a AgentDefinition,
}
