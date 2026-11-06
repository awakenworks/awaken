//! The publication closure every Session Run carries.

use awaken_runtime_contract::auxiliary_agents::{AuxiliaryAgentDeclaration, RunPublicationClosure};

/// The publication closure of every Session Run: the auxiliary Agents of the
/// plugins a Worker composes into it, declared by those plugins. Admission
/// freezes it into each dispatch envelope, so the Coordinator that admits a
/// Run and the Worker that executes it read the same closure.
pub const RUN_PUBLICATIONS: RunPublicationClosure<'static> = RunPublicationClosure::new(&[
    &awaken_ext_memory::MemoryAuxiliaryAgents as &dyn AuxiliaryAgentDeclaration,
    &awaken_ext_compact::CompactAuxiliaryAgents,
]);
