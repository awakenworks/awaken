//! Worker-facing HTTP request values.
//!
//! These serializable values are the single wire source shared by the database-less
//! Worker client and the Coordinator HTTP adapter. They contain only dispatch
//! business data; authentication, trusted time, and lease policy remain server-side.

use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_runtime_contract::CredentialRealizationReceipt;
use awaken_worker_contract::{WorkerHeartbeat, WorkerIdentity, WorkerRegistration};
use serde::{Deserialize, Serialize};

use crate::{DispatchOutcome, PendingInput, RunClaim, RunDispatch, SubmitOptions};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRequest {
    pub claim: RunClaim,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
    #[serde(default)]
    pub checkpoint: Option<StreamCheckpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRequest {
    pub claim: RunClaim,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRealizationRequest {
    pub claim: RunClaim,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
    pub receipt: CredentialRealizationReceipt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindSandboxRequest {
    pub claim: RunClaim,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
    pub sandbox_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterWorkerRequest {
    pub registration: WorkerRegistration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatWorkerRequest {
    pub identity: WorkerIdentity,
    pub heartbeat: WorkerHeartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerIdentityRequest {
    pub identity: WorkerIdentity,
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueRequest {
    pub request: RunDispatch,
    #[serde(default)]
    pub options: Option<SubmitOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimNewRunRequest {
    pub request: RunDispatch,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliverAndClaimRequest {
    pub input: PendingInput,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaimWorkerRequest {
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRunRequest {
    pub run_id: String,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewRequest {
    pub run_id: String,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettleRequest {
    pub run_id: String,
    #[serde(default)]
    pub epoch: u64,
    pub outcome: DispatchOutcome,
    #[serde(default)]
    pub consumed: Vec<String>,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cause/effect design: an older Worker omits optional identity/options fields;
    /// the Coordinator must decode the request with `None` rather than dead-letter
    /// it. Required business data remains mandatory. This covers the compatibility
    /// rule shared by every client/server user of these authoritative DTOs.
    #[test]
    fn optional_transport_fields_default_when_older_writers_omit_them() {
        let request: ClaimWorkerRequest = serde_json::from_str("{}").unwrap();
        assert!(request.identity.is_none());

        let request: WorkerIdentityRequest = serde_json::from_value(serde_json::json!({
            "identity": {
                "worker_id": "worker-1",
                "incarnation_id": "inc-1",
                "generation": 1
            }
        }))
        .unwrap();
        assert!(request.deadline_ms.is_none());
    }
}
