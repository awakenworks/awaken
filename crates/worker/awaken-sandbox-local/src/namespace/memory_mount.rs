use awaken_provisioning_contract as pc;


/// Consume a temporary all-or-retain participant set. An all-Ok teardown makes
/// normal drop safe; any failure deliberately retains the complete set for the
/// process lifetime because no Sandbox owner exists to retry it. This is the
/// same fail-closed ownership rule as container staging compensation.
pub(crate) async fn compensate_memory_mounts(
    mounts: Vec<Box<dyn pc::MemoryMount>>,
) -> Result<(), pc::SandboxError> {
    match super::teardown_memory_mounts(&mounts).await {
        Ok(()) => Ok(()),
        Err(error) => {
            std::mem::forget(mounts);
            Err(error)
        }
    }
}
