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
//!
//! Shape conformance alone only ever asks whether what this adapter emits is
//! declared; it cannot notice a variant the SDK declares that nothing here
//! emits, because an unemitted variant has no sample to walk. The second graph
//! closes that direction: C7 the SDK declares variant V of the union a field
//! names; C8 some sample emits V; C9 the table below names V unreachable with
//! its reason. Effects: E7 a declared variant neither emitted nor named fails;
//! E8 a named variant some sample does emit fails as stale; E9 a named variant
//! the SDK no longer declares fails as rotted. Rules: S4=C7+!C8+!C9=>E7;
//! S5=C8+C9=>E8; S6=C9+!C7=>E9.

use std::collections::{BTreeMap, BTreeSet};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::audit::permission::PermissionReasonCode;
use awaken_protocol_managed::types::{
    AutoEvaluatedPermission, BudgetLimit, ConfirmResult, Currency, EvaluatedPermission, Event,
    ModelConfig, MonetaryAmount, OutboundKind, OutcomeRubric, RetryStatus, SessionAgent,
    SessionError, SessionMultiagentCoordinator, SessionMultiagentRosterEntry, SessionThreadAgent,
    SpanModelUsage, StopReason, ToolEvaluation, Usage,
};
use serde_json::{Value, json};

/// Required SDK fields this adapter does not emit yet, each with the reason the
/// gap is real rather than an oversight. A gap listed here is a visible debt;
/// one not listed here fails the build. Empty is the goal state, and reaching
/// it is what keeps this list honest rather than decorative.
fn known_gaps() -> BTreeMap<&'static str, BTreeMap<&'static str, &'static str>> {
    BTreeMap::new()
}

/// Nested variants the SDK declares that this adapter cannot emit, each with the
/// reason it is unreachable rather than forgotten. Keyed by `field[variant]`,
/// not by the path the variant was found under: one closed Rust enum serves each
/// wire union wherever it appears, so coverage is a property of the union.
fn unreachable_variants() -> BTreeMap<&'static str, &'static str> {
    let repository = "`SessionError` carries no `repository_url`, which this \
         variant requires, and a repository fault fails sandbox provisioning \
         before a Run can commit a `session.error`; reaching it is a projection \
         that does not exist yet, not a missing match arm.";
    BTreeMap::from([
        (
            "stop_details[refusal]",
            "`StopDetails` is uninhabited because no Runtime transition claims a \
             refusal, so `Some` cannot be constructed and the key is provably null.",
        ),
        (
            "stop_reason[refusal]",
            "The companion of the refusal detail: projecting it needs the same \
             refusal transition the Runtime does not have.",
        ),
        (
            "error[billing_error]",
            "`SessionError::classify` maps no neutral fault code here; billing \
             exhaustion arrives as `usage_limit` and is projected as the \
             model-owned `model_request_failed_error`. Visible debt: a client \
             keying recovery on the billing class does not see it.",
        ),
        (
            "error[credential_host_unreachable_error]",
            "`SessionError` carries neither `credential_id` nor `vault_id`, which \
             this variant requires, and no fault code reaches it.",
        ),
        (
            "error[model_overloaded_error]",
            "`SessionError::classify` maps no neutral fault code here; an \
             overloaded model arrives as the retryable `rate_limited`.",
        ),
        ("error[repository_authentication_error]", repository),
        ("error[repository_checkout_error]", repository),
        ("error[repository_clone_error]", repository),
        ("error[repository_forbidden_error]", repository),
        ("error[repository_not_found_error]", repository),
    ])
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
            if value.is_null() {
                // C6 a required nested discriminator may also declare `null`;
                // E6 accept null only when the generated SDK shape says so.
                // S2+C6=>E6, while an undeclared null still fails closed.
                assert!(
                    shape["nullable"]
                        .as_array()
                        .is_some_and(|fields| fields.iter().any(|name| name == field)),
                    "{at}.{field}: null is not declared by the SDK"
                );
                continue;
            }
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

/// The discriminator each nested field actually carries, keyed by the field name
/// the union is reached through. A `null` field carries no `type` and so records
/// nothing, which is what makes a nullable union's only variant unemitted.
fn collect_emitted(object: &Value, shape: &Value, out: &mut BTreeMap<String, BTreeSet<String>>) {
    let Some(nested) = shape.get("nested").and_then(Value::as_object) else {
        return;
    };
    for (field, variants) in nested {
        let Some(value) = object.get(field) else {
            continue;
        };
        let Some(discriminator) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        out.entry(field.clone())
            .or_default()
            .insert(discriminator.to_string());
        if let Some(variant) = variants.get(discriminator) {
            collect_emitted(value, variant, out);
        }
    }
}

/// Every variant the SDK declares for each nested field. Recursion stops under a
/// variant already named unreachable, so one decision costs one reason instead of
/// a cascade of unreachable children. Two events declaring different variants for
/// one field name would make the keying claim coverage it does not have, so that
/// fails here rather than merging two unions into one claim.
fn collect_declared(
    shape: &Value,
    named: &BTreeMap<&str, &str>,
    at: &str,
    out: &mut BTreeMap<String, BTreeSet<String>>,
) {
    let Some(nested) = shape.get("nested").and_then(Value::as_object) else {
        return;
    };
    for (field, variants) in nested {
        let Some(variants) = variants.as_object() else {
            continue;
        };
        let declared: BTreeSet<String> = variants.keys().cloned().collect();
        match out.get(field) {
            Some(first) => assert_eq!(
                *first, declared,
                "{at}.{field}: one field name, two different declared unions"
            ),
            None => {
                out.insert(field.clone(), declared);
            }
        }
        for (name, variant) in variants {
            if named.contains_key(format!("{field}[{name}]").as_str()) {
                continue;
            }
            collect_declared(variant, named, &format!("{at}.{field}[{name}]"), out);
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
        // C4 a non-refusal Session idle has no detail; E4 the current SDK still
        // requires the key with null. S1+C4 => stop_reason plus explicit null.
        event(OutboundKind::SessionStatusIdle {
            stop_reason: StopReason::EndTurn,
            stop_details: None,
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
            // C5 a non-refusal Thread idle has no detail; S1+C5 => the same
            // required null without a second thread-specific serializer.
            stop_details: None,
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

/// Extra events whose only job is to reach a nested variant `samples()` does not.
/// `samples()` stays exactly one event per `OutboundKind`, which is what
/// `every_variant_is_sampled` counts, so variant coverage needs a second set;
/// both go through the same field-by-field shape check. The errors are built
/// through `SessionError::classify`, so a sample exercises the mapping production
/// uses instead of restating its result.
fn variant_samples() -> Vec<Event> {
    let coordinator = SessionAgent {
        id: "agt_coordinator".into(),
        kind: "agent",
        version: 4,
        model: ModelConfig::new("model-1"),
        name: "Coordinator".into(),
        description: Some("delegates".into()),
        system: Some("delegate carefully".into()),
        tools: Vec::new(),
        mcp_servers: Vec::new(),
        skills: Vec::new(),
        multiagent: Some(SessionMultiagentCoordinator {
            kind: "coordinator",
            agents: vec![SessionMultiagentRosterEntry::Agent(SessionThreadAgent {
                id: "agt_researcher".into(),
                kind: "agent",
                version: 2,
                model: ModelConfig::new("model-2"),
                name: "Researcher".into(),
                description: None,
                system: None,
                tools: Vec::new(),
                mcp_servers: Vec::new(),
                skills: Vec::new(),
            })],
        }),
    };
    let tool_use = |evaluated_permission, evaluation| {
        event(OutboundKind::AgentToolUse {
            name: "bash".into(),
            input: json!({"command": "ls"}),
            evaluated_permission: Some(evaluated_permission),
            evaluation: Some(evaluation),
            session_thread_id: None,
        })
    };
    vec![
        // stop_reason: the three further reasons a committed Run does project.
        event(OutboundKind::SessionStatusIdle {
            stop_reason: StopReason::BudgetReached,
            stop_details: None,
        }),
        event(OutboundKind::SessionStatusIdle {
            stop_reason: StopReason::RequiresAction {
                event_ids: vec!["sevt_tool".into()],
            },
            stop_details: None,
        }),
        event(OutboundKind::SessionStatusIdle {
            stop_reason: StopReason::RetriesExhausted,
            stop_details: None,
        }),
        // error and retry_status: every classification this adapter reaches. Both
        // mcp variants require `mcp_server_name`, so the message has to be one the
        // classifier can read a server name out of — which is exactly the pairing
        // a hand-written literal would have been free to get wrong.
        event(OutboundKind::SessionError {
            error: SessionError::classify(
                "mcp_connection_failed",
                "mcp server `calc`: connection refused",
            ),
        }),
        event(OutboundKind::SessionError {
            error: SessionError::classify(
                "mcp_authentication_failed",
                "mcp server `calc`: HTTP 401",
            ),
        }),
        event(OutboundKind::SessionError {
            error: SessionError::classify("rate_limited", "429 from the model"),
        }),
        event(OutboundKind::SessionError {
            error: SessionError::classify("context_overflow", "the prompt exceeds the window"),
        }),
        // evaluation: the two policies that decide without consulting `auto`.
        tool_use(EvaluatedPermission::Allow, ToolEvaluation::AlwaysAllow),
        tool_use(EvaluatedPermission::Ask, ToolEvaluation::AlwaysAsk),
        // evaluated_permission: `auto` allowing, the one outcome no other sample
        // reaches.
        tool_use(
            EvaluatedPermission::Allow,
            ToolEvaluation::Auto {
                evaluated_permission: AutoEvaluatedPermission::Allow,
            },
        ),
        // rubric: an outcome graded against an uploaded file.
        event(OutboundKind::UserDefineOutcome {
            description: "done".into(),
            rubric: OutcomeRubric::File {
                file_id: "file_1".into(),
            },
            max_iterations: None,
            outcome_id: "outc_2".into(),
        }),
        // agent, multiagent and budget: the Session snapshot an update carries
        // when the Agent coordinates and the Session has a spend limit.
        event(OutboundKind::SessionUpdated {
            title: None,
            metadata: BTreeMap::new(),
            agent: Some(coordinator),
            budget: Some(Some(BudgetLimit::Limit {
                max_list_cost: MonetaryAmount {
                    amount: "25".into(),
                    currency: Currency::USD,
                },
            })),
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
    for sample in samples().into_iter().chain(variant_samples()) {
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

/// S4/S5/S6: the direction shape conformance cannot see. A variant the SDK
/// declares is either emitted by a sample or named unreachable with its reason —
/// and the table may only shrink honestly, so an entry that became emitted, or
/// that the SDK stopped declaring, fails too.
#[test]
fn every_nested_variant_the_sdk_declares_is_emitted_or_named_unreachable() {
    let shapes = oracle_shapes();
    let named = unreachable_variants();
    let mut declared = BTreeMap::new();
    for (type_name, shape) in shapes.as_object().expect("shapes is an object") {
        collect_declared(shape, &named, type_name, &mut declared);
    }
    let mut emitted = BTreeMap::new();
    for sample in samples().into_iter().chain(variant_samples()) {
        let Some(shape) = shapes.get(sample.type_str()) else {
            continue;
        };
        let value = serde_json::to_value(&sample).expect("event serializes");
        collect_emitted(&value, shape, &mut emitted);
    }
    for (field, variants) in &declared {
        for variant in variants {
            let key = format!("{field}[{variant}]");
            let emits = emitted
                .get(field)
                .is_some_and(|seen| seen.contains(variant));
            let excused = named.contains_key(key.as_str());
            assert!(
                emits || excused,
                "{key}: the SDK declares this variant, no sample emits it, and no \
                 reason names it unreachable (S4)"
            );
            assert!(
                !(emits && excused),
                "{key}: named unreachable, yet a sample emits it; drop the entry (S5)"
            );
        }
    }
    for key in named.keys() {
        let (field, variant) = key
            .strip_suffix(']')
            .and_then(|key| key.split_once('['))
            .unwrap_or_else(|| panic!("{key}: an entry is spelled `field[variant]`"));
        assert!(
            declared
                .get(field)
                .is_some_and(|variants| variants.contains(variant)),
            "{key}: the SDK no longer declares this variant; drop the entry (S6)"
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
