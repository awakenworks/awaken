// @formal entrypoint session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
// @formal surface exclude=wire_dto
// @formal owner crates/server/awaken-dispatch-http/tests/dispatch_transport_http.rs
// @formal reason: Serializable Worker HTTP request envelopes and a field-preserving constructor;
// @formal reason: authentication and claim authorization stay server-side.
//! Worker-facing HTTP request values.
//!
//! These serializable values are the single wire source shared by the database-less
//! Worker client and the Coordinator HTTP adapter. They contain only dispatch
//! business data; authentication, trusted time, and lease policy remain server-side.

use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::thread::commit::operation::CommitOperation;
use awaken_credential_contract::{AttemptCredentialDeliveryRequest, CredentialRealizationReceipt};
use serde::{Deserialize, Serialize};

use crate::{
    ClaimedCommitCommand, DispatchOutcome, PendingInput, RegisteredWorker, RegistryMutation,
    RunClaim, RunDispatch, SessionRunReservationResolution, SubmitOptions, WorkerHeartbeat,
    WorkerIdentity, WorkerRegistration,
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
    /// Required for Session recovery. It is issued with the claim and must be
    /// revalidated against the canonical WorkQueue before committed Thread
    /// state is read.
    #[serde(default)]
    pub session_work_authority: Option<awaken_session_work_contract::SessionWorkAuthorityReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRealizationRequest {
    pub claim: RunClaim,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
    pub receipt: CredentialRealizationReceipt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialDeliveryRefreshRequest {
    pub claim: RunClaim,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
    pub delivery_request: AttemptCredentialDeliveryRequest,
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

/// Registration result plus the Coordinator-owned registry lease policy. The
/// Worker derives only conservative local timing from this authoritative TTL;
/// it never invents a competing deployment constant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerRegistrationReceipt {
    pub worker: RegisteredWorker,
    pub lease_ttl_ms: u64,
    /// A valid declaration is required before Runtime startup, not before the
    /// registered identity can be withdrawn. Missing/malformed legacy metadata
    /// retains the receipt solely for the existing bounded exit cleanup.
    #[serde(default, deserialize_with = "optional_runtime_protocol")]
    pub runtime_protocol_version: Option<u32>,
}

fn optional_runtime_protocol<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_u64()
        .and_then(|version| u32::try_from(version).ok()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatWorkerRequest {
    pub identity: WorkerIdentity,
    pub heartbeat: WorkerHeartbeat,
}

/// Heartbeat mutation plus a renewed proof duration. Non-applied mutations do
/// not carry a lease because they cannot prove continuing Worker authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerHeartbeatReceipt {
    pub mutation: RegistryMutation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_ms: Option<u64>,
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
    #[serde(deserialize_with = "awaken_lease_contract::fence::Epoch::deserialize_u64")]
    pub lease_epoch: u64,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelinquishRequest {
    pub claim: RunClaim,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
}

/// Exact claim request used by physical attempt begin/quiescence. The complete
/// claim is required because a stale predecessor may acknowledge only its own
/// execution epoch after the durable lease has advanced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptExecutionRequest {
    pub claim: RunClaim,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRunReservationResolutionRequest {
    pub claim: RunClaim,
    #[serde(default)]
    pub identity: Option<WorkerIdentity>,
    pub resolution: SessionRunReservationResolution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettleRequest {
    pub run_id: String,
    /// Required: a missing epoch used to decode as 0, which names no claim.
    #[serde(deserialize_with = "awaken_lease_contract::fence::Epoch::deserialize_u64")]
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

/// Context-bearing form of the existing stream request on the same HTTP
/// endpoint. Flattening preserves the historical JSON shape when the optional
/// coordinate is absent, while [`StreamEventRequest`] stays source-compatible
/// for external Worker adapters that construct its three-field literal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamObservationRequest {
    #[serde(flatten)]
    pub request: StreamEventRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_response:
        Option<awaken_agent_contract::stream::event::AssistantResponseCoordinate>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::event::{AgentEvent, Delta};
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

    /// Renewal-wire cause/effect table. R1 current Worker sends run+epoch and
    /// optional identity -> exact request round-trips. R2 legacy run-only request
    /// -> decode fails closed. Owner, time, and duration remain server-derived.
    /// Constraint: accepting a missing epoch would reintroduce owner-only renewal;
    /// this contract therefore requires a coordinated Control/Worker cutover.
    #[test]
    fn renewal_wire_requires_the_canonical_fencing_epoch() {
        let request = RenewRequest {
            run_id: "run-renew".into(),
            lease_epoch: 7,
            identity: Some(WorkerIdentity::new("worker", "incarnation", 1)),
        };
        let value = serde_json::to_value(&request).expect("R1 encode");
        assert_eq!(value["lease_epoch"], 7, "R1 exact epoch on wire");
        assert_eq!(
            serde_json::from_value::<RenewRequest>(value)
                .expect("R1 decode")
                .lease_epoch,
            7,
        );
        assert!(
            serde_json::from_value::<RenewRequest>(serde_json::json!({
                "run_id": "run-renew"
            }))
            .is_err(),
            "R2 missing epoch fails closed"
        );
    }

    /// Settle and claim wires require the fencing epoch like renewal does.
    /// | Rule | wire                                  | effect            |
    /// | S1   | settle with epoch                     | decodes exactly   |
    /// | S2   | settle without epoch                  | refused on decode |
    /// | L1   | claimed lease without epoch           | refused on decode |
    #[test]
    fn settle_and_lease_wires_require_the_canonical_fencing_epoch() {
        let settle = serde_json::json!({
            "run_id": "run-settle",
            "epoch": 4,
            "outcome": serde_json::to_value(crate::DispatchOutcome::Done).unwrap(),
        });
        assert_eq!(
            serde_json::from_value::<SettleRequest>(settle.clone())
                .expect("S1")
                .epoch,
            4,
            "S1"
        );
        let mut missing = settle;
        missing.as_object_mut().unwrap().remove("epoch");
        assert!(
            serde_json::from_value::<SettleRequest>(missing).is_err(),
            "S2"
        );
        assert!(
            serde_json::from_value::<crate::Lease>(serde_json::json!({
                "run_id": "run-settle",
                "owner": "worker",
                "expires_ms": 10,
            }))
            .is_err(),
            "L1"
        );
    }

    /// The complete dispatch-epoch wire table is shared by all four entrances.
    /// Causes: C1 Lease/RunClaim/Renew/Settle shape; C2 epoch missing, zero,
    /// positive or maximal. Effects: E1 no request is admitted for missing/zero;
    /// E2 a minted epoch is retained exactly (including u64::MAX, which cannot
    /// advance). Missing/zero never reach a server-side effect because decoding
    /// fails before a typed request exists. Credential legacy envelopes have a
    /// separate optional-epoch rule and are not dispatch grants.
    #[test]
    fn every_dispatch_epoch_wire_refuses_zero_and_missing_grants() {
        for epoch in [None, Some(0), Some(1), Some(u64::MAX)] {
            let mut lease = serde_json::json!({
                "run_id": "run-wire", "owner": "worker", "expires_ms": 10,
            });
            let mut claim = serde_json::json!({"run_id": "run-wire", "owner": "worker"});
            let mut renew = serde_json::json!({"run_id": "run-wire"});
            let mut settle = serde_json::json!({
                "run_id": "run-wire", "outcome": crate::DispatchOutcome::Done,
            });
            if let Some(epoch) = epoch {
                lease["epoch"] = epoch.into();
                claim["epoch"] = epoch.into();
                renew["lease_epoch"] = epoch.into();
                settle["epoch"] = epoch.into();
            }
            let decoded = [
                serde_json::from_value::<crate::Lease>(lease).map(|v| v.epoch),
                serde_json::from_value::<crate::RunClaim>(claim).map(|v| v.epoch),
                serde_json::from_value::<RenewRequest>(renew).map(|v| v.lease_epoch),
                serde_json::from_value::<SettleRequest>(settle).map(|v| v.epoch),
            ];
            for (entrance, result) in ["Lease", "RunClaim", "Renew", "Settle"]
                .into_iter()
                .zip(decoded)
            {
                match epoch {
                    Some(epoch) if epoch != 0 => assert_eq!(
                        result.expect("E2 a valid grant must decode"),
                        epoch,
                        "{entrance}"
                    ),
                    _ => assert!(result.is_err(), "E1 {entrance} epoch={epoch:?}"),
                }
            }
        }
    }

    /// Stream-request compatibility cause/effect table: C1 an older Worker
    /// serializes the historical three-field `StreamEventRequest`; C2 a current
    /// Worker supplies an exact assistant-response coordinate. E1 C1 decodes on
    /// the context-aware endpoint with no coordinate; E2 C2 preserves the same
    /// base request and adds exactly one optional context field. Both use the
    /// existing stream endpoint and claim fence. Constraint/Invariant: the
    /// wrapper cannot fork the historical request or weaken its claim fence.
    /// Decision rule: R1 and R2 cover absent and exact-coordinate wire shapes.
    ///
    /// | Rule | writer | coordinate | Effect |
    /// |---|---|---|---|
    /// | R1 | legacy request | absent | E1 |
    /// | R2 | observation wrapper | exact | E2 |
    #[test]
    fn stream_observation_request_wraps_the_source_compatible_request() {
        let request = StreamEventRequest {
            claim: RunClaim {
                run_id: RunId("run-live".into()),
                owner: "worker-1:1:inc-1".into(),
                epoch: 7,
            },
            identity: WorkerIdentity::new("worker-1", "inc-1", 1),
            event: StreamEvent {
                run_id: RunId("run-live".into()),
                kind: AgentEvent::Delta(Delta::TextDelta { delta: "x".into() }),
            },
        };
        let legacy_json = serde_json::to_value(&request).expect("legacy request serializes");
        let decoded: StreamObservationRequest =
            serde_json::from_value(legacy_json).expect("R1 legacy request decodes");
        assert!(decoded.assistant_response.is_none(), "R1/E1");
        assert_eq!(decoded.request.event, request.event, "R1/E1 base event");

        let exact = StreamObservationRequest {
            request,
            assistant_response: Some(
                awaken_agent_contract::stream::event::AssistantResponseCoordinate {
                    thread_id: ThreadId("thread-live".into()),
                    step: 2,
                    response: 3,
                },
            ),
        };
        let exact_json = serde_json::to_value(&exact).expect("R2 exact request serializes");
        assert_eq!(
            exact_json["assistant_response"]["thread_id"],
            serde_json::json!("thread-live"),
            "R2/E2"
        );
        assert_eq!(exact_json["event"]["run_id"], "run-live", "R2/E2");
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
