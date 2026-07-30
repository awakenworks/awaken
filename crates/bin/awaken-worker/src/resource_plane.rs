//! Explicit Resource dependencies used by an execution Worker.

/// Resource-plane wiring for an execution Worker.
///
/// These Resource-owned capabilities support shared-store composition; their
/// presence must not be described as
/// process-level authority-store isolation.
pub struct WorkerResourcePlane {
    pub(crate) plane: awaken_runtime_host::ResourcePlane,
    pub(crate) validator: std::sync::Arc<dyn awaken_resource_contract::ResourceBindingValidator>,
    pub(crate) memory_mounter:
        Option<std::sync::Arc<dyn awaken_provisioning_contract::MemoryMounter>>,
}

impl WorkerResourcePlane {
    #[must_use]
    pub fn new(
        plane: awaken_runtime_host::ResourcePlane,
        validator: std::sync::Arc<dyn awaken_resource_contract::ResourceBindingValidator>,
    ) -> Self {
        Self {
            plane,
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
