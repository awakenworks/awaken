//! A member set that can hold at most two epochs, for proofs of two-admission
//! traces. Two unordered slots, no allocation, no loop longer than two: the
//! point is that Kani can execute it twice where it cannot execute the real
//! B-tree twice. It is not a general set. A third distinct member fails loudly
//! rather than being dropped, so a proof that outgrows the model cannot pass
//! by losing a member.

use super::ActivityMembers;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::session_repo) struct TwoSlotMembers([Option<u64>; 2]);

impl ActivityMembers for TwoSlotMembers {
    fn insert(&mut self, epoch: u64) -> bool {
        if self.contains(&epoch) {
            return false;
        }
        let slot = self
            .0
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("a two-admission trace holds at most two members");
        *slot = Some(epoch);
        true
    }

    fn contains(&self, epoch: &u64) -> bool {
        self.0.contains(&Some(*epoch))
    }

    fn remove(&mut self, epoch: &u64) -> bool {
        match self.0.iter_mut().find(|slot| **slot == Some(*epoch)) {
            Some(slot) => {
                *slot = None;
                true
            }
            None => false,
        }
    }

    fn len(&self) -> usize {
        self.0.iter().flatten().count()
    }

    fn is_empty(&self) -> bool {
        self.0 == [None; 2]
    }

    fn clear(&mut self) {
        self.0 = [None; 2];
    }
}
