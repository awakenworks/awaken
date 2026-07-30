//! Explicit data-plane dependencies used by a database-less Worker.

/// Resource-plane wiring for a database-less Worker.
///
/// The ports remain authoritative data-plane dependencies; the Worker only
/// materializes their already-authorized bindings for an attempt.
pub struct WorkerResourcePlane {
    pub(crate) ports: awaken_runtime_host::ResourcePlanePorts,
    pub(crate) validator: std::sync::Arc<dyn awaken_resource_contract::ResourceBindingValidator>,
    pub(crate) memory_mounter:
        Option<std::sync::Arc<dyn awaken_provisioning_contract::MemoryMounter>>,
}

impl WorkerResourcePlane {
    #[must_use]
    pub fn new(
        ports: awaken_runtime_host::ResourcePlanePorts,
        validator: std::sync::Arc<dyn awaken_resource_contract::ResourceBindingValidator>,
    ) -> Self {
        Self {
            ports,
            validator,
            memory_mounter: None,
        }
    }

    #[must_use]
    pub fn with_memory_mounter(
        mut self,
        mounter: std::sync::Arc<dyn awaken_provisioning_contract::MemoryMounter>,
    ) -> Self {
        self.memory_mounter = Some(mounter);
        self
    }
}
