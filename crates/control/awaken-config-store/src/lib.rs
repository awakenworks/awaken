//! Durable SQLite and Postgres adapters for the Agent Config repository ports.

#![forbid(unsafe_code)]

mod codec;
mod postgres;
mod schema;
mod sqlite;

use awaken_agent_config::{ManagementEffect, ManagementEffectReceipt};

pub use postgres::{PostgresConfigStore, StoreError as PostgresStoreError};
pub use schema::config_bundle;
pub use sqlite::{SqliteConfigStore, StoreError as SqliteStoreError};

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
    use super::management_effect_retirement_decision;

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
