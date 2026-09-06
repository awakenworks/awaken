//! Pure mutation helpers shared by the durable WorkQueue adapters.

use std::collections::BTreeMap;

use awaken_session_contract::work_queue::{WorkQueueError, WorkQueueError::Storage};
use awaken_sql_storage::StoredU64;

pub(super) fn apply_metadata_patch(
    metadata: &mut BTreeMap<String, String>,
    patch: BTreeMap<String, Option<String>>,
) {
    for (key, value) in patch {
        match value {
            Some(value) => {
                metadata.insert(key, value);
            }
            None => {
                metadata.remove(&key);
            }
        }
    }
}

pub(super) fn lease_epoch(current: i64, advance: bool) -> Result<(i64, u64), WorkQueueError> {
    let current =
        StoredU64::try_from(current).map_err(|_| Storage("negative work lease epoch".into()))?;
    let next = if advance {
        current
            .checked_add(1)
            .map_err(|_| Storage("work lease epoch exhausted".into()))?
    } else {
        current
    };
    Ok((next.database_value(), next.domain_value()))
}

#[cfg(kani)]
#[kani::proof]
fn work_lease_epoch_is_exact_nonnegative_and_nonwrapping() {
    // Full i64 x bool domain, no artificial collection bound or SQL stubs.
    // X1 negative -> reject; X2 MAX+advance -> reject; X3 nonnegative+observe
    // -> unchanged; X4 other nonnegative+advance -> exact successor.
    // The i128 oracle is independent of the production StoredU64 arithmetic.
    // This proves the production numeric transition used by both claim paths,
    // not database locking, bearer entropy, or whole-queue linearizability.
    let current: i64 = kani::any();
    let advance: bool = kani::any();
    match lease_epoch(current, advance) {
        Ok((database, domain)) => {
            assert!(current >= 0 && !(advance && current == i64::MAX));
            assert!(i128::from(database) == i128::from(current) + i128::from(advance));
            assert!(i128::from(domain) == i128::from(database));
        }
        Err(WorkQueueError::Storage(_)) => {
            assert!(current < 0 || (advance && current == i64::MAX));
        }
    }
}
