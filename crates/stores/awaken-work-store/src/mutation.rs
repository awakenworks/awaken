//! Pure mutation helpers shared by the durable WorkQueue adapters.

use std::collections::BTreeMap;

use awaken_session_contract::work_queue::{WorkQueueError, WorkQueueError::Storage};

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
    let current = u64::try_from(current).map_err(|error| Storage(error.to_string()))?;
    let next = if advance {
        current
            .checked_add(1)
            .ok_or_else(|| Storage("work lease epoch exhausted".into()))?
    } else {
        current
    };
    Ok((
        i64::try_from(next).map_err(|error| Storage(error.to_string()))?,
        next,
    ))
}
