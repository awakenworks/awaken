//! Exact Session Resource boundary adapters used by an execution Worker.

/// Transitional Session Resource wiring for an execution Worker.
///
/// File, Memory, and Skill use dedicated claim-fenced network adapters assembled
/// separately. This value retains only the live binding validator until the
/// remaining per-kind Resource boundary replaces direct catalog validation.
pub struct WorkerSessionResourceAdapters {
    pub(crate) validator: std::sync::Arc<dyn awaken_resource_contract::ResourceBindingValidator>,
}

impl WorkerSessionResourceAdapters {
    #[must_use]
    pub fn new(
        validator: std::sync::Arc<dyn awaken_resource_contract::ResourceBindingValidator>,
    ) -> Self {
        Self { validator }
    }
}
