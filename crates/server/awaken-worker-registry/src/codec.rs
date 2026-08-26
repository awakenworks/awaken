use awaken_worker_contract::{RegisteredWorker, RegistryError, WorkerIdentity, WorkerSnapshot};

use crate::transition;

pub(crate) const WORKER_COLUMNS: &str = "worker_id, incarnation_id, generation, state, \
    manifest_json, capability_fingerprint, in_flight, warm_environment_shapes_json, \
    credential_observations_json, acp_capability_observations_json, expires_at_ms, \
    heartbeat_sequence, observation_sequence, registered_at_ms, heartbeat_at_ms, \
    drain_deadline_ms";

pub(crate) struct EncodedWorkerRow {
    pub worker_id: String,
    pub incarnation_id: String,
    pub generation: i64,
    pub state: String,
    pub manifest_json: String,
    pub capability_fingerprint: String,
    pub in_flight: i64,
    pub warm_environment_shapes_json: String,
    pub credential_observations_json: String,
    pub acp_capability_observations_json: String,
    pub expires_at_ms: i64,
    pub heartbeat_sequence: i64,
    pub observation_sequence: i64,
    pub registered_at_ms: i64,
    pub heartbeat_at_ms: i64,
    pub drain_deadline_ms: Option<i64>,
}

fn persistence(message: impl Into<String>) -> RegistryError {
    RegistryError::Persistence(message.into())
}

fn unsigned(field: &'static str, value: i64) -> Result<u64, RegistryError> {
    u64::try_from(value)
        .map_err(|_| persistence(format!("negative persisted worker {field}: {value}")))
}

fn decode_json<T: serde::de::DeserializeOwned>(
    field: &'static str,
    value: &str,
) -> Result<T, RegistryError> {
    serde_json::from_str(value)
        .map_err(|error| persistence(format!("invalid persisted worker {field}: {error}")))
}

pub(crate) fn encode_json<T: serde::Serialize>(
    field: &'static str,
    value: &T,
) -> Result<String, RegistryError> {
    serde_json::to_string(value)
        .map_err(|error| persistence(format!("cannot encode worker {field}: {error}")))
}

pub(crate) fn decode(row: EncodedWorkerRow) -> Result<RegisteredWorker, RegistryError> {
    let in_flight = u32::try_from(row.in_flight).map_err(|_| {
        persistence(format!(
            "persisted worker in_flight is outside u32 range: {}",
            row.in_flight
        ))
    })?;
    Ok(RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: WorkerIdentity::new(
                row.worker_id,
                row.incarnation_id,
                unsigned("generation", row.generation)?,
            ),
            state: transition::state_from_name(&row.state)?,
            manifest: decode_json("manifest_json", &row.manifest_json)?,
            capability_fingerprint: row.capability_fingerprint,
            in_flight,
            warm_environment_shapes: decode_json(
                "warm_environment_shapes_json",
                &row.warm_environment_shapes_json,
            )?,
            credential_observations: decode_json(
                "credential_observations_json",
                &row.credential_observations_json,
            )?,
            acp_capability_observations: decode_json(
                "acp_capability_observations_json",
                &row.acp_capability_observations_json,
            )?,
            expires_at_ms: unsigned("expires_at_ms", row.expires_at_ms)?,
        },
        heartbeat_sequence: unsigned("heartbeat_sequence", row.heartbeat_sequence)?,
        observation_sequence: unsigned("observation_sequence", row.observation_sequence)?,
        registered_at_ms: unsigned("registered_at_ms", row.registered_at_ms)?,
        heartbeat_at_ms: unsigned("heartbeat_at_ms", row.heartbeat_at_ms)?,
        drain_deadline_ms: row
            .drain_deadline_ms
            .map(|value| unsigned("drain_deadline_ms", value))
            .transpose()?,
    })
}
