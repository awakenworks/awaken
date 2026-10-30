// @formal entrypoint session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
// Declares the Session-owned lease decision boundary. The Session application
// retains topology, terminal-state and claimed-Run revival admission.
//! Session-owned permission to acquire or release a Worker assignment lease.

#[cfg(kani)]
use crate::worker_lease_types::proof_representative_lease;
use crate::{SessionWork, SessionWorkAcquisition, SessionWorkGrant, SessionWorkOwnership};
use awaken_lease_contract::fence::{Epoch, Tenure, ZeroEpoch};
use awaken_session_types::{RunError, SessionId};

/// Session admission over the Environment queue's one existing lease authority.
/// Queue adapters cannot implement this decision in place of the Session owner.
#[async_trait::async_trait]
pub trait SessionWorkerLeaseAuthority: Send + Sync {
    /// Hint that placement evidence changed; this is not mutation authority.
    fn request_lifecycle_reconciliation(&self) {}

    async fn acquire_session_work(
        &self,
        session_id: &SessionId,
        worker_owner: &str,
        now_ms: u64,
        acquisition: SessionWorkAcquisition,
    ) -> Result<SessionWorkOwnership, RunError>;

    /// Release only the exact Session, Environment, Work owner and lease epoch.
    async fn release_session_work(&self, lease: &SessionWorkGrant) -> Result<bool, RunError>;

    /// Return this exact Worker incarnation's non-terminal assignments to the queue.
    async fn release_worker_session_work(&self, worker_owner: &str) -> Result<usize, RunError>;
}

/// Exact, secret-free proof of the Work authority observed when a registered
/// Worker entered one Session attempt.
///
/// The authority is private so production callers cannot assemble it from
/// unrelated strings or silently replace a stale epoch with a later one. A
/// leased receipt is exactly the lease's [`Tenure`] (Work, Environment,
/// Session, owner, epoch). Wire input is still untrusted: the Coordinator
/// always compares this receipt with a fresh observation from the canonical
/// WorkQueue before exposing committed Thread truth or admitting a Session
/// effect.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    try_from = "SessionWorkAuthorityReceiptWire",
    into = "SessionWorkAuthorityReceiptWire"
)]
pub struct SessionWorkAuthorityReceipt(ReceiptAuthority);

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReceiptAuthority {
    NotRequired {
        session_id: SessionId,
        worker_owner: String,
    },
    Leased(Tenure<SessionWork>),
}

/// The persisted spelling of [`SessionWorkAuthorityReceipt`], unchanged from
/// the shape the receipt had before it embedded a tenure.
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionWorkAuthorityReceiptWire {
    session_id: SessionId,
    worker_owner: String,
    authority: SessionWorkAuthorityCoordinate,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SessionWorkAuthorityCoordinate {
    NotRequired,
    Leased {
        work_id: String,
        environment_id: String,
        epoch: u64,
    },
}

impl TryFrom<SessionWorkAuthorityReceiptWire> for SessionWorkAuthorityReceipt {
    type Error = ZeroEpoch;

    fn try_from(wire: SessionWorkAuthorityReceiptWire) -> Result<Self, Self::Error> {
        Ok(Self(match wire.authority {
            SessionWorkAuthorityCoordinate::NotRequired => ReceiptAuthority::NotRequired {
                session_id: wire.session_id,
                worker_owner: wire.worker_owner,
            },
            SessionWorkAuthorityCoordinate::Leased {
                work_id,
                environment_id,
                epoch,
            } => ReceiptAuthority::Leased(Tenure::granted(
                (work_id, environment_id, wire.session_id),
                wire.worker_owner,
                Epoch::try_from(epoch)?,
            )),
        }))
    }
}

impl From<SessionWorkAuthorityReceipt> for SessionWorkAuthorityReceiptWire {
    fn from(receipt: SessionWorkAuthorityReceipt) -> Self {
        match receipt.0 {
            ReceiptAuthority::NotRequired {
                session_id,
                worker_owner,
            } => Self {
                session_id,
                worker_owner,
                authority: SessionWorkAuthorityCoordinate::NotRequired,
            },
            ReceiptAuthority::Leased(tenure) => {
                let epoch = tenure.epoch().get();
                let worker_owner = tenure.holder().clone();
                let (work_id, environment_id, session_id) = tenure.subject().clone();
                Self {
                    session_id,
                    worker_owner,
                    authority: SessionWorkAuthorityCoordinate::Leased {
                        work_id,
                        environment_id,
                        epoch,
                    },
                }
            }
        }
    }
}

const fn exact_session_work_coordinate(
    session_matches: bool,
    owner_matches: bool,
    scope_matches: bool,
    environment_matches: bool,
) -> bool {
    session_matches && owner_matches && scope_matches && environment_matches
}

impl SessionWorkAuthorityReceipt {
    /// Issue a receipt only from the complete result returned by the canonical
    /// acquisition port. `Unowned` and a foreign owner cannot mint authority.
    #[must_use]
    fn from_acquired(
        session_id: &str,
        worker_owner: &str,
        ownership: &SessionWorkOwnership,
    ) -> Option<Self> {
        Some(Self(match ownership {
            SessionWorkOwnership::NotRequired => ReceiptAuthority::NotRequired {
                session_id: SessionId::from(session_id),
                worker_owner: worker_owner.to_string(),
            },
            SessionWorkOwnership::Unowned => return None,
            SessionWorkOwnership::Leased(lease)
                if lease.session_id() == session_id && lease.owner() == worker_owner =>
            {
                ReceiptAuthority::Leased(lease_tenure(lease))
            }
            SessionWorkOwnership::Leased(_) => return None,
        }))
    }

    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        match &self.0 {
            ReceiptAuthority::NotRequired { session_id, .. } => session_id,
            ReceiptAuthority::Leased(tenure) => &tenure.subject().2,
        }
    }

    #[must_use]
    pub fn worker_owner(&self) -> &str {
        match &self.0 {
            ReceiptAuthority::NotRequired { worker_owner, .. } => worker_owner,
            ReceiptAuthority::Leased(tenure) => tenure.holder(),
        }
    }

    /// Compare every stable coordinate with a new canonical observation. Lease
    /// expiry is deliberately omitted: renewal may extend time, while Work id,
    /// Environment, Session, owner, and epoch — the complete tenure — must
    /// remain identical.
    #[must_use]
    pub fn matches_observation(&self, ownership: &SessionWorkOwnership) -> bool {
        match (&self.0, ownership) {
            (ReceiptAuthority::NotRequired { .. }, SessionWorkOwnership::NotRequired) => true,
            (ReceiptAuthority::Leased(tenure), SessionWorkOwnership::Leased(lease)) => {
                tenure == &lease_tenure(lease)
            }
            _ => false,
        }
    }

    /// Compare the receipt with the immutable realization facts projected by
    /// the Session facade, without depending back on its directive type.
    #[must_use]
    pub fn binds_realization(
        &self,
        session_id: &str,
        runtime_incarnation: &str,
        environment_self_hosted: bool,
        environment_id: &str,
    ) -> bool {
        let (scope_matches, environment_matches) = match &self.0 {
            ReceiptAuthority::NotRequired { .. } => (!environment_self_hosted, true),
            ReceiptAuthority::Leased(tenure) => (
                environment_self_hosted,
                tenure.subject().1 == environment_id,
            ),
        };
        exact_session_work_coordinate(
            self.session_id() == session_id,
            self.worker_owner() == runtime_incarnation,
            scope_matches,
            environment_matches,
        )
    }
}

/// Acquire through the one Work authority and seal its complete stable
/// coordinate into a receipt. Callers cannot construct or alter the typed
/// receipt through Rust fields; a decoded wire value remains only an assertion
/// until [`SessionWorkAuthorityReceipt::matches_observation`] succeeds.
pub async fn acquire_session_work_receipt(
    authority: &dyn SessionWorkerLeaseAuthority,
    session_id: &SessionId,
    worker_owner: &str,
    now_ms: u64,
    acquisition: SessionWorkAcquisition,
) -> Result<Option<SessionWorkAuthorityReceipt>, RunError> {
    let ownership = authority
        .acquire_session_work(session_id, worker_owner, now_ms, acquisition)
        .await?;
    Ok(SessionWorkAuthorityReceipt::from_acquired(
        session_id.as_str(),
        worker_owner,
        &ownership,
    ))
}

// Reuse the canonical tenure already owned by the observed grant.
// This performs no second coordinate reconstruction or lease mutation.
fn lease_tenure(lease: &SessionWorkGrant) -> Tenure<SessionWork> {
    lease.tenure().clone()
}

#[cfg(kani)]
/// @obligation session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
#[kani::proof]
fn session_work_receipt_kind_and_minting_are_exact() {
    // Causes: K1 the observed authority kind differs; K2 the acquisition is
    // unowned; K3 Session or owner differs at minting. Effects: K1 never
    // matches a leased receipt; K2-K3 mint nothing. A Work-free receipt matches
    // only Work-free. Four symbolic minting rows and concrete cross-kind rows
    // keep this branch separate from the executable coordinate matrix.
    let session_matches: bool = kani::any();
    let owner_matches: bool = kani::any();
    let acquired = SessionWorkOwnership::Leased(proof_representative_lease(
        true, true, true, true, true, true,
    ));
    let receipt = SessionWorkAuthorityReceipt::from_acquired("s", "w", &acquired)
        .expect("an exactly owned acquisition mints a receipt");

    // Cross-kind rows: a leased receipt never matches a Work-free or unowned
    // observation, and a NotRequired receipt matches only its own kind.
    assert!(!receipt.matches_observation(&SessionWorkOwnership::NotRequired));
    assert!(!receipt.matches_observation(&SessionWorkOwnership::Unowned));
    let free =
        SessionWorkAuthorityReceipt::from_acquired("s", "w", &SessionWorkOwnership::NotRequired)
            .expect("a Work-free acquisition mints a receipt");
    assert!(free.matches_observation(&SessionWorkOwnership::NotRequired));
    assert!(!free.matches_observation(&SessionWorkOwnership::Unowned));
    assert!(!free.matches_observation(&acquired));

    // The minting door is part of the same derivation: unowned Work mints no
    // authority, and a foreign Session or owner cannot mint one either.
    assert!(
        SessionWorkAuthorityReceipt::from_acquired("s", "w", &SessionWorkOwnership::Unowned)
            .is_none(),
        "unowned Work mints no authority"
    );
    assert_eq!(
        SessionWorkAuthorityReceipt::from_acquired(
            if session_matches { "s" } else { "S" },
            if owner_matches { "w" } else { "W" },
            &acquired,
        )
        .is_some(),
        session_matches && owner_matches,
        "a leased acquisition mints authority only for its exact Session and owner"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A leased receipt is exactly the lease's tenure, on an unchanged wire.
    ///
    /// Held: Work `k`, Environment `e`, Session `s`, owner `w`, epoch 3.
    /// | Rule | observation / wire                      | effect            |
    /// | T1   | same tenure, renewed expiry             | matches           |
    /// | T2   | other Work item of the same Session     | no match          |
    /// | T3   | other Environment                       | no match          |
    /// | T4   | other owner                             | no match          |
    /// | T5   | successor epoch                         | no match          |
    /// | T6   | foreign Session                         | no match          |
    /// | T7   | any subset of five stable coordinates   | match iff none    |
    /// |      | changed, with either expiry            |                  |
    /// | W1   | receipt encodes                         | the flat wire     |
    /// | W2   | that wire decodes                       | the same receipt  |
    /// | W3   | a leased wire with epoch 0              | refused on decode |
    #[test]
    fn a_leased_receipt_is_the_exact_tenure_on_an_unchanged_wire() {
        let lease =
            |work: &str, environment: &str, session: &str, owner: &str, epoch: u64, expiry: u64| {
                SessionWorkOwnership::Leased(
                    awaken_session_types::SessionWorkLease::from_persisted(
                        work,
                        environment,
                        session.into(),
                        owner,
                        epoch,
                        expiry,
                    )
                    .map(SessionWorkGrant::from_transfer)
                    .unwrap(),
                )
            };
        let receipt = super::SessionWorkAuthorityReceipt::from_acquired(
            "s",
            "w",
            &lease("k", "e", "s", "w", 3, 10),
        )
        .expect("an owned acquisition mints a receipt");
        for (rule, observation, matches) in [
            ("T1", lease("k", "e", "s", "w", 3, 99), true),
            ("T2", lease("k2", "e", "s", "w", 3, 10), false),
            ("T3", lease("k", "e2", "s", "w", 3, 10), false),
            ("T4", lease("k", "e", "s", "w2", 3, 10), false),
            ("T5", lease("k", "e", "s", "w", 4, 10), false),
            ("T6", lease("k", "e", "foreign", "w", 3, 10), false),
        ] {
            assert_eq!(receipt.matches_observation(&observation), matches, "{rule}");
        }
        // T7 cause/effect matrix: bits 0-4 independently alter Session,
        // owner, Work, Environment and epoch; bit 5 alters only expiry. Every
        // feasible combination yields a match exactly when bits 0-4 are clear.
        // This joint table complements the bounded Kani minting/kind proof.
        for changed in 0_u8..64 {
            let observation = lease(
                if changed & 4 == 0 { "k" } else { "K" },
                if changed & 8 == 0 { "e" } else { "E" },
                if changed & 1 == 0 { "s" } else { "S" },
                if changed & 2 == 0 { "w" } else { "W" },
                if changed & 16 == 0 { 3 } else { 4 },
                if changed & 32 == 0 { 10 } else { 99 },
            );
            assert_eq!(
                receipt.matches_observation(&observation),
                changed & 31 == 0,
                "T7 changed={changed}"
            );
        }
        let wire = serde_json::to_value(&receipt).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "session_id": "s",
                "worker_owner": "w",
                "authority": {"kind": "leased", "work_id": "k", "environment_id": "e", "epoch": 3},
            }),
            "W1"
        );
        assert_eq!(
            serde_json::from_value::<super::SessionWorkAuthorityReceipt>(wire.clone()).unwrap(),
            receipt,
            "W2"
        );
        let mut zero = wire;
        zero["authority"]["epoch"] = 0.into();
        assert!(
            serde_json::from_value::<super::SessionWorkAuthorityReceipt>(zero).is_err(),
            "W3"
        );
    }
}
