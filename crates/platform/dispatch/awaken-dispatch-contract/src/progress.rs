// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Progress publication is separate from preview delivery and requires an exact claim.
use awaken_agent_contract::stream::event::{AssistantResponseCoordinate, Observation};
use awaken_worker_contract::{RunClaim, WorkerIdentity};
use serde::{Deserialize, Serialize};

pub const PROGRESS_BATCH_PATH: &str = "/v1/worker/progress/append";
pub const PROGRESS_BATCH_ITEMS: usize = 64;
pub const PROGRESS_BATCH_BYTES: usize = 256 * 1024;

/// The claim and response identify the stream. The batch position is the
/// idempotency key inside that stream and is assigned by the producer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressBatch {
    pub commit_ordinal: u64,
    pub response: AssistantResponseCoordinate,
    pub batch_index: u64,
    pub observations: Vec<Observation>,
}

impl ProgressBatch {
    pub fn validate(&self, claim: &RunClaim) -> Result<(), ProgressError> {
        use awaken_agent_contract::event::AgentEvent;
        if self.observations.is_empty()
            || self.observations.len() > PROGRESS_BATCH_ITEMS
            || self.batch_index == u64::MAX
            || self.observations.iter().any(|item| {
                item.event.run_id != claim.run_id
                    || item.assistant_response.as_ref() != Some(&self.response)
                    || !matches!(item.event.kind, AgentEvent::Delta(_))
            })
        {
            return Err(ProgressError::Invalid);
        }
        if serde_json::to_vec(self)
            .map_err(|_| ProgressError::Invalid)?
            .len()
            > PROGRESS_BATCH_BYTES
        {
            return Err(ProgressError::Invalid);
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendProgressRequest {
    pub identity: WorkerIdentity,
    pub claim: RunClaim,
    pub batch: ProgressBatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressReceipt {
    pub next_batch: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ProgressError {
    #[error("this runtime does not support durable progress")]
    Unsupported,
    #[error("invalid durable progress batch")]
    Invalid,
    #[error("progress claim is no longer authorized")]
    Fenced,
    #[error("progress batch position or content conflicts with the published stream")]
    Conflict,
    #[error("durable progress outcome is unavailable")]
    Unavailable,
}

#[async_trait::async_trait]
pub trait AppendRunProgress: Send + Sync {
    async fn append_progress_batch(
        &self,
        claim: &RunClaim,
        batch: ProgressBatch,
    ) -> Result<ProgressReceipt, ProgressError>;
}
