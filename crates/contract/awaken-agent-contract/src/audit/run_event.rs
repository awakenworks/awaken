// @formal evidence tool_result.resume_receipt_is_atomic_with_ticket_consumption
// @formal surface exclude=projection_dto
// @formal owner crates/contract/awaken-agent-contract/src/audit/run_event.rs
// @formal reason: Typed audit-event product shape and lossless lowering into a draft payload; it observes
// @formal reason: already-decided facts and cannot authorize or change Run state.
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
use crate::audit::permission::{
    PermissionConfirmation, PermissionDecider, PermissionPolicy, PermissionReasonCode,
};

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
    ///
    /// `decided_by` names who decided: an authored policy, a per-Run
    /// narrowing, or a plugin gate. `policy` names the authored policy that
    /// decided and `reason_code` the grounds of an `auto` judgement; both are
    /// absent when no authored policy decided. All three are additive on the
    /// wire so rows written before they existed still decode; every row written
    /// now carries `decided_by`. A person's answer to the wait an `ask` opened
    /// is not a decision of the gate; it is [`RunEvent::PermissionConfirmed`].
    PermissionDecided {
        tool_id: String,
        call_id: String,
        decision: String,
        decided_by: PermissionDecider,
        policy: Option<PermissionPolicy>,
        reason_code: Option<PermissionReasonCode>,
    },
    /// A person answered a permission ticket for this call (ADR-0030 D5).
    /// Committed with the transition the answer drives, so an allowed call is
    /// never entered and a denied call never completes without it.
    PermissionConfirmed {
        tool_id: String,
        call_id: String,
        result: PermissionConfirmation,
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
                decided_by,
                policy,
                reason_code,
            } => {
                let mut payload = serde_json::json!({
                    "tool_id": tool_id,
                    "call_id": call_id,
                    "decision": decision,
                    "decided_by": decided_by.as_str(),
                });
                if let Some(policy) = policy {
                    payload["policy"] = serde_json::Value::String(policy.as_str().into());
                }
                if let Some(reason_code) = reason_code {
                    payload["reason_code"] = serde_json::Value::String(reason_code.as_str().into());
                }
                (Kind::PermissionDecided, payload)
            }
            RunEvent::PermissionConfirmed {
                tool_id,
                call_id,
                result,
            } => (
                Kind::PermissionConfirmed,
                serde_json::json!({
                    "tool_id": tool_id,
                    "call_id": call_id,
                    "result": result.as_str(),
                }),
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
            decided_by: PermissionDecider::PluginGate,
            policy: None,
            reason_code: None,
        }
        .into();
        assert_eq!(d.kind, Kind::PermissionDecided);
        assert_eq!(
            d.payload,
            serde_json::json!({
                "tool_id": "echo", "call_id": "c1", "decision": "allow",
                "decided_by": "plugin_gate"
            })
        );

        let d: Draft = RunEvent::PermissionConfirmed {
            tool_id: "echo".into(),
            call_id: "c1".into(),
            result: PermissionConfirmation::Deny,
        }
        .into();
        assert_eq!(d.kind, Kind::PermissionConfirmed);
        assert_eq!(
            d.payload,
            serde_json::json!({ "tool_id": "echo", "call_id": "c1", "result": "deny" })
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
        use crate::agent::awaiting::{
            AwaitTarget, PendingTool, ToolAwaitReason, ToolCallAuthority,
        };
        let target = AwaitTarget::ToolCall {
            reason: ToolAwaitReason::Permission,
            call_id: "call-1".into(),
            tool: PendingTool {
                tool_id: "shell".into(),
                arguments: serde_json::json!({"command": "pwd"}),
            },
            authority: ToolCallAuthority::RuntimeToolBatch,
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

    #[test]
    fn permission_decided_lowers_decider_policy_and_reason_additively() {
        // Cause/effect graph: C1 an authored policy decided; C2 that policy was
        // `auto` and did not allow, so it has grounds; C3 a per-Run narrowing
        // decided; C4 a plugin gate decided. Effects: E1 `decided_by` is always
        // written and names exactly the decider; E2 `policy` rides only with a
        // policy decider; E3 `reason_code` rides only with an `auto` refusal or
        // ask; E4 a non-policy decider writes neither `policy` nor
        // `reason_code`, so the two sources stay distinguishable by
        // `decided_by` alone.
        //
        // | Rule | C1 | C2 | C3 | C4 | payload |
        // |---|---|---|---|---|---|
        // | L1 | T | F | F | F | decision + decided_by=policy + policy |
        // | L2 | T | T | F | F | decision + decided_by=policy + policy + reason_code |
        // | L3 | F | F | T | F | decision + decided_by=run_narrowing |
        // | L4 | F | F | F | T | decision + decided_by=plugin_gate |
        let lower = |decision: &str,
                     decided_by: PermissionDecider,
                     policy: Option<PermissionPolicy>,
                     reason_code: Option<PermissionReasonCode>| {
            Draft::from(RunEvent::PermissionDecided {
                tool_id: "bash".into(),
                call_id: "c".into(),
                decision: decision.into(),
                decided_by,
                policy,
                reason_code,
            })
            .payload
        };
        assert_eq!(
            lower(
                "allow",
                PermissionDecider::Policy,
                Some(PermissionPolicy::AlwaysAllow),
                None
            ),
            serde_json::json!({
                "tool_id": "bash", "call_id": "c", "decision": "allow",
                "decided_by": "policy", "policy": "always_allow"
            }),
            "L1"
        );
        assert_eq!(
            lower(
                "ask",
                PermissionDecider::Policy,
                Some(PermissionPolicy::Auto),
                Some(PermissionReasonCode::INDETERMINATE)
            ),
            serde_json::json!({
                "tool_id": "bash", "call_id": "c", "decision": "ask",
                "decided_by": "policy", "policy": "auto", "reason_code": "indeterminate"
            }),
            "L2"
        );
        assert_eq!(
            lower("deny", PermissionDecider::RunNarrowing, None, None),
            serde_json::json!({
                "tool_id": "bash", "call_id": "c", "decision": "deny",
                "decided_by": "run_narrowing"
            }),
            "L3"
        );
        assert_eq!(
            lower("deny", PermissionDecider::PluginGate, None, None),
            serde_json::json!({
                "tool_id": "bash", "call_id": "c", "decision": "deny",
                "decided_by": "plugin_gate"
            }),
            "L4"
        );
    }

    #[test]
    fn permission_decided_payloads_round_trip_old_and_new_rows() {
        // The reader side of the additive contract: historical rows (no
        // decider, no policy) and current rows all decode through the same
        // optional fields; an unknown decider or policy label is rejected
        // rather than silently read as another member of its closed
        // vocabulary; and a reason code outside the named registry is kept as
        // written.
        //
        // | Row | payload | decided_by | policy | reason_code |
        // |---|---|---|---|---|
        // | R1 historical, pre-policy | {decision} | None | None | None |
        // | R2 auto deny | {decision, decided_by: policy, policy: auto, reason_code: high_risk} | Policy | Auto | HIGH_RISK |
        // | R3 historical auto allow | {decision, policy: auto} | None | Auto | None |
        // | R4 rule | {decision, decided_by: policy, policy: rule} | Policy | Rule | None |
        // | R5 unknown policy | {decision, policy: bypass} | decode error | - | - |
        // | R6 open registry | {decision, policy: auto, reason_code: rate_limited} | None | Auto | rate_limited, kept |
        // | R7 run narrowing | {decision, decided_by: run_narrowing} | RunNarrowing | None | None |
        // | R8 plugin gate | {decision, decided_by: plugin_gate} | PluginGate | None | None |
        // | R9 unknown decider | {decision, decided_by: operator} | decode error | - | - |
        #[derive(serde::Deserialize)]
        struct Row {
            decision: String,
            #[serde(default)]
            decided_by: Option<PermissionDecider>,
            #[serde(default)]
            policy: Option<PermissionPolicy>,
            #[serde(default)]
            reason_code: Option<PermissionReasonCode>,
        }
        let read = |payload: serde_json::Value| serde_json::from_value::<Row>(payload);

        let r1 = read(serde_json::json!({ "tool_id": "t", "call_id": "c", "decision": "allow" }))
            .unwrap();
        assert_eq!(
            (
                r1.decision.as_str(),
                r1.decided_by,
                r1.policy,
                r1.reason_code
            ),
            ("allow", None, None, None),
            "R1"
        );

        let r2: Draft = RunEvent::PermissionDecided {
            tool_id: "t".into(),
            call_id: "c".into(),
            decision: "deny".into(),
            decided_by: PermissionDecider::Policy,
            policy: Some(PermissionPolicy::Auto),
            reason_code: Some(PermissionReasonCode::HIGH_RISK),
        }
        .into();
        let r2 = read(r2.payload).unwrap();
        assert_eq!(
            (
                r2.decision.as_str(),
                r2.decided_by,
                r2.policy,
                r2.reason_code
            ),
            (
                "deny",
                Some(PermissionDecider::Policy),
                Some(PermissionPolicy::Auto),
                Some(PermissionReasonCode::HIGH_RISK)
            ),
            "R2"
        );

        let r3 = read(serde_json::json!({ "decision": "allow", "policy": "auto" })).unwrap();
        assert_eq!(
            (r3.decided_by, r3.policy, r3.reason_code),
            (None, Some(PermissionPolicy::Auto), None),
            "R3"
        );

        let r4 = read(serde_json::json!({
            "decision": "allow", "decided_by": "policy", "policy": "rule"
        }))
        .unwrap();
        assert_eq!(
            (r4.decided_by, r4.policy),
            (
                Some(PermissionDecider::Policy),
                Some(PermissionPolicy::Rule)
            ),
            "R4"
        );

        assert!(
            read(serde_json::json!({ "decision": "allow", "policy": "bypass" })).is_err(),
            "R5"
        );

        // The reason code is an open registry: a code a newer producer wrote
        // decodes, and lowering it again writes the exact label back.
        let r6 = read(serde_json::json!({
            "decision": "ask", "policy": "auto", "reason_code": "rate_limited"
        }))
        .expect("R6 an unnamed registry code still decodes");
        assert_eq!(
            (r6.policy, r6.reason_code.clone()),
            (
                Some(PermissionPolicy::Auto),
                Some(PermissionReasonCode::new("rate_limited"))
            ),
            "R6"
        );
        let relowered: Draft = RunEvent::PermissionDecided {
            tool_id: "t".into(),
            call_id: "c".into(),
            decision: r6.decision,
            decided_by: PermissionDecider::Policy,
            policy: r6.policy,
            reason_code: r6.reason_code,
        }
        .into();
        assert_eq!(relowered.payload["reason_code"], "rate_limited", "R6");

        for (rule, decider) in [
            ("R7", PermissionDecider::RunNarrowing),
            ("R8", PermissionDecider::PluginGate),
        ] {
            let written: Draft = RunEvent::PermissionDecided {
                tool_id: "t".into(),
                call_id: "c".into(),
                decision: "deny".into(),
                decided_by: decider,
                policy: None,
                reason_code: None,
            }
            .into();
            let row = read(written.payload).unwrap();
            assert_eq!(
                (row.decided_by, row.policy, row.reason_code),
                (Some(decider), None, None),
                "{rule}"
            );
        }

        assert!(
            read(serde_json::json!({ "decision": "deny", "decided_by": "operator" })).is_err(),
            "R9"
        );
    }
}
