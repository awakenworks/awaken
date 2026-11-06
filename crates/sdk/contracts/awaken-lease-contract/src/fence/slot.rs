//! Process-local allocation identity for a single-holder slot.

use std::fmt;
use std::sync::Arc;

/// The identity of one occupancy of a process-local slot.
///
/// Equal only to its own clones: identity is the allocation, never a number,
/// so no counter can wrap and let a stale holder match a later occupancy, and
/// no two independently minted tokens are ever equal. It is deliberately not
/// serializable and has no `Default`: it cannot leave the process or be
/// conjured without [`SlotToken::mint`].
///
/// Consumers: the run-ingress pool's single in-flight health probe and the
/// Sandbox control-transport publication slot.
#[derive(Clone)]
pub struct SlotToken(Arc<()>);

impl SlotToken {
    /// A token no other occupancy has ever held.
    #[must_use]
    pub fn mint() -> Self {
        Self(Arc::new(()))
    }
}

impl PartialEq for SlotToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SlotToken {}

impl fmt::Debug for SlotToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SlotToken")
            .field(&Arc::as_ptr(&self.0))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_equals_only_its_own_clones() {
        // | Rule | pair                          | equal |
        // | S1   | a token and its clone         | yes   |
        // | S2   | two independently minted      | no    |
        // | S3   | a mint after the first dropped | no   |
        let first = SlotToken::mint();
        assert_eq!(first, first.clone(), "S1");
        let second = SlotToken::mint();
        assert_ne!(first, second, "S2");
        let kept = first.clone();
        drop(first);
        let later = SlotToken::mint();
        assert_ne!(kept, later, "S3");
    }
}
