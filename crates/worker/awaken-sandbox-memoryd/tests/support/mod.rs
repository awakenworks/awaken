use awaken_provisioning_contract::{MemoryMount, MemoryMountOutcome};

/// Test-only success extractor. Failure-path tests match the typed variants
/// directly so `NoEffect` and `RecoveryRequired` cannot be conflated.
pub trait MemoryMountOutcomeTestExt {
    fn mounted(self) -> Box<dyn MemoryMount>;
}

impl MemoryMountOutcomeTestExt for MemoryMountOutcome {
    fn mounted(self) -> Box<dyn MemoryMount> {
        match self {
            MemoryMountOutcome::Mounted(mount) => mount,
            MemoryMountOutcome::NoEffect(error) | MemoryMountOutcome::RecoveryRequired(error) => {
                panic!("expected mounted Memory participant: {error}")
            }
        }
    }
}
