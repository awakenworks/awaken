//! Typed evidence a Session environment returns when it stops its processes.

/// Typed evidence produced by the canonical Hand binding owner. A resident
/// Hand is deliberately reported as retained: dropping its channel is not
/// proof that the Session-Pod workload has terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandStopProof {
    NoBinding,
    AttachedProcessReaped,
    ResidentProcessRetained,
}

/// What stopping an environment's bound processes proved. Consumer: the
/// Session environment lifecycle owner, which requires it before revocation,
/// terminal harvest and filesystem disposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentStopProof {
    NoBoundProcesses,
    Hand(HandStopProof),
    #[cfg(target_os = "linux")]
    ExecutionTreeQuiescent,
}

/// Proof that no process of the environment can still write to its
/// filesystem. Consumer: checkpoint-and-release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvironmentQuiescenceProof;
