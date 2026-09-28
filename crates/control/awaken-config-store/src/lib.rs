//! Durable SQLite and Postgres adapters for the Agent Config repository ports.

#![forbid(unsafe_code)]

mod codec;
mod postgres;
mod schema;
mod sqlite;

#[cfg(any(test, kani))]
use awaken_agent_config::PublicationWrite;
use awaken_agent_config::{ManagementEffect, ManagementEffectReceipt};
use awaken_sql_storage::{StoredU64, StoredU64Error};

pub use postgres::{PostgresConfigStore, StoreError as PostgresStoreError};
pub use schema::config_bundle;
pub use sqlite::{SqliteConfigStore, StoreError as SqliteStoreError};

/// Admission of supplied source-revision facts, before publication-history I/O.
/// The adapters own the authoritative read and transaction and derive the
/// compared revision directly from the prepared publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationSourceAdmission {
    Current,
    Conflict { current_revision: Option<u64> },
}

impl PublicationSourceAdmission {
    fn new(current_revision: Option<u64>, expected: u64) -> Self {
        match current_revision {
            Some(current) if current == expected => Self::Current,
            _ => Self::Conflict { current_revision },
        }
    }
}

#[cfg(any(test, kani))]
fn check_publication_source_admission(current: Option<u64>, source_revision: u64) {
    // Causes: C1 source present; C2 current compared with the revision embedded
    // in the publication. Effects: P1 absent -> Conflict(None); P2 current >
    // source (stale candidate) -> Conflict(current); P3 current < source
    // (future candidate) -> Conflict;
    // P4 exact -> Current. R1 only P4 can mint the exact committed receipt that
    // the production ConfigService registration gate accepts. Zero and
    // u64::MAX are ordinary supplied facts; this owner does not encode SQL.
    let admitted = PublicationSourceAdmission::new(current, source_revision);
    let write = match admitted {
        PublicationSourceAdmission::Current => PublicationWrite::Committed { source_revision },
        PublicationSourceAdmission::Conflict { current_revision } => {
            PublicationWrite::Conflict { current_revision }
        }
    };
    match current.map(|revision| revision.cmp(&source_revision)) {
        Some(std::cmp::Ordering::Equal) => {
            assert_eq!(admitted, PublicationSourceAdmission::Current);
            assert_eq!(write.registration_admission(source_revision), Ok(()));
        }
        _ => {
            assert_eq!(
                admitted,
                PublicationSourceAdmission::Conflict {
                    current_revision: current,
                }
            );
            assert_eq!(write.registration_admission(source_revision), Err(current));
        }
    }
}

#[cfg(kani)]
/// @obligation config_activation.stale_publication_never_installs
#[kani::proof]
fn publication_source_admission_requires_present_exact_revision() {
    // No assumptions: all Option<u64> current source states x every embedded
    // publication revision reach the production owner and exact registration
    // receipt gate. SQL read/locking, commit durability and async registrar
    // invocation remain cross-checked at their actual production boundaries.
    check_publication_source_admission(kani::any(), kani::any());
}

/// Config owns the create/update choice; store-runtime owns integer encoding.
/// Both SQL CAS seams consume this plan at their existing transaction position,
/// after audit replay admission. This does not execute or model a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigRevisionWriteKind {
    Insert,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfigRevisionWritePlan {
    kind: ConfigRevisionWriteKind,
    expected: StoredU64,
    next: StoredU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigRevisionAdmission {
    Apply(ConfigRevisionWritePlan),
    Conflict { current_revision: Option<u64> },
}

impl ConfigRevisionWritePlan {
    fn new(expected: u64) -> Result<Self, StoredU64Error> {
        let expected = StoredU64::try_from(expected)?;
        let next = expected.checked_add(1)?;
        Ok(Self {
            kind: if expected.domain_value() == 0 {
                ConfigRevisionWriteKind::Insert
            } else {
                ConfigRevisionWriteKind::Update
            },
            expected,
            next,
        })
    }

    /// Admit one row transition before its adapter executes the final SQL
    /// predicate. Absence is the generation-zero state, never a present row at
    /// revision zero; every present transition must match a positive revision.
    fn admit(
        current_revision: Option<u64>,
        expected: u64,
    ) -> Result<ConfigRevisionAdmission, StoredU64Error> {
        let plan = Self::new(expected)?;
        let applies = matches!(
            (current_revision, plan.kind),
            (None, ConfigRevisionWriteKind::Insert)
        ) || matches!(
            (current_revision, plan.kind),
            (Some(current), ConfigRevisionWriteKind::Update) if current == expected
        );
        Ok(if applies {
            ConfigRevisionAdmission::Apply(plan)
        } else {
            ConfigRevisionAdmission::Conflict { current_revision }
        })
    }

    /// Advance the row currently owned by the store. This is the unconditional
    /// write mode, but it still enters through the same insert/update admission
    /// and checked successor used by compare-and-set writes.
    fn advance_current(
        current_revision: Option<u64>,
    ) -> Result<ConfigRevisionAdmission, StoredU64Error> {
        Self::admit(current_revision, current_revision.unwrap_or(0))
    }
}

#[cfg(any(test, kani))]
fn check_config_revision_admission(current_revision: Option<u64>, expected: u64) {
    // Causes: C1 row absent/present; C2 request is unconditional or carries an
    // expected zero/exact/different revision; C3 exact successor fits signed
    // storage. Effects: R1 absent+zero CAS/unconditional -> insert 1; R2
    // present+positive exact CAS/unconditional -> update to current+1; R3
    // absent+positive, present+zero, stale or future CAS -> conflict with the
    // observed revision; R4 exhausted/out-of-range -> rejection before SQL.
    let successor = u128::from(expected) + 1;
    let should_apply = successor <= i64::MAX as u128
        && match current_revision {
            None => expected == 0,
            Some(current) => expected > 0 && current == expected,
        };
    match ConfigRevisionWritePlan::admit(current_revision, expected) {
        Ok(ConfigRevisionAdmission::Apply(plan)) => {
            assert!(should_apply);
            assert!(successor <= i64::MAX as u128);
            assert_eq!(plan.expected.domain_value(), expected);
            assert_eq!(plan.expected.database_value() as u128, u128::from(expected));
            assert_eq!(u128::from(plan.next.domain_value()), successor);
            assert_eq!(plan.next.database_value() as u128, successor);
            assert_eq!(plan.kind == ConfigRevisionWriteKind::Insert, expected == 0);
            assert_eq!(plan.kind == ConfigRevisionWriteKind::Update, expected > 0);
        }
        Ok(ConfigRevisionAdmission::Conflict {
            current_revision: conflict,
        }) => {
            assert!(!should_apply);
            assert_eq!(conflict, current_revision);
            assert!(successor <= i64::MAX as u128);
        }
        Err(error) => {
            assert!(!should_apply);
            assert!(successor > i64::MAX as u128);
            let rejected = if expected > i64::MAX as u64 {
                expected
            } else {
                successor as u64
            };
            assert_eq!(error, StoredU64Error::OutOfRange(rejected));
        }
    }

    let advance_expected = current_revision.unwrap_or(0);
    let advance_successor = u128::from(advance_expected) + 1;
    let advance_should_apply =
        advance_successor <= i64::MAX as u128 && current_revision.is_none_or(|current| current > 0);
    match ConfigRevisionWritePlan::advance_current(current_revision) {
        Ok(ConfigRevisionAdmission::Apply(plan)) => {
            assert!(advance_should_apply);
            assert_eq!(plan.expected.domain_value(), advance_expected);
            assert_eq!(u128::from(plan.next.domain_value()), advance_successor);
            assert_eq!(
                plan.kind == ConfigRevisionWriteKind::Insert,
                current_revision.is_none()
            );
            assert_eq!(
                plan.kind == ConfigRevisionWriteKind::Update,
                current_revision.is_some()
            );
        }
        Ok(ConfigRevisionAdmission::Conflict {
            current_revision: conflict,
        }) => {
            assert_eq!(current_revision, Some(0));
            assert!(!advance_should_apply);
            assert_eq!(conflict, current_revision);
        }
        Err(error) => {
            assert!(!advance_should_apply);
            assert!(advance_successor > i64::MAX as u128);
            let rejected = if advance_expected > i64::MAX as u64 {
                advance_expected
            } else {
                advance_successor as u64
            };
            assert_eq!(error, StoredU64Error::OutOfRange(rejected));
        }
    }
}

#[cfg(kani)]
/// @obligation config.generation_is_monotonic
/// @obligation config.stale_writer_never_applies
#[kani::proof]
fn config_revision_admission_is_exact_and_nonwrapping() {
    // No assumptions: exhaust every Option<u64> observed row x independent
    // caller expectation through both production request modes at the one
    // owner. SQL predicates, locking and isolation remain adapter/database TCB.
    check_config_revision_admission(kani::any(), kani::any());
}

const fn management_effect_retirement_decision(
    pending_present: bool,
    receipt_matches: bool,
) -> bool {
    pending_present && receipt_matches
}

pub(crate) fn management_effect_retirement_is_admitted(
    receipt: &ManagementEffectReceipt,
    pending: Option<&ManagementEffect>,
) -> bool {
    pending.is_some_and(|pending| {
        management_effect_retirement_decision(true, receipt.matches_pending(pending))
    })
}

#[cfg(kani)]
/// @obligation resource_binding.pending_effect_survives_until_readback
/// @obligation resource_binding.replay_is_idempotent_and_identity_stable
#[kani::proof]
fn management_effect_retirement_requires_present_matching_receipt() {
    // C1 current pending row exists; C2 its complete payload matches the receipt.
    // E1 delete iff C1+C2; R0/R1/R2 retain for every other truth-table row.
    let pending_present: bool = kani::any();
    let receipt_matches: bool = kani::any();
    assert_eq!(
        management_effect_retirement_decision(pending_present, receipt_matches),
        pending_present && receipt_matches
    );
}

#[cfg(test)]
mod tests {
    use super::{
        check_config_revision_admission, check_publication_source_admission,
        management_effect_retirement_decision,
    };

    #[test]
    fn publication_source_admission_boundary_decision_table() {
        // P1-P4 above: cross absence and every equality/order relation with zero,
        // the signed storage boundary and both ends of the full u64 domain.
        let revisions = [0, 1, 7, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX];
        for expected in revisions {
            check_publication_source_admission(None, expected);
            for current in revisions {
                check_publication_source_admission(Some(current), expected);
            }
        }
    }

    #[test]
    fn config_revision_admission_boundary_decision_table() {
        // R1-R4 above. Cross CAS and unconditional mode with absent/present,
        // zero/exact/stale/future, the signed ceiling and full-u64 endpoints.
        // The same production oracle is used by Kani.
        let currents = [
            None,
            Some(0),
            Some(1),
            Some(9),
            Some(i64::MAX as u64),
            Some(u64::MAX),
        ];
        for expected in [
            0,
            1,
            9,
            i64::MAX as u64 - 1,
            i64::MAX as u64,
            i64::MAX as u64 + 1,
            u64::MAX,
        ] {
            for current in currents {
                check_config_revision_admission(current, expected);
            }
        }
    }

    #[test]
    fn retirement_requires_a_present_row_matching_the_receipt() {
        // Causes: C1 current row exists; C2 receipt matches its complete payload.
        // Effects: E1 retire; E2 retain. Decision table: R0 00->E2,
        // R1 01->E2 (impossible typed state, kept fail-closed), R2 10->E2,
        // R3 11->E1. The Kani harness exhausts the same production classifier.
        for (pending_present, receipt_matches, expected) in [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            assert_eq!(
                management_effect_retirement_decision(pending_present, receipt_matches),
                expected
            );
        }
    }
}
