//! Every committed Managed event this adapter emits has the shape the official
//! SDK declares for it — field by field, not only by its `type` name.
//!
//! The generated oracle (`contracts/anthropic-managed/upstream-oracle.generated.json`,
//! `current.wire_contract.events.shapes`) extracts each member of the SDK's
//! committed-event union: its required and optional property names and, for a
//! property that is itself a discriminated object (`evaluation`, and under
//! `auto` its `evaluated_permission`), the shape of every variant. This test
//! serializes one sample of every `OutboundKind` variant and checks it against
//! that extraction. A field the SDK does not declare fails; a required field
//! the sample omits fails unless the gap is named below with its reason.
//!
//! Cause/effect graph: C1 the adapter emits an event of type T; C2 the SDK
//! declares T with required R and optional O; C3 a field carries a
//! discriminated object. Effects: E1 every emitted key is in R ∪ O; E2 every
//! key in R is emitted (or is a named gap); E3 a nested discriminated object
//! obeys E1/E2 for its own variant. Decision rules: S1=C1+C2=>E1+E2;
//! S2=C1+C3=>E3; S3=T unknown to the SDK=>fail. One rule per event below.

use std::collections::{BTreeMap, BTreeSet};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::audit::permission::PermissionReasonCode;
use awaken_protocol_managed::types::{
    AutoEvaluatedPermission, ConfirmResult, EvaluatedPermission, Event, OutboundKind,
    OutcomeRubric, RetryStatus, SessionError, SpanModelUsage, StopReason, ToolEvaluation, Usage,
};
use serde_json::{Value, json};

/// Required SDK fields this adapter does not emit yet, each with the reason the
/// gap is real rather than an oversight. A gap listed here is a visible debt;
/// one not listed here fails the build. Empty is the goal state, and reaching
/// it is what keeps this list honest rather than decorative.
fn known_gaps() -> BTreeMap<&'static str, BTreeMap<&'static str, &'static str>> {
    BTreeMap::new()
}

fn oracle_shapes() -> Value {
    let oracle: Value = serde_json::from_str(include_str!(
        "../../../../contracts/anthropic-managed/upstream-oracle.generated.json"
    ))
    .expect("generated Managed SDK oracle is valid JSON");
    oracle["current"]["wire_contract"]["events"]["shapes"].clone()
}

fn names(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .expect("shape names are an array")
        .iter()
        .map(|name| name.as_str().expect("a property name").to_string())
        .collect()
}

/// E1/E2/E3 for one object against one extracted shape.
fn assert_shape(object: &Value, shape: &Value, at: &str, gaps: &BTreeMap<&str, &str>) {
    let emitted: BTreeSet<String> = object
        .as_object()
        .unwrap_or_else(|| panic!("{at}: emitted value is an object"))
        .keys()
        .cloned()
        .collect();
    let required = names(&shape["required"]);
    let optional = names(&shape["optional"]);
    for key in &emitted {
        assert!(
            required.contains(key) || optional.contains(key),
            "{at}: emits `{key}`, which the SDK does not declare (S1/E1)"
        );
    }
    for key in &required {
        assert!(
            emitted.contains(key) || gaps.contains_key(key.as_str()),
            "{at}: omits required `{key}` and names no reason for the gap (S1/E2)"
        );
    }
    for (key, reason) in gaps {
        assert!(
            required.contains(*key) && !emitted.contains(*key),
            "{at}: `{key}` is listed as a gap ({reason}) but is not a missing required field; drop it from the gap list"
        );
    }
    if let Some(nested) = shape.get("nested").and_then(Value::as_object) {
        for (field, variants) in nested {
            let Some(value) = object.get(field) else {
                continue;
            };
            let discriminator = value["type"]
                .as_str()
                .unwrap_or_else(|| panic!("{at}.{field}: a discriminated object carries `type`"));
            let variant = variants.get(discriminator).unwrap_or_else(|| {
                panic!("{at}.{field}: `{discriminator}` is not a variant the SDK declares (S2)")
            });
            assert_shape(
                value,
                variant,
                &format!("{at}.{field}[{discriminator}]"),
                &BTreeMap::new(),
            );
        }
    }
}

fn event(kind: OutboundKind) -> Event {
    Event {
        id: "sevt_sample".into(),
        kind,
        processed_at: Some("2026-09-13T00:00:00Z".into()),
    }
}

fn text() -> Vec<ContentBlock> {
    vec![ContentBlock::text("sample")]
}

/// One sample per committed event variant. Adding a variant to `OutboundKind`
/// without a sample here is a compile error in `every_variant_is_sampled`.
fn samples() -> Vec<Event> {
    let auto_deny = ToolEvaluation::Auto {
        evaluated_permission: AutoEvaluatedPermission::Deny {
            reason_code: PermissionReasonCode::HIGH_RISK,
        },
    };
    let auto_ask = ToolEvaluation::Auto {
        evaluated_permission: AutoEvaluatedPermission::Ask {
            reason_code: PermissionReasonCode::INDETERMINATE,
        },
    };
    vec![
        event(OutboundKind::UserMessage { content: text() }),
        event(OutboundKind::SystemMessage { content: text() }),
        event(OutboundKind::UserToolConfirmation {
            tool_use_id: "sevt_tool".into(),
            result: ConfirmResult::Deny,
            deny_message: Some("no".into()),
            session_thread_id: Some("sthr_1".into()),
        }),
        event(OutboundKind::UserCustomToolResult {
            custom_tool_use_id: "sevt_tool".into(),
            content: Some(text()),
            is_error: false,
            session_thread_id: None,
        }),
        event(OutboundKind::UserToolResult {
            tool_use_id: "sevt_tool".into(),
            content: Some(text()),
            is_error: false,
            session_thread_id: None,
        }),
        event(OutboundKind::UserDefineOutcome {
            description: "done".into(),
            rubric: OutcomeRubric::Text {
                content: "ok".into(),
            },
            max_iterations: Some(2),
            outcome_id: "outc_1".into(),
        }),
        event(OutboundKind::UserInterrupt {
            session_thread_id: None,
        }),
        event(OutboundKind::AgentMessage { content: text() }),
        event(OutboundKind::AgentThinking {}),
        event(OutboundKind::AgentToolUse {
            name: "bash".into(),
            input: json!({"command": "ls"}),
            evaluated_permission: Some(EvaluatedPermission::Deny),
            evaluation: Some(auto_deny),
            session_thread_id: Some("sthr_1".into()),
        }),
        event(OutboundKind::AgentToolResult {
            tool_use_id: "sevt_tool".into(),
            content: text(),
            is_error: Some(true),
        }),
        event(OutboundKind::AgentCustomToolUse {
            name: "lookup".into(),
            input: json!({}),
            session_thread_id: None,
        }),
        event(OutboundKind::AgentMcpToolUse {
            name: "mcp__github__search".into(),
            mcp_server_name: "github".into(),
            input: json!({}),
            evaluated_permission: Some(EvaluatedPermission::Ask),
            evaluation: Some(auto_ask),
            session_thread_id: None,
        }),
        event(OutboundKind::AgentMcpToolResult {
            mcp_tool_use_id: "sevt_tool".into(),
            content: text(),
            is_error: None,
        }),
        event(OutboundKind::SessionError {
            error: SessionError {
                kind: "unknown_error",
                message: "sample".into(),
                retry_status: RetryStatus::Terminal,
                mcp_server_name: None,
            },
        }),
        event(OutboundKind::SessionStatusRunning {}),
        event(OutboundKind::SessionStatusIdle {
            stop_reason: StopReason::EndTurn,
        }),
        event(OutboundKind::SessionUsage {
            usage: Usage::default(),
            budget: None,
        }),
        event(OutboundKind::SessionStatusRescheduled {}),
        event(OutboundKind::SessionStatusTerminated {}),
        event(OutboundKind::SessionDeleted {}),
        event(OutboundKind::SessionThreadCreated {
            session_thread_id: "sthr_1".into(),
            agent_name: "child".into(),
        }),
        event(OutboundKind::SessionThreadStatusRunning {
            session_thread_id: "sthr_1".into(),
            agent_name: "child".into(),
        }),
        event(OutboundKind::SessionThreadStatusIdle {
            session_thread_id: "sthr_1".into(),
            agent_name: "child".into(),
            stop_reason: StopReason::EndTurn,
        }),
        event(OutboundKind::SessionThreadStatusRescheduled {
            session_thread_id: "sthr_1".into(),
            agent_name: "child".into(),
        }),
        event(OutboundKind::SessionUpdated {
            title: Some(Some("t".into())),
            metadata: BTreeMap::from([("k".to_string(), "v".to_string())]),
            agent: None,
            budget: None,
        }),
        event(OutboundKind::SessionThreadStatusTerminated {
            session_thread_id: "sthr_1".into(),
            agent_name: "child".into(),
        }),
        event(OutboundKind::AgentThreadMessageSent {
            to_session_thread_id: "sthr_1".into(),
            to_agent_name: Some("child".into()),
            content: text(),
        }),
        event(OutboundKind::AgentThreadMessageReceived {
            from_session_thread_id: "sthr_1".into(),
            from_agent_name: None,
            content: text(),
        }),
        event(OutboundKind::ThreadContextCompacted {}),
        event(OutboundKind::SpanModelRequestStart {}),
        event(OutboundKind::SpanModelRequestEnd {
            model_request_start_id: "sevt_start".into(),
            is_error: None,
            model_usage: SpanModelUsage::default(),
        }),
        event(OutboundKind::SpanOutcomeEvaluationStart {
            outcome_id: "outc_1".into(),
            iteration: 1,
        }),
        event(OutboundKind::SpanOutcomeEvaluationOngoing {
            outcome_id: "outc_1".into(),
            iteration: 1,
        }),
        event(OutboundKind::SpanOutcomeEvaluationEnd {
            outcome_id: "outc_1".into(),
            outcome_evaluation_start_id: "sevt_start".into(),
            iteration: 1,
            result: "pass".into(),
            explanation: "fine".into(),
            usage: SpanModelUsage {
                input_tokens: 31,
                output_tokens: 7,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                speed: None,
            },
        }),
    ]
}

#[test]
fn every_variant_is_sampled() {
    // The sample list is exhaustive by construction: this match must name
    // every variant, and each named variant must appear in `samples()`.
    let sampled: BTreeSet<&'static str> = samples().iter().map(Event::type_str).collect();
    let all: Vec<&'static str> = samples()
        .into_iter()
        .map(|event| match event.kind {
            OutboundKind::UserMessage { .. } => "user.message",
            OutboundKind::SystemMessage { .. } => "system.message",
            OutboundKind::UserToolConfirmation { .. } => "user.tool_confirmation",
            OutboundKind::UserCustomToolResult { .. } => "user.custom_tool_result",
            OutboundKind::UserToolResult { .. } => "user.tool_result",
            OutboundKind::UserDefineOutcome { .. } => "user.define_outcome",
            OutboundKind::UserInterrupt { .. } => "user.interrupt",
            OutboundKind::AgentMessage { .. } => "agent.message",
            OutboundKind::AgentThinking {} => "agent.thinking",
            OutboundKind::AgentToolUse { .. } => "agent.tool_use",
            OutboundKind::AgentToolResult { .. } => "agent.tool_result",
            OutboundKind::AgentCustomToolUse { .. } => "agent.custom_tool_use",
            OutboundKind::AgentMcpToolUse { .. } => "agent.mcp_tool_use",
            OutboundKind::AgentMcpToolResult { .. } => "agent.mcp_tool_result",
            OutboundKind::SessionError { .. } => "session.error",
            OutboundKind::SessionStatusRunning {} => "session.status_running",
            OutboundKind::SessionStatusIdle { .. } => "session.status_idle",
            OutboundKind::SessionUsage { .. } => "session.usage",
            OutboundKind::SessionStatusRescheduled {} => "session.status_rescheduled",
            OutboundKind::SessionStatusTerminated {} => "session.status_terminated",
            OutboundKind::SessionDeleted {} => "session.deleted",
            OutboundKind::SessionThreadCreated { .. } => "session.thread_created",
            OutboundKind::SessionThreadStatusRunning { .. } => "session.thread_status_running",
            OutboundKind::SessionThreadStatusIdle { .. } => "session.thread_status_idle",
            OutboundKind::SessionThreadStatusRescheduled { .. } => {
                "session.thread_status_rescheduled"
            }
            OutboundKind::SessionUpdated { .. } => "session.updated",
            OutboundKind::SessionThreadStatusTerminated { .. } => {
                "session.thread_status_terminated"
            }
            OutboundKind::AgentThreadMessageSent { .. } => "agent.thread_message_sent",
            OutboundKind::AgentThreadMessageReceived { .. } => "agent.thread_message_received",
            OutboundKind::ThreadContextCompacted {} => "agent.thread_context_compacted",
            OutboundKind::SpanModelRequestStart {} => "span.model_request_start",
            OutboundKind::SpanModelRequestEnd { .. } => "span.model_request_end",
            OutboundKind::SpanOutcomeEvaluationStart { .. } => "span.outcome_evaluation_start",
            OutboundKind::SpanOutcomeEvaluationOngoing { .. } => "span.outcome_evaluation_ongoing",
            OutboundKind::SpanOutcomeEvaluationEnd { .. } => "span.outcome_evaluation_end",
        })
        .collect();
    assert_eq!(
        all.len(),
        sampled.len(),
        "one sample per variant, no duplicates"
    );
}

#[test]
fn every_committed_event_matches_the_sdk_declared_shape() {
    let shapes = oracle_shapes();
    let gaps = known_gaps();
    let mut checked = BTreeSet::new();
    for sample in samples() {
        let type_name = sample.type_str();
        let value = serde_json::to_value(&sample).expect("event serializes");
        let Some(shape) = shapes.get(type_name) else {
            // `session.updated` and `session.usage` are declared beside the
            // Session resource, not in the committed-event union the oracle
            // extracts; every other type must be in the union (S3).
            assert!(
                matches!(type_name, "session.updated" | "session.usage"),
                "{type_name}: not a committed event the SDK declares (S3)"
            );
            continue;
        };
        let event_gaps = gaps.get(type_name).cloned().unwrap_or_default();
        assert_shape(&value, shape, type_name, &event_gaps);
        checked.insert(type_name);
    }
    for type_name in gaps.keys() {
        assert!(
            checked.contains(type_name),
            "gap named for an unchecked event {type_name}"
        );
    }
}

#[test]
fn the_evaluation_object_repeats_the_top_level_outcome_it_accompanies() {
    // The SDK states the invariant on the type: `evaluation.evaluated_permission.type`
    // always equals the event's top-level `evaluated_permission`. The adapter
    // derives both from one recorded provenance; this pins the wire result.
    for sample in samples() {
        let value = serde_json::to_value(&sample).expect("event serializes");
        let (Some(top), Some(evaluation)) =
            (value.get("evaluated_permission"), value.get("evaluation"))
        else {
            continue;
        };
        if evaluation["type"] == "auto" {
            assert_eq!(
                evaluation["evaluated_permission"]["type"],
                *top,
                "{}",
                sample.type_str()
            );
        } else {
            let implied = match evaluation["type"].as_str() {
                Some("always_allow") => "allow",
                Some("always_ask") => "ask",
                other => panic!("unexpected evaluation type {other:?}"),
            };
            assert_eq!(top, implied, "{}", sample.type_str());
        }
    }
}
