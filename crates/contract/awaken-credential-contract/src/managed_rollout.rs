// @formal entrypoint credential.managed_rollout_ack_requires_converged_progress
// @formal entrypoint credential.managed_rollout_event_id_replay_requires_exact_payload_and_colliding_ack_is_rejected
//! Secret-free credential revision adoption protocol.
//!
//! The Credential aggregate publishes this wire fact after committing an exact
//! source/managed-child pair. Consumers may be local or remote, but they only
//! acknowledge convergence after every target in their own bounded context has
//! adopted or retired the named revision.

use serde::{Deserialize, Serialize};

use crate::CredentialSourceId;

/// The closed command vocabulary carried by durable credential mutations and
/// their adoption events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedCredentialOperation {
    Create,
    Update,
    Archive,
    Delete,
}

/// One exact, secret-free credential fence that online consumers must adopt.
///
/// Target discovery, topology and rollout policy deliberately do not appear in
/// this event. A consumer freezes those facts within its own bounded context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedCredentialRollout {
    pub id: String,
    pub workspace_id: String,
    pub vault_id: String,
    pub credential_id: String,
    pub source_id: CredentialSourceId,
    pub source_version: u64,
    pub credential_revision: u64,
    pub operation: ManagedCredentialOperation,
}

/// Exact durable-event identity decision shared by every Credential store.
///
/// `Absent` admits first publication or an idempotent acknowledgement. `Exact`
/// admits replay/acknowledgement of the complete payload. `Collision` rejects
/// reuse of the event id with any different authority coordinate. The store
/// adapter still owns its transaction and affected-row checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedCredentialRolloutIdentityDecision {
    Absent,
    Exact,
    Collision,
}

#[must_use]
pub fn managed_credential_rollout_identity_decision(
    durable: Option<&ManagedCredentialRollout>,
    proposed: &ManagedCredentialRollout,
) -> ManagedCredentialRolloutIdentityDecision {
    match durable {
        None => ManagedCredentialRolloutIdentityDecision::Absent,
        Some(durable) if durable == proposed => ManagedCredentialRolloutIdentityDecision::Exact,
        Some(_) => ManagedCredentialRolloutIdentityDecision::Collision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_wire_is_secret_free_and_round_trips() {
        let event = ManagedCredentialRollout {
            id: "rollout-1".into(),
            workspace_id: "ws".into(),
            vault_id: "vault".into(),
            credential_id: "credential".into(),
            source_id: CredentialSourceId("source".into()),
            source_version: 2,
            credential_revision: 2,
            operation: ManagedCredentialOperation::Update,
        };
        let wire = serde_json::to_string(&event).unwrap();
        assert!(!wire.contains("secret"));
        assert_eq!(
            serde_json::from_str::<ManagedCredentialRollout>(&wire).unwrap(),
            event
        );
    }
}

#[cfg(any(test, kani))]
mod identity_proofs {
    use super::*;

    #[derive(Clone, Copy)]
    struct Input {
        bytes: [[u8; 2]; 5],
        lengths: [u8; 5],
        revisions: [u64; 2],
        operation: u8,
    }

    impl Input {
        fn event(self) -> ManagedCredentialRollout {
            let text = |index: usize| {
                std::str::from_utf8(&self.bytes[index][..usize::from(self.lengths[index])])
                    .expect("bounded ASCII input")
                    .to_owned()
            };
            ManagedCredentialRollout {
                id: text(0),
                workspace_id: text(1),
                vault_id: text(2),
                credential_id: text(3),
                source_id: CredentialSourceId(text(4)),
                source_version: self.revisions[0],
                credential_revision: self.revisions[1],
                operation: match self.operation {
                    0 => ManagedCredentialOperation::Create,
                    1 => ManagedCredentialOperation::Update,
                    2 => ManagedCredentialOperation::Archive,
                    3 => ManagedCredentialOperation::Delete,
                    _ => unreachable!("closed operation domain"),
                },
            }
        }
    }

    fn check_case(left: Input, right: Input) {
        // Cause/effect graph: C1..C5 are the five independent string identities;
        // C6/C7 are full-width revisions; C8 is the closed operation. A string
        // differs iff its length differs or an occupied byte differs. Bytes
        // beyond its length are unobservable. R1 all eight equal => exact
        // replay/equality; R2 any differing field => collision/inequality.
        // The oracle compares pre-construction bytes/lengths and numeric tags,
        // independently of the production derived PartialEq and String Eq.
        let mut exact = left.revisions[0] == right.revisions[0]
            && left.revisions[1] == right.revisions[1]
            && left.operation == right.operation;
        for field in 0..5 {
            exact &= left.lengths[field] == right.lengths[field];
            for offset in 0..2 {
                if offset < usize::from(left.lengths[field]) {
                    exact &= left.bytes[field][offset] == right.bytes[field][offset];
                }
            }
        }
        let left = left.event();
        let right = right.event();
        assert_eq!(left == right, exact);
        assert_eq!(left != right, !exact);
        assert_eq!(
            managed_credential_rollout_identity_decision(Some(&left), &right),
            if exact {
                ManagedCredentialRolloutIdentityDecision::Exact
            } else {
                ManagedCredentialRolloutIdentityDecision::Collision
            }
        );
        assert_eq!(
            managed_credential_rollout_identity_decision(None, &right),
            ManagedCredentialRolloutIdentityDecision::Absent
        );
    }

    #[test]
    fn rollout_identity_field_decision_table() {
        // Exhaust all 256 combinations of the eight independent field causes,
        // then each string's length/prefix/empty/unoccupied-byte cases. This
        // is a native decision table, not a claim of unbounded string proof.
        let base = Input {
            bytes: [[b'a', b'b']; 5],
            lengths: [2; 5],
            revisions: [0, u64::MAX],
            operation: 1,
        };
        for mask in 0_u16..256 {
            let mut other = base;
            for field in 0..5 {
                if mask & (1 << field) != 0 {
                    other.bytes[field][1] = b'c';
                }
            }
            if mask & 32 != 0 {
                other.revisions[0] = u64::MAX;
            }
            if mask & 64 != 0 {
                other.revisions[1] = 0;
            }
            if mask & 128 != 0 {
                other.operation = 3;
            }
            check_case(base, other);
        }
        for field in 0..5 {
            for left_length in 0..=2 {
                for right_length in 0..=2 {
                    for changed_offset in 0..2 {
                        let mut left = base;
                        let mut right = base;
                        left.lengths[field] = left_length;
                        right.lengths[field] = right_length;
                        right.bytes[field][changed_offset] = 0;
                        check_case(left, right);
                    }
                }
            }
        }
    }

    #[cfg(kani)]
    /// @obligation credential.managed_rollout_event_id_replay_requires_exact_payload_and_colliding_ack_is_rejected
    /// @kani profile=solver-minisat-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(6)]
    fn managed_rollout_identity_requires_every_payload_field() {
        fn input() -> Input {
            let input = Input {
                bytes: kani::any(),
                lengths: kani::any(),
                revisions: kani::any(),
                operation: kani::any(),
            };
            for field in 0..5 {
                kani::assume(input.lengths[field] <= 2);
                for offset in 0..2 {
                    kani::assume(input.bytes[field][offset] < 128);
                }
            }
            kani::assume(input.operation < 4);
            input
        }
        // Actual production equality/inequality, no stubs. Bounds: five
        // independent 0..2-byte ASCII strings per event (including NUL), all
        // u64 revisions and all operations. Allocation, comparison and Drop
        // execute normally. SQL locks/JSON decoding and arbitrary lengths or
        // non-ASCII strings are outside this bounded kernel proof.
        check_case(input(), input());
    }
}
