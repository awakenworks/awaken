//! Exact Session Resource boundary adapters used by an execution Worker.

/// Transitional Session Resource wiring for an execution Worker.
///
/// File and Memory use dedicated claim-fenced network adapters assembled
/// separately. This value retains only immutable Skill access and the live
/// binding validator until their dedicated boundary adapters replace the
/// remaining shared-store access.
pub struct WorkerSessionResourceAdapters {
    pub(crate) skill_store: std::sync::Arc<dyn awaken_resource_contract::SkillStore>,
    pub(crate) validator: std::sync::Arc<dyn awaken_resource_contract::ResourceBindingValidator>,
}

impl WorkerSessionResourceAdapters {
    #[must_use]
    pub fn new(
        skill_store: std::sync::Arc<dyn awaken_resource_contract::SkillStore>,
        validator: std::sync::Arc<dyn awaken_resource_contract::ResourceBindingValidator>,
    ) -> Self {
        Self {
            skill_store,
            validator,
        }
    }
}
