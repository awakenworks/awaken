//! Explicit Resource dependencies used by an execution Worker.

/// Resource-plane wiring for an execution Worker.
///
/// These broad ports currently support shared-store composition. They remain
/// Resource-owned capabilities; their presence must not be described as
/// process-level authority-store isolation.
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
