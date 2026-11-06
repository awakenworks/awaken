// @formal surface exclude=read_model_contract
// @formal reason: Session-consumer vocabulary and a read-only scoped query port; grants no execution, recovery or closure authority.
//! Operational observations consumed by the Session application boundary.
use crate::SessionId;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::runs::RunError;
use serde::{Deserialize, Serialize};

/// Evidence only; terminal Run truth and delivery settlement remain separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRunExecutionPhase {
    Unavailable,
    AdmissionPending,
    Admitted,
    RecoveryPending,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRunDispatchObservation {
    pub session_id: String,
    pub run_id: RunId,
    pub phase: SessionRunExecutionPhase,
}
/// The Coordinator composes original Session membership and Dispatch readers;
/// this consumer contract never depends upward on the physical implementation.
#[async_trait::async_trait]
pub trait SessionRunDispatchQueries: Send + Sync {
    async fn observe_for_owner(
        &self,
        owner: &str,
        session: &SessionId,
        run: &RunId,
    ) -> Result<SessionRunDispatchObservation, RunError>;
}
