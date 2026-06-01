use std::collections::{BTreeMap, HashMap};

use awaken_runtime_contract::now_ms;
use awaken_runtime_contract::state::PersistedState;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::{BackendAbortRequest, ExecutionBackendError};

use super::{DirectMessageSnapshot, TaskSnapshot, task_state_name};

pub(super) const REMOTE_STATE_KEY: &str = "__runtime_remote_backend";
pub(super) const REMOTE_STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PersistedRemoteBackendState {
    #[serde(default = "remote_state_schema_version")]
    version: u32,
    #[serde(default)]
    targets: BTreeMap<String, PersistedA2aThreadState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PersistedA2aThreadState {
    #[serde(default = "remote_state_schema_version")]
    pub(super) version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) updated_at_ms: Option<u64>,
}

fn remote_state_schema_version() -> u32 {
    REMOTE_STATE_SCHEMA_VERSION
}

pub(super) fn update_persisted_state(
    state: Option<PersistedState>,
    target_key: &str,
    snapshot: &TaskSnapshot,
) -> Result<Option<PersistedState>, ExecutionBackendError> {
    let mut persisted = state.unwrap_or(PersistedState {
        revision: 0,
        extensions: HashMap::new(),
    });
    let mut remote_state =
        decode_persisted_remote_state(persisted.extensions.remove(REMOTE_STATE_KEY))?;
    remote_state.version = REMOTE_STATE_SCHEMA_VERSION;
    remote_state.targets.insert(
        target_key.to_string(),
        PersistedA2aThreadState {
            version: REMOTE_STATE_SCHEMA_VERSION,
            task_id: Some(snapshot.task_id.clone()),
            context_id: snapshot.context_id.clone(),
            last_state: Some(task_state_name(snapshot.state).to_string()),
            updated_at_ms: Some(now_ms()),
        },
    );
    persisted.extensions.insert(
        REMOTE_STATE_KEY.to_string(),
        encode_persisted_remote_state(remote_state)?,
    );
    Ok(Some(persisted))
}

pub(super) fn update_persisted_state_from_direct(
    state: Option<PersistedState>,
    target_key: &str,
    snapshot: &DirectMessageSnapshot,
) -> Result<Option<PersistedState>, ExecutionBackendError> {
    if snapshot.task_id.is_none() && snapshot.context_id.is_none() {
        return Ok(state);
    }

    let mut persisted = state.unwrap_or(PersistedState {
        revision: 0,
        extensions: HashMap::new(),
    });
    let mut remote_state =
        decode_persisted_remote_state(persisted.extensions.remove(REMOTE_STATE_KEY))?;
    let prior = remote_state
        .targets
        .get(target_key)
        .cloned()
        .unwrap_or_default();

    remote_state.version = REMOTE_STATE_SCHEMA_VERSION;
    remote_state.targets.insert(
        target_key.to_string(),
        PersistedA2aThreadState {
            version: REMOTE_STATE_SCHEMA_VERSION,
            task_id: snapshot.task_id.clone().or(prior.task_id),
            context_id: snapshot.context_id.clone().or(prior.context_id),
            last_state: Some("DIRECT_MESSAGE".to_string()),
            updated_at_ms: Some(now_ms()),
        },
    );

    persisted.extensions.insert(
        REMOTE_STATE_KEY.to_string(),
        encode_persisted_remote_state(remote_state)?,
    );
    Ok(Some(persisted))
}

pub(super) fn read_remote_state_entry(
    state: &PersistedState,
    target_key: &str,
) -> Result<Option<PersistedA2aThreadState>, ExecutionBackendError> {
    Ok(
        decode_persisted_remote_state(state.extensions.get(REMOTE_STATE_KEY).cloned())?
            .targets
            .get(target_key)
            .cloned(),
    )
}

pub(super) fn persisted_abort_task_id(
    request: &BackendAbortRequest<'_>,
    target_key: &str,
) -> Result<Option<String>, ExecutionBackendError> {
    Ok(request
        .persisted_state
        .map(|state| read_remote_state_entry(state, target_key))
        .transpose()?
        .flatten()
        .and_then(|state| reusable_prior_task_id(&state)))
}

pub(super) fn reusable_prior_task_id(state: &PersistedA2aThreadState) -> Option<String> {
    if state
        .last_state
        .as_deref()
        .is_some_and(is_interrupted_remote_state)
    {
        state.task_id.clone()
    } else {
        None
    }
}

fn decode_persisted_remote_state(
    value: Option<Value>,
) -> Result<PersistedRemoteBackendState, ExecutionBackendError> {
    match value {
        Some(value) => {
            serde_json::from_value::<PersistedRemoteBackendState>(value).map_err(|error| {
                ExecutionBackendError::ExecutionFailed(format!(
                    "corrupt A2A persisted remote state at {REMOTE_STATE_KEY}: {error}"
                ))
            })
        }
        None => Ok(PersistedRemoteBackendState::default()),
    }
}

fn encode_persisted_remote_state(
    state: PersistedRemoteBackendState,
) -> Result<Value, ExecutionBackendError> {
    serde_json::to_value(state).map_err(|error| {
        ExecutionBackendError::ExecutionFailed(format!(
            "failed to encode A2A persisted remote state: {error}"
        ))
    })
}

fn is_interrupted_remote_state(state: &str) -> bool {
    matches!(
        state,
        "TASK_STATE_INPUT_REQUIRED" | "TASK_STATE_AUTH_REQUIRED"
    )
}
