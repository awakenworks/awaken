// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: Original ledger CAS and fresh capture are fail closed;
// @formal reason: the cause-effect tests cover byte and replay scope, not universal physical closure.
//! Full ConfigMap envelope check at its sole replacement/CAS boundary.
use k8s_openapi::api::core::v1::ConfigMap;

use super::state::{WorkerCreateLedgerState, future_capacity};
use crate::RuntimeError;
use crate::k8s::error::backend;

pub(super) const CONFIGMAP_DATA_BYTES: usize = 1024 * 1024;
const ANNOTATION_BYTES: usize = 256 * 1024;
// Pinned v1.30 default incoming write limit, not storage/proxy qualification.
const WRITE_BYTES: usize = 3 * 1024 * 1024;

use super::capture::checked_size_add as add;

/// Actual physical ConfigMap law also bounds historical whole-join input.
/// This never imposes the fresh result envelope on a historical UID.
pub(super) fn validate_observed(object: &ConfigMap) -> Result<usize, RuntimeError> {
    let mut bytes = 0;
    for value in object.data.iter().flat_map(|data| data.values()) {
        bytes = add(bytes, value.len())?;
    }
    for value in object.binary_data.iter().flat_map(|data| data.values()) {
        bytes = add(bytes, value.0.len())?;
    }
    if bytes > CONFIGMAP_DATA_BYTES {
        return Err(backend(
            "Create ledger ConfigMap value capacity is exhausted",
        ));
    }
    let mut annotations = 0;
    for (key, value) in object
        .metadata
        .annotations
        .iter()
        .flat_map(|data| data.iter())
    {
        annotations = add(annotations, add(key.len(), value.len())?)?;
    }
    if annotations > ANNOTATION_BYTES {
        return Err(backend("Create ledger annotation capacity is exhausted"));
    }
    Ok(bytes)
}

pub(super) fn validate_candidate(
    object: &ConfigMap,
    state: &WorkerCreateLedgerState,
) -> Result<(), RuntimeError> {
    let observed_data = validate_observed(object)?;
    let future = future_capacity(state)?;
    let data = add(observed_data, future.data_growth)?;
    if data > CONFIGMAP_DATA_BYTES {
        return Err(backend(
            "Create ledger cannot retain all outstanding capture obligations",
        ));
    }
    let wire = add(
        serde_json::to_vec(object).map_err(backend)?.len(),
        future.outer_growth,
    )?;
    if wire > WRITE_BYTES {
        return Err(backend(
            "Create ledger complete future write envelope is exhausted",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
