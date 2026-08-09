//! Worker-facing HTTP request values.
//!
//! These serializable values are the single wire source shared by the database-less
//! Worker client and the Coordinator HTTP adapter. They contain only dispatch
//! business data; authentication, trusted time, and lease policy remain server-side.

use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::thread::commit::operation::CommitOperation;
use awaken_runtime_contract::CredentialRealizationReceipt;
use serde::{Deserialize, Serialize};

use crate::{
    ClaimedCommitCommand, DispatchOutcome, PendingInput, RunClaim, RunDispatch, SubmitOptions,
    WorkerHeartbeat, WorkerIdentity, WorkerRegistration,
};

/// Complete Worker-to-Coordinator envelope for one claim-fenced committed-truth
/// operation. Authentication remains transport metadata; `identity` proves the
/// registered incarnation named by the claim owner at the Coordinator edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimedCommitRequest {
    pub claim: RunClaim,
    pub operation: CommitOperation,
    pub identity: WorkerIdentity,
}

impl ClaimedCommitRequest {
    #[must_use]
    pub fn new(command: ClaimedCommitCommand, identity: WorkerIdentity) -> Self {
        Self {
            claim: command.claim,
            operation: command.operation,
            identity,
        }
    }
}

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

/// One best-effort live event bound to the exact dispatch claim that produced it.
/// It is observation only; committed messages and Run state remain authoritative.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEventRequest {
    pub claim: RunClaim,
    pub identity: WorkerIdentity,
    pub event: StreamEvent,
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::commit::operation::{CommitOperationId, CommitPayloadHash};
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};

    /// Cause/effect design: an older Worker omits optional identity/options fields;
    /// the Coordinator must decode the request with `None` rather than dead-letter
    /// it. Required business data remains mandatory. This covers the compatibility
    /// rule shared by every client/server user of these authoritative wire values.
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

    /// Cause/effect decision table for the sole claimed-commit wire owner:
    /// R1 a complete application command plus registered identity becomes the
    /// flat historical HTTP shape; R2 JSON round-trip preserves claim, operation,
    /// payload hash and incarnation. Either failure would let client and server
    /// silently regain independent request contracts.
    #[test]
    fn claimed_commit_request_is_the_single_flat_wire_shape() {
        let run_id = RunId("run-transport".into());
        let commit = ThreadCommit {
            thread_id: ThreadId("thread-transport".into()),
            run: RunDisposition::ended(run_id.clone(), EndCause::NaturalEnd),
            messages: vec![Message::text(
                MessageId("message-transport".into()),
                Role::Assistant,
                "committed",
            )],
            state: Vec::new(),
            events: Vec::new(),
        };
        let command = ClaimedCommitCommand {
            claim: RunClaim {
                run_id: run_id.clone(),
                owner: "worker-transport:incarnation".into(),
                epoch: 7,
            },
            operation: CommitOperation {
                operation_id: CommitOperationId::new(run_id, 0),
                expected_thread_version: 3,
                payload_hash: CommitPayloadHash("sha256:transport".into()),
                commit,
            },
        };
        let request = ClaimedCommitRequest::new(
            command,
            WorkerIdentity::new("worker-transport", "incarnation", 4),
        );
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["claim", "identity", "operation"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            "R1"
        );
        assert_eq!(
            serde_json::from_value::<ClaimedCommitRequest>(value).unwrap(),
            request,
            "R2"
        );
    }
}
