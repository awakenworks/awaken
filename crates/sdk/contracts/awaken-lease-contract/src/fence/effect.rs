// @formal evidence sandbox.effect_fence_succession_is_renewal_or_epoch_advance
//! A lease projected onto one exact operation.

use std::fmt;

use super::{Lease, TenureScope};

/// The authority for one effect: the operation it performs and the lease it
/// performs it under.
///
/// Operation identity is compared here, once, beside the lease law, so a
/// caller never combines lease monotonicity with a separately guessed
/// operation comparison. Consumer: `awaken_provisioning_contract`'s
/// `SandboxEffectFence`, the provider-neutral fence of one physical Sandbox
/// effect.
pub struct EffectFence<S: TenureScope, Op> {
    operation: Op,
    lease: Lease<S>,
}

impl<S: TenureScope, Op: Eq> EffectFence<S, Op> {
    #[must_use]
    pub const fn new(operation: Op, lease: Lease<S>) -> Self {
        Self { operation, lease }
    }

    #[must_use]
    pub const fn operation(&self) -> &Op {
        &self.operation
    }

    #[must_use]
    pub const fn lease(&self) -> &Lease<S> {
        &self.lease
    }

    /// The same operation under the same tenure, ignoring the renewable
    /// expiry.
    #[must_use]
    pub fn same_effect_identity(&self, other: &Self) -> bool {
        self.operation == other.operation && self.lease.tenure() == other.lease.tenure()
    }

    /// Whether `successor` may take over this exact operation: the same
    /// operation, under a renewal or reassignment of this lease.
    #[must_use]
    pub fn authorizes_effect_successor(&self, successor: &Self) -> bool {
        self.operation == successor.operation && self.lease.authorizes_successor(&successor.lease)
    }
}

impl<S: TenureScope, Op: Clone> Clone for EffectFence<S, Op> {
    fn clone(&self) -> Self {
        Self {
            operation: self.operation.clone(),
            lease: self.lease.clone(),
        }
    }
}

impl<S: TenureScope, Op: fmt::Debug> fmt::Debug for EffectFence<S, Op> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectFence")
            .field("operation", &self.operation)
            .field("lease", &self.lease)
            .finish()
    }
}

impl<S: TenureScope, Op: PartialEq> PartialEq for EffectFence<S, Op> {
    fn eq(&self, other: &Self) -> bool {
        self.operation == other.operation && self.lease == other.lease
    }
}

impl<S: TenureScope, Op: Eq> Eq for EffectFence<S, Op> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fence::{Epoch, Tenure};
    use awaken_time::{Expiry, UtcTime};

    struct Probe;

    impl TenureScope for Probe {
        type Subject = ();
        type Holder = &'static str;
    }

    fn fence(
        operation: &'static str,
        holder: &'static str,
        epoch: u64,
        expires: u64,
    ) -> EffectFence<Probe, &'static str> {
        EffectFence::new(
            operation,
            Lease::new(
                Tenure::granted((), holder, Epoch::from_persisted(epoch).unwrap()),
                Expiry::at(UtcTime::from_epoch_ms(expires)),
            ),
        )
    }

    #[test]
    fn effect_identity_and_successor_decision_table() {
        // Predecessor: operation op, holder h, epoch 7, expiry 100.
        // | Rule | operation | lease successor        | same identity | effect successor |
        // | F1   | same      | renewal (longer)       | yes           | yes              |
        // | F2   | same      | reassignment           | no            | yes              |
        // | F3   | other     | renewal                | no            | no               |
        // | F4   | same      | shortened              | yes           | no               |
        // | F5   | same      | foreign holder same ep | no            | no               |
        let held = fence("op", "h", 7, 100);
        for (rule, successor, identity, authorized) in [
            ("F1", fence("op", "h", 7, 200), true, true),
            ("F2", fence("op", "g", 8, 200), false, true),
            ("F3", fence("other", "h", 7, 200), false, false),
            ("F4", fence("op", "h", 7, 50), true, false),
            ("F5", fence("op", "g", 7, 200), false, false),
        ] {
            assert_eq!(held.same_effect_identity(&successor), identity, "{rule}");
            assert_eq!(
                held.authorizes_effect_successor(&successor),
                authorized,
                "{rule}"
            );
        }
    }
}
