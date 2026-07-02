//! Seam-level behavior of the state-machine plugin: the gate (deny/ask/allow),
//! the tool-outcome hook (transitions, emits, metrics, violation log), and the
//! run-end continuation guard — driven through the plugin's public
//! `Contributions`, with a hand-built state `Store`.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{Command, Store};
use awaken_ext_state_machine::{
    ContinuationSettings, FsmTransition, Metrics, RunInstances, StateCell, StateMachineConfig,
    StateMachinePlugin, ThreadInstances, ViolationLog,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext};
use awaken_runtime_contract::plugin::{Plugin, RunEndContext, RunEndDecision, enforce_bound};
use awaken_runtime_contract::tool::{ToolCall, ToolOutput};
use serde_json::json;

const READ_BEFORE_WRITE: &str = r#"{"machines":[{
    "name":"rbw","scope":"thread","key":"{file_path}","initial":"unread","terminal":["written"],
    "transitions":[
        {"on":"Read(file_path ~ \"*\")","from":["unread","written","read"],"to":"read"},
        {"on":"Write(file_path ~ \"*\")","from":"read","to":"written",
         "on_violation":{"action":"deny","reason":"Read {file_path} before writing."}}
    ]}],"continuation":{"max_continuations":5,"message":"Finish: {summary}"}}"#;

fn plugin(config: &str) -> StateMachinePlugin {
    StateMachinePlugin::from_config(StateMachineConfig::from_json_str(config).unwrap()).unwrap()
}

fn ctx(tool: &str, args: serde_json::Value) -> PermissionContext {
    PermissionContext {
        tool_id: tool.to_string(),
        call_id: "call-1".to_string(),
        arguments: args,
    }
}

fn call(tool: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "call-1".to_string(),
        tool_id: tool.to_string(),
        arguments: args,
    }
}

fn apply(store: &mut Store, commands: &[Command]) {
    for command in commands {
        store.apply(command);
    }
}

fn read_state(store: &mut Store, key: &str) {
    let command = ThreadInstances::commit(
        store,
        FsmTransition {
            machine: "rbw".into(),
            key: key.into(),
            to: "read".into(),
        },
    );
    store.apply(&command);
}

// ---------------------------------------------------------------------------
// Plugin manifest / resolve / bound
// ---------------------------------------------------------------------------

#[test]
fn manifest_admits_resolved_contributions() {
    let p = plugin(READ_BEFORE_WRITE);
    let manifest = p.manifest();
    let contributions = p.resolve();
    assert_eq!(manifest.id, "state_machine");
    assert!(enforce_bound(&manifest, &contributions).is_ok());
    assert_eq!(contributions.tool_gates.len(), 1);
    assert_eq!(contributions.tool_observers.len(), 1);
    assert_eq!(contributions.run_end_guards.len(), 1);
    assert_eq!(contributions.state_keys.len(), 4);
}

#[test]
fn plugin_new_from_compiled_machines_resolves_within_bound() {
    let machines = StateMachineConfig::from_json_str(READ_BEFORE_WRITE)
        .unwrap()
        .into_machines()
        .unwrap();
    let p = StateMachinePlugin::new(machines, ContinuationSettings::default());
    assert!(enforce_bound(&p.manifest(), &p.resolve()).is_ok());
}

#[test]
fn from_config_rejects_invalid_machine() {
    let bad = r#"{"machines":[{"name":"m","initial":"a",
        "transitions":[{"on":"Read(","from":"a","to":"b"}]}]}"#;
    assert!(
        StateMachinePlugin::from_config(StateMachineConfig::from_json_str(bad).unwrap()).is_err()
    );
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gate_denies_write_before_read() {
    let p = plugin(READ_BEFORE_WRITE);
    let contributions = p.resolve();
    let gate = &contributions.tool_gates[0];
    let store = Store::new();
    let outcome = gate
        .gate(&ctx("Write", json!({"file_path": "a.rs"})), &store)
        .await;
    match outcome {
        GateOutcome::Block { reason } => assert_eq!(reason, "Read a.rs before writing."),
        other => panic!("expected block, got {other:?}"),
    }
}

#[tokio::test]
async fn gate_allows_write_after_read() {
    let p = plugin(READ_BEFORE_WRITE);
    let contributions = p.resolve();
    let gate = &contributions.tool_gates[0];
    let mut store = Store::new();
    read_state(&mut store, "a.rs");
    let outcome = gate
        .gate(&ctx("Write", json!({"file_path": "a.rs"})), &store)
        .await;
    assert_eq!(outcome, GateOutcome::Allow);
}

#[tokio::test]
async fn gate_allows_read_and_ignores_keyless_call() {
    let p = plugin(READ_BEFORE_WRITE);
    let contributions = p.resolve();
    let gate = &contributions.tool_gates[0];
    let store = Store::new();
    assert_eq!(
        gate.gate(&ctx("Read", json!({"file_path": "a.rs"})), &store)
            .await,
        GateOutcome::Allow
    );
    // No file_path ⇒ machine does not apply ⇒ allow.
    assert_eq!(
        gate.gate(&ctx("Write", json!({"other": 1})), &store).await,
        GateOutcome::Allow
    );
}

#[tokio::test]
async fn gate_suspends_on_ask() {
    let cfg = r#"{"machines":[{"name":"m","key":"{file_path}","initial":"unread",
        "transitions":[{"on":"Write(file_path ~ \"*\")","from":"read","to":"written",
            "on_violation":{"action":"ask"}}]}]}"#;
    let p = plugin(cfg);
    let contributions = p.resolve();
    let gate = &contributions.tool_gates[0];
    let store = Store::new();
    match gate
        .gate(&ctx("Write", json!({"file_path": "a.rs"})), &store)
        .await
    {
        GateOutcome::Suspend { ticket_id } => assert_eq!(ticket_id, "fsm-call-1"),
        other => panic!("expected suspend, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tool-outcome hook
// ---------------------------------------------------------------------------

#[tokio::test]
async fn observer_advances_on_success_and_records_metric() {
    let p = plugin(READ_BEFORE_WRITE);
    let contributions = p.resolve();
    let observer = &contributions.tool_observers[0];
    let mut store = Store::new();
    let output = ToolOutput::ok("call-1", "contents");
    let reaction = observer
        .after_tool(&call("Read", json!({"file_path": "a.rs"})), &output, &store)
        .await;
    apply(&mut store, &reaction.state);
    assert_eq!(
        ThreadInstances::load(&store).current("rbw", "a.rs"),
        Some("read")
    );
    assert_eq!(Metrics::load(&store).total.transitioned, 1);
    assert!(reaction.messages.is_empty());
}

#[tokio::test]
async fn observer_does_not_advance_on_error() {
    let p = plugin(READ_BEFORE_WRITE);
    let contributions = p.resolve();
    let observer = &contributions.tool_observers[0];
    let mut store = Store::new();
    let output = ToolOutput::error("call-1", "boom");
    let reaction = observer
        .after_tool(&call("Read", json!({"file_path": "a.rs"})), &output, &store)
        .await;
    apply(&mut store, &reaction.state);
    assert_eq!(ThreadInstances::load(&store).current("rbw", "a.rs"), None);
}

#[tokio::test]
async fn observer_records_deny_metric_and_violation_on_blocked_write() {
    let p = plugin(READ_BEFORE_WRITE);
    let contributions = p.resolve();
    let observer = &contributions.tool_observers[0];
    let mut store = Store::new();
    // The gate blocked the write; the runtime feeds an error result back.
    let output = ToolOutput::error("call-1", "blocked: Read a.rs before writing.");
    let reaction = observer
        .after_tool(
            &call("Write", json!({"file_path": "a.rs"})),
            &output,
            &store,
        )
        .await;
    apply(&mut store, &reaction.state);
    assert_eq!(Metrics::load(&store).total.denied, 1);
    let log = ViolationLog::load(&store);
    assert_eq!(log.records.len(), 1);
    assert_eq!(log.records[0].tool_name, "Write");
}

#[tokio::test]
async fn observer_emits_warn_message_and_records() {
    let cfg = r#"{"machines":[{"name":"m","scope":"run","key":"{file_path}","initial":"unread",
        "transitions":[{"on":"Write(file_path ~ \"*\")","from":"read","to":"written",
            "on_violation":{"action":"warn","reason":"writing unread {file_path}"}}]}]}"#;
    let p = plugin(cfg);
    let contributions = p.resolve();
    let observer = &contributions.tool_observers[0];
    let mut store = Store::new();
    let output = ToolOutput::ok("call-1", "wrote");
    let reaction = observer
        .after_tool(
            &call("Write", json!({"file_path": "a.rs"})),
            &output,
            &store,
        )
        .await;
    apply(&mut store, &reaction.state);
    assert_eq!(reaction.messages.len(), 1);
    assert_eq!(reaction.messages[0].text_content(), "writing unread a.rs");
    assert_eq!(Metrics::load(&store).total.warned, 1);
    assert_eq!(ViolationLog::load(&store).records.len(), 1);
}

#[tokio::test]
async fn observer_emits_transition_message() {
    let cfg = r#"{"machines":[{"name":"m","scope":"run","key":"","initial":"a",
        "transitions":[{"on":"X","from":"a","to":"b",
            "emit":{"target":"conversation","content":"moved to b","role":"assistant"}}]}]}"#;
    let p = plugin(cfg);
    let contributions = p.resolve();
    let observer = &contributions.tool_observers[0];
    let mut store = Store::new();
    let reaction = observer
        .after_tool(
            &call("X", json!({})),
            &ToolOutput::ok("call-1", "ok"),
            &store,
        )
        .await;
    apply(&mut store, &reaction.state);
    assert_eq!(reaction.messages.len(), 1);
    assert_eq!(reaction.messages[0].text_content(), "moved to b");
    assert_eq!(RunInstances::load(&store).current("m", ""), Some("b"));
    assert_eq!(Metrics::load(&store).total.emitted, 1);
}

// ---------------------------------------------------------------------------
// Run-end continuation guard
// ---------------------------------------------------------------------------

async fn evaluate_guard(p: &StateMachinePlugin, store: &Store, fc: usize) -> RunEndDecision {
    let contributions = p.resolve();
    let guard = &contributions.run_end_guards[0];
    let conversation: Vec<Message> = Vec::new();
    let ctx = RunEndContext {
        run_id: RunId("run-1".into()),
        conversation: &conversation,
        forced_continuations: fc,
        cancellation: None,
        state: store,
    };
    guard.evaluate(&ctx).await
}

#[tokio::test]
async fn guard_steers_while_instance_incomplete() {
    let p = plugin(READ_BEFORE_WRITE);
    let mut store = Store::new();
    read_state(&mut store, "a.rs"); // "read" is not terminal ("written" is)
    match evaluate_guard(&p, &store, 0).await {
        RunEndDecision::Steer { feedback, .. } => {
            assert_eq!(feedback, "Finish: rbw[a.rs]=read");
        }
        RunEndDecision::Complete { .. } => panic!("expected steer, got complete"),
    }
}

#[tokio::test]
async fn guard_completes_when_all_terminal() {
    let p = plugin(READ_BEFORE_WRITE);
    let mut store = Store::new();
    let command = ThreadInstances::commit(
        &store,
        FsmTransition {
            machine: "rbw".into(),
            key: "a.rs".into(),
            to: "written".into(),
        },
    );
    store.apply(&command);
    assert!(matches!(
        evaluate_guard(&p, &store, 0).await,
        RunEndDecision::Complete { .. }
    ));
}

#[tokio::test]
async fn guard_completes_when_cap_reached_or_disabled() {
    let p = plugin(READ_BEFORE_WRITE);
    let mut store = Store::new();
    read_state(&mut store, "a.rs");
    // forced_continuations == cap ⇒ complete despite incomplete instance.
    assert!(matches!(
        evaluate_guard(&p, &store, 5).await,
        RunEndDecision::Complete { .. }
    ));

    // A machine set with no continuation cap never steers.
    let no_cont = plugin(
        r#"{"machines":[{"name":"rbw","key":"{file_path}","initial":"unread","terminal":["written"],
        "transitions":[{"on":"Read(file_path ~ \"*\")","from":"unread","to":"read"}]}]}"#,
    );
    assert!(matches!(
        evaluate_guard(&no_cont, &store, 0).await,
        RunEndDecision::Complete { .. }
    ));
}
