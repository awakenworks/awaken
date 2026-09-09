//! Durable SQLite and Postgres adapters for the Agent Config repository ports.

#![forbid(unsafe_code)]

mod codec;
mod postgres;
mod schema;
mod sqlite;

use awaken_agent_config::{ManagementEffect, ManagementEffectReceipt};
use awaken_sql_storage::{StoredU64, StoredU64Error};

pub use postgres::{PostgresConfigStore, StoreError as PostgresStoreError};
pub use schema::config_bundle;
pub use sqlite::{SqliteConfigStore, StoreError as SqliteStoreError};

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
}

#[cfg(any(test, kani))]
fn check_config_revision_write_plan(expected: u64) {
    // C1 zero/positive expected; C2 successor fits signed storage. R1 zero ->
    // insert 1; R2 positive and representable successor -> exact update; R3
    // max/out-of-range -> typed rejection. u128 is an independent range oracle.
    let successor = u128::from(expected) + 1;
    match ConfigRevisionWritePlan::new(expected) {
        Ok(plan) => {
            assert!(successor <= i64::MAX as u128);
            assert_eq!(plan.expected.domain_value(), expected);
            assert_eq!(plan.expected.database_value() as u128, u128::from(expected));
            assert_eq!(u128::from(plan.next.domain_value()), successor);
            assert_eq!(plan.next.database_value() as u128, successor);
            assert_eq!(plan.kind == ConfigRevisionWriteKind::Insert, expected == 0);
            assert_eq!(plan.kind == ConfigRevisionWriteKind::Update, expected > 0);
        }
        Err(error) => {
            assert!(successor > i64::MAX as u128);
            let rejected = if expected > i64::MAX as u64 {
                expected
            } else {
                successor as u64
            };
            assert_eq!(error, StoredU64Error::OutOfRange(rejected));
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn config_revision_write_plan_preserves_full_u64_contract() {
    // No assumptions: exhaust every u64 input through the production plan and
    // StoredU64 conversion/addition. SQL execution and concurrency remain TCB.
    check_config_revision_write_plan(kani::any());
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
    use super::{check_config_revision_write_plan, management_effect_retirement_decision};

    #[test]
    fn config_revision_write_plan_boundary_decision_table() {
        // R1 zero; R2 smallest/ordinary/largest valid update; R3 exhausted and
        // both ends of the out-of-range interval. Same oracle as full-u64 Kani.
        for expected in [
            0,
            1,
            9,
            i64::MAX as u64 - 1,
            i64::MAX as u64,
            i64::MAX as u64 + 1,
            u64::MAX,
        ] {
            check_config_revision_write_plan(expected);
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
