// @formal surface exclude=read_model_contract
// @formal reason: Existing immutable execution selection and committed Run read vocabulary, moved to its read boundary without adding authority.
//! Session execution observation values.
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
/// Read-only application projection joining one Session's immutable execution
/// selection to committed Run/model-request facts. It owns no status or audit
/// state: every field is rebuilt from the Session aggregate and the Runtime's
/// one internally consistent Thread recovery prefix.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionExecutionObservation {
    pub configuration:
        Option<awaken_session_configuration_contract::SessionConfigurationInspection>,
    pub artifact_capture: Option<awaken_resource_contract::ArtifactCapture>,
    pub recovery: crate::SessionExecutionRecoveryObservation,
    pub session_id: String,
    pub session_revision: u64,
    pub agent_id: String,
    pub agent_revision: Option<u64>,
    pub application_origin: Option<awaken_session_configuration_contract::ApplicationOriginBinding>,
    pub model: String,
    pub execution_model_ref: String,
    pub backend_ref: Option<String>,
    pub execution: awaken_session_state_contract::SessionExecutionState,
    pub runs: Vec<SessionRunExecutionObservation>,
}

/// One committed Run and only the model-request completion coordinates that
/// belong to it. The Run record and audit log remain the authorities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRunExecutionObservation {
    pub model_operations: Vec<awaken_runtime_contract::model_inspection::ModelOperationInspection>,
    pub run_id: RunId,
    pub idempotency_key: Option<String>,
    pub state: RunState,
    pub tool_uses: Vec<SessionToolUseObservation>,
    pub model_requests: Vec<SessionModelRequestObservation>,
}

/// Neutral coordinates for one committed tool use. Protocol adapters retain
/// ownership of their public event identity codecs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionToolUseObservation {
    pub source_message_id: String,
    pub call_id: String,
}

/// Minimal proof that one logical model request reached a committed completion
/// boundary. Usage remains on the existing Managed Events projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionModelRequestObservation {
    pub sequence: u64,
    pub is_error: bool,
    pub retry_count: u32,
}

impl SessionModelRequestObservation {
    /// Inspect only canonical completion records attributed to this exact Run.
    pub fn for_run(
        records: &[awaken_agent_contract::audit::record::Record],
        run: &RunId,
    ) -> Result<Vec<Self>, serde_json::Error> {
        records
            .iter()
            .filter(|event| &event.run_id == run)
            .filter_map(|event| {
                awaken_agent_contract::audit::model_request::ModelRequestObservation::from_record(
                    event,
                )
                .transpose()
                .map(|result| {
                    result.map(|value| Self {
                        sequence: event.sequence,
                        is_error: value.is_error,
                        retry_count: value.retry_count,
                    })
                })
            })
            .collect()
    }
}
