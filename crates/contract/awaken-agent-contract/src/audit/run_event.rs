//! The rich, typed domain event a run emits at its commit boundary.
//!
//! `RunEvent` is the authoritative producer-side vocabulary: each variant is a
//! past-tense fact carrying exactly its data, so events are constructed type-safe
//! rather than as hand-built `json!` blobs. It lowers to a persisted [`Draft`]
//! (`kind` + JSON `payload`) via `From` — the event log keeps a queryable `kind`
//! column plus a flexible payload, while producers speak the rich domain type.

use crate::agent::run::RunState;
use crate::audit::draft::Draft;
use crate::audit::kind::Kind;
use crate::audit::model_request::ModelRequestObservation;

/// A committed run fact, carrying its own typed data. Lowers to a [`Draft`] for
/// the durable event log.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// The run's state transitioned (nothing→Running, Running→Awaiting/Ended).
    RunStateChanged {
        state: RunState,
        await_reason: Option<crate::agent::awaiting::AwaitReason>,
        /// Exact closed resume target at an Awaiting boundary. The active ticket
        /// may later be consumed and deleted; retaining this neutral fact lets a
        /// cold history projection reproduce the original answerable payload.
        await_target: Option<crate::agent::awaiting::AwaitTarget>,
    },
    /// A dispatch lease was durably reclaimed and its replacement claim is
    /// about to execute. `claim_epoch` is the queue's existing fencing
    /// coordinate, not a second attempt counter.
    RunRescheduled { state: RunState, claim_epoch: u64 },
    /// One logical model request completed at the inference seam. Transparent
    /// provider retries stay inside this observation; model-pool failover and
    /// response continuation each produce another event.
    ModelRequestCompleted(ModelRequestObservation),
    /// `commands` committed-state commands rode this checkpoint.
    StateChanged { commands: usize },
    /// The run began awaiting, keyed by `run_id`.
    RunAwaiting { run_id: String },
    /// A protected tool call passed the permission gate (ADR-0030 audit).
    PermissionDecided {
        tool_id: String,
        call_id: String,
        decision: String,
    },
    /// A stable durable-ingress operation consumed this Run's resume ticket.
    /// The correlation detects a different operation attempting to answer the
    /// same closed wait after its active ticket has been removed.
    ResumeApplied {
        operation_id: String,
        correlation_id: String,
    },
    /// A run-end continuation guard decided one round; `detail` is the guard's
    /// opaque payload (the kernel does not interpret it).
    Continuation { detail: serde_json::Value },
}

impl From<RunEvent> for Draft {
    fn from(event: RunEvent) -> Self {
        let (kind, payload) = match event {
            RunEvent::RunStateChanged {
                state,
                await_reason,
                await_target,
            } => {
                let mut payload = serde_json::json!({ "state": state });
                if let Some(reason) = await_reason {
                    payload["await_reason"] = serde_json::to_value(reason)
                        .expect("AwaitReason serialization is infallible");
                }
                if let Some(target) = await_target {
                    payload["await_target"] = serde_json::to_value(target)
                        .expect("AwaitTarget serialization is infallible");
                }
                (Kind::RunStateChanged, payload)
            }
            RunEvent::RunRescheduled { state, claim_epoch } => (
                Kind::RunRescheduled,
                serde_json::json!({ "state": state, "claim_epoch": claim_epoch }),
            ),
            RunEvent::ModelRequestCompleted(observation) => (
                Kind::ModelRequestCompleted,
                serde_json::to_value(observation)
                    .expect("ModelRequestObservation serialization is infallible"),
            ),
            RunEvent::StateChanged { commands } => (
                Kind::StateChanged,
                serde_json::json!({ "commands": commands }),
            ),
            RunEvent::RunAwaiting { run_id } => {
                (Kind::RunAwaiting, serde_json::json!({ "run_id": run_id }))
            }
            RunEvent::PermissionDecided {
                tool_id,
                call_id,
                decision,
            } => (
                Kind::PermissionDecided,
                serde_json::json!({ "tool_id": tool_id, "call_id": call_id, "decision": decision }),
            ),
            RunEvent::ResumeApplied {
                operation_id,
                correlation_id,
            } => (
                Kind::ResumeApplied,
                serde_json::json!({
                    "operation_id": operation_id,
                    "correlation_id": correlation_id
                }),
            ),
            RunEvent::Continuation { detail } => (Kind::Continuation, detail),
        };
        Draft { kind, payload }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowering_preserves_the_persisted_kind_and_payload() {
        let d: Draft = RunEvent::StateChanged { commands: 3 }.into();
        assert_eq!(d.kind, Kind::StateChanged);
        assert_eq!(d.payload, serde_json::json!({ "commands": 3 }));

        let d: Draft = RunEvent::RunAwaiting {
            run_id: "r1".into(),
        }
        .into();
        assert_eq!(d.kind, Kind::RunAwaiting);
        assert_eq!(d.payload, serde_json::json!({ "run_id": "r1" }));

        let d: Draft = RunEvent::PermissionDecided {
            tool_id: "echo".into(),
            call_id: "c1".into(),
            decision: "allow".into(),
        }
        .into();
        assert_eq!(d.kind, Kind::PermissionDecided);
        assert_eq!(
            d.payload,
            serde_json::json!({ "tool_id": "echo", "call_id": "c1", "decision": "allow" })
        );

        let d: Draft = RunEvent::ResumeApplied {
            operation_id: "delivery-1".into(),
            correlation_id: "ticket-1".into(),
        }
        .into();
        assert_eq!(d.kind, Kind::ResumeApplied);
        assert_eq!(
            d.payload,
            serde_json::json!({
                "operation_id": "delivery-1",
                "correlation_id": "ticket-1"
            })
        );

        // Continuation carries the guard's opaque detail verbatim as the payload.
        let detail = serde_json::json!({ "round": 2, "note": "keep going" });
        let d: Draft = RunEvent::Continuation {
            detail: detail.clone(),
        }
        .into();
        assert_eq!(d.kind, Kind::Continuation);
        assert_eq!(d.payload, detail);
    }

    #[test]
    fn run_state_changed_lowers_with_the_state_in_its_payload() {
        // Test design — Causes: Running is lowered with no await reason.
        // Effects: the neutral audit kind and exact serialized state are retained.
        // Constraints/invariants: lowering cannot infer another state or await
        // authority. Decision rule S1: Running+None=>state-only payload.
        use crate::agent::run::{EndCause, RunState};
        let d: Draft = RunEvent::RunStateChanged {
            state: RunState::Ended(EndCause::NaturalEnd),
            await_reason: None,
            await_target: None,
        }
        .into();
        assert_eq!(d.kind, Kind::RunStateChanged);
        // The state serializes under a "state" key; a bare Running is the "Running"
        // string form pinned by the serde-boundary test.
        let d2: Draft = RunEvent::RunStateChanged {
            state: RunState::Running,
            await_reason: None,
            await_target: None,
        }
        .into();
        assert_eq!(d2.payload, serde_json::json!({ "state": "Running" }));
    }

    #[test]
    fn awaiting_state_retains_the_consumable_target_in_the_committed_fact() {
        // Causes: C1 a Run enters Awaiting with an exact permission target; C2
        // the active resume ticket may be deleted after a later resume. Effects:
        // E1 the same RunStateChanged fact retains reason and closed target; E2 a
        // cold reader can recover call/tool identity without a ticket table row.
        // Decision rule R1=C1=>E1, which guarantees E2 under C2. Constraint: the
        // audit fact is existing Runtime commit truth, not a second ticket owner.
        use crate::agent::awaiting::{AwaitTarget, PendingTool, ToolAwaitReason};
        let target = AwaitTarget::ToolCall {
            reason: ToolAwaitReason::Permission,
            call_id: "call-1".into(),
            tool: PendingTool {
                tool_id: "shell".into(),
                arguments: serde_json::json!({"command": "pwd"}),
            },
        };
        let draft: Draft = RunEvent::RunStateChanged {
            state: RunState::Awaiting,
            await_reason: Some(target.reason()),
            await_target: Some(target.clone()),
        }
        .into();
        assert_eq!(draft.kind, Kind::RunStateChanged, "R1/E1");
        assert_eq!(
            serde_json::from_value::<AwaitTarget>(draft.payload["await_target"].clone()).unwrap(),
            target,
            "R1/E1/E2"
        );
    }

    #[test]
    fn run_rescheduled_lowers_the_current_state_and_existing_claim_fence() {
        // Cause/effect graph: C1 the dispatch aggregate reclaimed an active Run;
        // C2 its exact replacement claim has epoch 7; C3 the Run remains
        // Running. Effects: E1 one neutral RunRescheduled kind is emitted; E2
        // the current state and queue-owned fence survive lowering. Decision
        // rule R1=C1+C2+C3=>E1+E2. No attempt counter is manufactured here.
        // Constraints/invariants: the dispatch claim epoch remains the only
        // reschedule fence and audit lowering does not create execution state.
        let draft: Draft = RunEvent::RunRescheduled {
            state: RunState::Running,
            claim_epoch: 7,
        }
        .into();

        assert_eq!(draft.kind, Kind::RunRescheduled, "R1/E1");
        assert_eq!(
            draft.payload,
            serde_json::json!({"state":"Running", "claim_epoch":7}),
            "R1/E2"
        );
    }

    #[test]
    fn model_request_completion_lowers_usage_error_and_provider_retries() {
        // Cause/effect graph: C1 one logical request completes with provider
        // usage; C2 one transparent retry occurred; C3 the final result is an
        // error. Effects: E1 one typed audit kind is written; E2 usage and retry
        // count remain attached to that same request. Decision rule
        // R1=C1+C2+C3=>E1+E2; no provider attempt is promoted into another
        // logical request.
        // Constraints/invariants: one logical request owns exactly one audit
        // record even when its provider implementation retried internally.
        let observation = ModelRequestObservation {
            is_error: true,
            usage: crate::audit::model_request::TokenUsage {
                prompt_tokens: 11,
                completion_tokens: 7,
                cache_read_tokens: 3,
                cache_creation_tokens: 2,
            },
            retry_count: 1,
        };
        let draft: Draft = RunEvent::ModelRequestCompleted(observation).into();

        assert_eq!(draft.kind, Kind::ModelRequestCompleted, "R1/E1");
        assert_eq!(
            draft.payload,
            serde_json::to_value(observation).unwrap(),
            "R1/E2"
        );
    }
}
