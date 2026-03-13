use super::compaction::{
    find_compaction_plan, unsummarized_start_index, ContextError, ContextSummarizer, SummaryPayload,
};
use super::state::{ArtifactRef, CompactBoundary, ContextAction, ContextState};
use super::transform::{ContextTransform, INTERRUPTED_TOOL_RESULT_NOTICE};
use super::{ContextPlugin, SUMMARY_MESSAGE_OPEN};

use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tirea_contract::runtime::inference::{
    ContextCompactionMode, ContextWindowPolicy, InferenceRequestTransform,
};
use tirea_contract::runtime::phase::{AfterToolExecuteAction, BeforeInferenceAction, Phase};
use tirea_contract::runtime::behavior::ReadOnlyContext;
use tirea_contract::runtime::tool_call::{ToolDescriptor, ToolResult};
use tirea_contract::thread::{Message, Role, Thread};
use tirea_contract::RunPolicy;
use tirea_state::{DocCell, State};

use super::compaction::trim_thread_to_latest_boundary;

fn make_msg_with_id(role: Role, content: &str, id: &str) -> Message {
    match role {
        Role::System => Message::system(content).with_id(id.to_string()),
        Role::User => Message::user(content).with_id(id.to_string()),
        Role::Assistant => Message::assistant(content).with_id(id.to_string()),
        Role::Tool => Message::tool("call_0", content).with_id(id.to_string()),
    }
}

fn assistant_with_tool_calls(
    id: &str,
    calls: Vec<tirea_contract::thread::ToolCall>,
) -> Message {
    Message::assistant_with_tool_calls("tool call", calls).with_id(id.to_string())
}

fn tool_result_with_call(id: &str, call_id: &str, content: &str) -> Message {
    Message::tool(call_id, content).with_id(id.to_string())
}

#[derive(Debug)]
struct TestSummarizer {
    calls: Mutex<Vec<SummaryPayload>>,
    response: String,
}

impl TestSummarizer {
    fn new(response: impl Into<String>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            response: response.into(),
        }
    }

    fn calls(&self) -> Vec<SummaryPayload> {
        self.calls.lock().expect("lock poisoned").clone()
    }
}

#[async_trait]
impl ContextSummarizer for TestSummarizer {
    async fn summarize(&self, payload: SummaryPayload) -> Result<String, ContextError> {
        self.calls.lock().expect("lock poisoned").push(payload);
        Ok(self.response.clone())
    }
}

/// Policy with a very large budget so truncation is a no-op in
/// compaction-focused tests.
fn large_budget_policy() -> ContextWindowPolicy {
    ContextWindowPolicy {
        max_context_tokens: 1_000_000,
        ..ContextWindowPolicy::default()
    }
}

// -- State tests --

#[test]
fn state_default() {
    let state = ContextState::default();
    assert!(state.boundaries.is_empty());
    assert!(state.artifact_refs.is_empty());
}

#[test]
fn reducer_add_boundary_replaces_latest() {
    let mut state = ContextState::default();
    state.reduce(ContextAction::AddBoundary(CompactBoundary {
        covers_through_message_id: "msg-1".into(),
        summary: "First summary".into(),
        original_token_count: 100,
        created_at_ms: 1000,
    }));
    state.reduce(ContextAction::AddBoundary(CompactBoundary {
        covers_through_message_id: "msg-5".into(),
        summary: "Second summary".into(),
        original_token_count: 200,
        created_at_ms: 2000,
    }));

    assert_eq!(state.boundaries.len(), 1);
    assert_eq!(state.boundaries[0].summary, "Second summary");
}

#[test]
fn reducer_add_artifact_deduplicates_by_tool_call_id() {
    let mut state = ContextState::default();
    state.reduce(ContextAction::AddArtifact(ArtifactRef {
        message_id: None,
        tool_call_id: Some("call-1".into()),
        label: "first".into(),
        summary: "preview".into(),
        original_size: 10,
        original_token_count: 3,
    }));
    state.reduce(ContextAction::AddArtifact(ArtifactRef {
        message_id: None,
        tool_call_id: Some("call-1".into()),
        label: "updated".into(),
        summary: "preview-2".into(),
        original_size: 20,
        original_token_count: 4,
    }));

    assert_eq!(state.artifact_refs.len(), 1);
    assert_eq!(state.artifact_refs[0].label, "updated");
}

#[test]
fn reducer_prune_artifacts_by_exact_ids() {
    let mut state = ContextState {
        boundaries: vec![],
        artifact_refs: vec![
            ArtifactRef {
                message_id: Some("msg-1".into()),
                tool_call_id: Some("call-1".into()),
                label: "old".into(),
                summary: "old preview".into(),
                original_size: 100,
                original_token_count: 25,
            },
            ArtifactRef {
                message_id: Some("msg-2".into()),
                tool_call_id: Some("call-2".into()),
                label: "keep".into(),
                summary: "keep preview".into(),
                original_size: 200,
                original_token_count: 50,
            },
        ],
    };

    state.reduce(ContextAction::PruneArtifacts {
        message_ids: vec!["msg-1".into()],
        tool_call_ids: vec![],
    });

    assert_eq!(state.artifact_refs.len(), 1);
    assert_eq!(state.artifact_refs[0].label, "keep");
}

// -- Transform tests --

#[test]
fn transform_no_boundaries_passthrough() {
    let transform = ContextTransform::new(ContextState::default(), large_budget_policy());
    let messages = vec![
        Message::system("sys"),
        Message::user("hello"),
        Message::assistant("hi"),
    ];
    let output = transform.transform(messages.clone(), &[]);
    assert_eq!(output.messages.len(), 3);
    assert_eq!(output.messages[1].content, "hello");
}

#[test]
fn transform_replaces_pre_boundary_messages() {
    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-2".into(),
            summary: "User asked about weather, assistant replied sunny.".into(),
            original_token_count: 50,
            created_at_ms: 1000,
        }],
        artifact_refs: vec![],
    };
    let transform = ContextTransform::new(state, large_budget_policy());

    let messages = vec![
        make_msg_with_id(Role::System, "You are helpful.", "sys-1"),
        make_msg_with_id(Role::User, "What is the weather?", "msg-1"),
        make_msg_with_id(Role::Assistant, "It is sunny today.", "msg-2"),
        make_msg_with_id(Role::User, "Thanks!", "msg-3"),
        make_msg_with_id(Role::Assistant, "You're welcome!", "msg-4"),
    ];

    let output = transform.transform(messages, &[]);
    assert_eq!(output.messages.len(), 4);
    assert_eq!(output.messages[0].content, "You are helpful.");
    assert!(output.messages[1].content.contains(SUMMARY_MESSAGE_OPEN));
    assert!(output.messages[1].content.contains("sunny"));
    assert_eq!(output.messages[2].content, "Thanks!");
    assert_eq!(output.messages[3].content, "You're welcome!");
}

#[test]
fn transform_frontier_boundary_can_replace_all_history() {
    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-2".into(),
            summary: "User asked about weather, assistant replied sunny.".into(),
            original_token_count: 50,
            created_at_ms: 1000,
        }],
        artifact_refs: vec![],
    };
    let transform = ContextTransform::new(state, large_budget_policy());

    let messages = vec![
        make_msg_with_id(Role::System, "You are helpful.", "sys-1"),
        make_msg_with_id(Role::User, "What is the weather?", "msg-1"),
        make_msg_with_id(Role::Assistant, "It is sunny today.", "msg-2"),
    ];

    let output = transform.transform(messages, &[]);
    assert_eq!(output.messages.len(), 2);
    assert_eq!(output.messages[0].content, "You are helpful.");
    assert!(output.messages[1].content.contains(SUMMARY_MESSAGE_OPEN));
    assert!(output.messages[1].content.contains("sunny"));
}

#[test]
fn transform_only_preserves_leading_system_messages() {
    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-3".into(),
            summary: "summary".into(),
            original_token_count: 10,
            created_at_ms: 1,
        }],
        artifact_refs: vec![],
    };
    let transform = ContextTransform::new(state, large_budget_policy());
    let messages = vec![
        make_msg_with_id(Role::System, "system prompt", "sys-1"),
        make_msg_with_id(Role::User, "hello", "msg-1"),
        Message::internal_system("historical reminder").with_id("msg-2".into()),
        make_msg_with_id(Role::Assistant, "before boundary", "msg-3"),
        make_msg_with_id(Role::Assistant, "after boundary", "msg-4"),
    ];

    let output = transform.transform(messages, &[]);
    assert_eq!(output.messages.len(), 3);
    assert_eq!(output.messages[0].content, "system prompt");
    assert!(output.messages[1].content.contains(SUMMARY_MESSAGE_OPEN));
    assert_eq!(output.messages[2].content, "after boundary");
}

#[test]
fn transform_boundary_message_not_found_passthrough() {
    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "nonexistent".into(),
            summary: "Should not appear".into(),
            original_token_count: 0,
            created_at_ms: 1000,
        }],
        artifact_refs: vec![],
    };
    let transform = ContextTransform::new(state, large_budget_policy());

    let messages = vec![
        Message::system("sys"),
        Message::user("hello"),
        Message::assistant("hi"),
    ];
    let output = transform.transform(messages.clone(), &[]);
    assert_eq!(output.messages.len(), 3);
    assert_eq!(output.messages[1].content, "hello");
}

#[test]
fn transform_artifact_ref_replaces_content_by_tool_call_id() {
    let state = ContextState {
        boundaries: vec![],
        artifact_refs: vec![ArtifactRef {
            message_id: None,
            tool_call_id: Some("call-1".into()),
            label: "file.rs".into(),
            summary: "tool: read_file\nstatus: success\ndata preview:\nfn main() {}".into(),
            original_size: 25_000,
            original_token_count: 6_000,
        }],
    };
    let transform = ContextTransform::new(state, large_budget_policy());

    let messages = vec![
        Message::system("sys"),
        make_msg_with_id(Role::User, "Read the file", "msg-1"),
        tool_result_with_call(
            "tool-msg-1",
            "call-1",
            "fn main() { /* very long content */ }",
        ),
        make_msg_with_id(Role::Assistant, "Here is the file content.", "msg-2"),
    ];

    let output = transform.transform(messages, &[]);
    assert_eq!(output.messages.len(), 4);
    assert!(output.messages[2]
        .content
        .contains("[Artifact compacted: file.rs]"));
    assert!(output.messages[2].content.contains("6000 tokens"));
}

#[test]
fn transform_repairs_orphaned_tool_result_after_manual_boundary() {
    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-2".into(),
            summary: "summary".into(),
            original_token_count: 40,
            created_at_ms: 1,
        }],
        artifact_refs: vec![],
    };
    let transform = ContextTransform::new(state, large_budget_policy());
    let messages = vec![
        make_msg_with_id(Role::System, "sys", "sys-1"),
        assistant_with_tool_calls(
            "msg-2",
            vec![tirea_contract::thread::ToolCall::new(
                "call-1",
                "search",
                json!({"q": "rust"}),
            )],
        ),
        tool_result_with_call("tool-1", "call-1", "result"),
        make_msg_with_id(Role::User, "next", "msg-3"),
    ];

    let output = transform.transform(messages, &[]);
    assert_eq!(output.messages.len(), 3);
    assert!(output.messages[1].content.contains(SUMMARY_MESSAGE_OPEN));
    assert_eq!(output.messages[2].content, "next");
}

#[test]
fn transform_patches_dangling_tool_call_after_boundary() {
    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-1".into(),
            summary: "summary".into(),
            original_token_count: 10,
            created_at_ms: 1,
        }],
        artifact_refs: vec![],
    };
    let transform = ContextTransform::new(state, large_budget_policy());
    let messages = vec![
        make_msg_with_id(Role::System, "sys", "sys-1"),
        make_msg_with_id(Role::User, "earlier", "msg-1"),
        assistant_with_tool_calls(
            "msg-2",
            vec![tirea_contract::thread::ToolCall::new(
                "call-1",
                "search",
                json!({"q": "rust"}),
            )],
        ),
        make_msg_with_id(Role::User, "after", "msg-3"),
    ];

    let output = transform.transform(messages, &[]);
    assert_eq!(output.messages.len(), 5);
    assert!(output.messages[1].content.contains(SUMMARY_MESSAGE_OPEN));
    assert_eq!(output.messages[2].role, Role::Assistant);
    assert_eq!(output.messages[3].role, Role::Tool);
    assert_eq!(output.messages[3].tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(output.messages[3].content, INTERRUPTED_TOOL_RESULT_NOTICE);
    assert_eq!(output.messages[4].content, "after");
}

// -- Planner tests --

#[test]
fn planner_keeps_latest_user_turn_raw() {
    let messages = vec![
        Arc::new(make_msg_with_id(Role::User, "old", "msg-1")),
        Arc::new(make_msg_with_id(Role::Assistant, "old reply", "msg-2")),
        Arc::new(make_msg_with_id(Role::User, "current request", "msg-3")),
    ];

    let plan = find_compaction_plan(
        &messages,
        &ContextState::default(),
        &HashMap::new(),
        &HashMap::new(),
        ContextCompactionMode::KeepRecentRawSuffix,
        2,
    )
    .expect("should find boundary before current user");

    assert_eq!(plan.boundary_message_id, "msg-1");
}

#[test]
fn planner_compacts_through_latest_safe_frontier() {
    let messages = vec![
        Arc::new(make_msg_with_id(Role::User, "old", "msg-1")),
        Arc::new(make_msg_with_id(Role::Assistant, "old reply", "msg-2")),
        Arc::new(make_msg_with_id(Role::User, "current request", "msg-3")),
    ];

    let plan = find_compaction_plan(
        &messages,
        &ContextState::default(),
        &HashMap::new(),
        &HashMap::new(),
        ContextCompactionMode::CompactToSafeFrontier,
        2,
    )
    .expect("frontier mode should compact through the latest safe message");

    assert_eq!(plan.boundary_message_id, "msg-3");
}

#[test]
fn planner_does_not_cut_open_tool_round() {
    let messages = vec![
        Arc::new(make_msg_with_id(Role::User, "start", "msg-1")),
        Arc::new(assistant_with_tool_calls(
            "msg-2",
            vec![tirea_contract::thread::ToolCall::new(
                "call-1",
                "search",
                json!({"q": "rust"}),
            )],
        )),
        Arc::new(make_msg_with_id(Role::User, "later user", "msg-3")),
    ];

    let plan = find_compaction_plan(
        &messages,
        &ContextState::default(),
        &HashMap::new(),
        &HashMap::new(),
        ContextCompactionMode::KeepRecentRawSuffix,
        2,
    )
    .expect("older closed prefix should still be compactable");
    assert_eq!(plan.boundary_message_id, "msg-1");
}

#[test]
fn planner_frontier_mode_stops_before_open_tool_round() {
    let messages = vec![
        Arc::new(make_msg_with_id(Role::User, "start", "msg-1")),
        Arc::new(assistant_with_tool_calls(
            "msg-2",
            vec![tirea_contract::thread::ToolCall::new(
                "call-1",
                "search",
                json!({"q": "rust"}),
            )],
        )),
        Arc::new(make_msg_with_id(Role::User, "later user", "msg-3")),
    ];

    let plan = find_compaction_plan(
        &messages,
        &ContextState::default(),
        &HashMap::new(),
        &HashMap::new(),
        ContextCompactionMode::CompactToSafeFrontier,
        2,
    )
    .expect("frontier mode should stop at the latest safe boundary");
    assert_eq!(plan.boundary_message_id, "msg-1");
}

#[test]
fn planner_suffix_mode_respects_custom_raw_suffix_size() {
    let messages = vec![
        Arc::new(make_msg_with_id(Role::User, "older request", "msg-1")),
        Arc::new(make_msg_with_id(Role::Assistant, "older reply", "msg-2")),
        Arc::new(make_msg_with_id(Role::User, "recent request", "msg-3")),
        Arc::new(make_msg_with_id(Role::Assistant, "recent reply", "msg-4")),
    ];

    let plan = find_compaction_plan(
        &messages,
        &ContextState::default(),
        &HashMap::new(),
        &HashMap::new(),
        ContextCompactionMode::KeepRecentRawSuffix,
        3,
    )
    .expect("custom raw suffix should still leave an older prefix to compact");

    assert_eq!(plan.boundary_message_id, "msg-1");
    assert_eq!(plan.covered_message_ids, vec!["msg-1".to_string()]);
}

#[test]
fn planner_skips_messages_already_covered_by_existing_boundary() {
    let messages = vec![
        Arc::new(make_msg_with_id(Role::User, "first", "msg-1")),
        Arc::new(make_msg_with_id(Role::Assistant, "second", "msg-2")),
        Arc::new(make_msg_with_id(Role::User, "third", "msg-3")),
        Arc::new(make_msg_with_id(Role::Assistant, "fourth", "msg-4")),
    ];
    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-2".into(),
            summary: "prior summary".into(),
            original_token_count: 20,
            created_at_ms: 1,
        }],
        artifact_refs: vec![],
    };

    let plan = find_compaction_plan(
        &messages,
        &state,
        &HashMap::new(),
        &HashMap::new(),
        ContextCompactionMode::CompactToSafeFrontier,
        2,
    )
    .expect("unsummarized suffix should remain compactable");

    assert_eq!(plan.start_index, 2);
    assert_eq!(plan.boundary_message_id, "msg-4");
    assert_eq!(
        plan.covered_message_ids,
        vec!["msg-3".to_string(), "msg-4".to_string()]
    );
}

#[test]
fn planner_frontier_mode_advances_to_latest_closed_round_before_open_call() {
    let messages = vec![
        Arc::new(make_msg_with_id(Role::User, "start", "msg-1")),
        Arc::new(assistant_with_tool_calls(
            "msg-2",
            vec![tirea_contract::thread::ToolCall::new(
                "call-1",
                "search",
                json!({"q": "rust"}),
            )],
        )),
        Arc::new(tool_result_with_call("tool-1", "call-1", "done")),
        Arc::new(assistant_with_tool_calls(
            "msg-3",
            vec![tirea_contract::thread::ToolCall::new(
                "call-2",
                "grep",
                json!({"pattern": "fn"}),
            )],
        )),
        Arc::new(make_msg_with_id(Role::User, "later", "msg-4")),
    ];

    let plan = find_compaction_plan(
        &messages,
        &ContextState::default(),
        &HashMap::new(),
        &HashMap::new(),
        ContextCompactionMode::CompactToSafeFrontier,
        2,
    )
    .expect("frontier mode should stop at the most recent closed round");

    assert_eq!(plan.boundary_message_id, "tool-1");
}

// -- Plugin tests --

#[test]
fn plugin_id() {
    let plugin = ContextPlugin::new(ContextWindowPolicy::default());
    use tirea_contract::runtime::AgentBehavior;
    assert_eq!(plugin.id(), "context");
}

#[tokio::test]
async fn plugin_before_inference_no_state_no_threshold() {
    let plugin = ContextPlugin::new(ContextWindowPolicy::default());
    use tirea_contract::runtime::AgentBehavior;

    let config = RunPolicy::new();
    let doc = DocCell::new(json!({}));
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

    let actions = plugin.before_inference(&ctx).await;
    assert_eq!(
        actions.len(),
        1,
        "always registers combined transform for truncation"
    );
}

#[tokio::test]
async fn plugin_before_inference_with_boundary_registers_transform() {
    let plugin = ContextPlugin::new(ContextWindowPolicy::default());
    use tirea_contract::runtime::AgentBehavior;

    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-5".into(),
            summary: "A summary".into(),
            original_token_count: 200,
            created_at_ms: 1000,
        }],
        artifact_refs: vec![],
    };
    let state_value = serde_json::to_value(&state).unwrap();
    let doc = DocCell::new(json!({ "__context": state_value }));
    let config = RunPolicy::new();
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

    let actions = plugin.before_inference(&ctx).await;
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        actions.into_vec().pop().unwrap(),
        BeforeInferenceAction::AddRequestTransform(_)
    ));
}

#[tokio::test]
async fn plugin_before_inference_autocompacts_and_registers_transform() {
    use tirea_contract::runtime::AgentBehavior;

    let summarizer = Arc::new(TestSummarizer::new("compacted summary"));
    let plugin = ContextPlugin::new(ContextWindowPolicy {
        max_context_tokens: 4_000,
        max_output_tokens: 512,
        min_recent_messages: 4,
        enable_prompt_cache: false,
        autocompact_threshold: Some(30),
        ..ContextWindowPolicy::default()
    })
    .with_summarizer(summarizer.clone());

    let messages: Vec<Arc<Message>> = vec![
        Arc::new(make_msg_with_id(
            Role::User,
            &"old request with enough content to exceed the threshold ".repeat(120),
            "msg-1",
        )),
        Arc::new(make_msg_with_id(
            Role::Assistant,
            &"old reply with enough content to exceed the threshold ".repeat(120),
            "msg-2",
        )),
        Arc::new(make_msg_with_id(Role::User, "current request", "msg-3")),
    ];
    let config = RunPolicy::new();
    let doc = DocCell::new(json!({}));
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &messages, &config, &doc);

    let actions = plugin.before_inference(&ctx).await.into_vec();
    assert_eq!(actions.len(), 3);
    assert!(matches!(actions[0], BeforeInferenceAction::State(_)));
    assert!(matches!(actions[1], BeforeInferenceAction::State(_)));
    assert!(matches!(
        actions[2],
        BeforeInferenceAction::AddRequestTransform(_)
    ));

    let calls = summarizer.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].transcript.contains("old request"));
    assert!(!calls[0].transcript.contains("current request"));
}

#[tokio::test]
async fn plugin_before_inference_frontier_mode_compacts_current_frontier() {
    use tirea_contract::runtime::AgentBehavior;

    let summarizer = Arc::new(TestSummarizer::new("frontier summary"));
    let plugin = ContextPlugin::new(ContextWindowPolicy {
        max_context_tokens: 4_000,
        max_output_tokens: 512,
        min_recent_messages: 4,
        enable_prompt_cache: false,
        autocompact_threshold: Some(30),
        compaction_mode: ContextCompactionMode::CompactToSafeFrontier,
        ..ContextWindowPolicy::default()
    })
    .with_summarizer(summarizer.clone());

    let messages: Vec<Arc<Message>> = vec![
        Arc::new(make_msg_with_id(
            Role::User,
            &"old request with enough content to exceed the threshold ".repeat(120),
            "msg-1",
        )),
        Arc::new(make_msg_with_id(
            Role::Assistant,
            &"old reply with enough content to exceed the threshold ".repeat(120),
            "msg-2",
        )),
        Arc::new(make_msg_with_id(Role::User, "current request", "msg-3")),
    ];
    let config = RunPolicy::new();
    let doc = DocCell::new(json!({}));
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &messages, &config, &doc);

    let actions = plugin.before_inference(&ctx).await.into_vec();
    assert_eq!(actions.len(), 3);
    assert!(matches!(actions[0], BeforeInferenceAction::State(_)));
    assert!(matches!(actions[1], BeforeInferenceAction::State(_)));
    assert!(matches!(
        actions[2],
        BeforeInferenceAction::AddRequestTransform(_)
    ));

    let calls = summarizer.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].transcript.contains("old request"));
    assert!(calls[0].transcript.contains("current request"));
}

#[tokio::test]
async fn plugin_before_inference_suffix_mode_respects_custom_raw_suffix() {
    use tirea_contract::runtime::AgentBehavior;

    let summarizer = Arc::new(TestSummarizer::new("suffix summary"));
    let plugin = ContextPlugin::new(ContextWindowPolicy {
        max_context_tokens: 4_000,
        max_output_tokens: 512,
        min_recent_messages: 4,
        enable_prompt_cache: false,
        autocompact_threshold: Some(30),
        compaction_mode: ContextCompactionMode::KeepRecentRawSuffix,
        compaction_raw_suffix_messages: 2,
    })
    .with_summarizer(summarizer.clone());

    let messages: Vec<Arc<Message>> = vec![
        Arc::new(make_msg_with_id(
            Role::User,
            &"old request with enough content to exceed the threshold ".repeat(120),
            "msg-1",
        )),
        Arc::new(make_msg_with_id(
            Role::Assistant,
            &"old reply with enough content to exceed the threshold ".repeat(120),
            "msg-2",
        )),
        Arc::new(make_msg_with_id(
            Role::User,
            &"recent request that should remain raw ".repeat(40),
            "msg-3",
        )),
        Arc::new(make_msg_with_id(
            Role::Assistant,
            &"recent reply that should remain raw ".repeat(40),
            "msg-4",
        )),
    ];
    let config = RunPolicy::new();
    let doc = DocCell::new(json!({}));
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &messages, &config, &doc);

    let actions = plugin.before_inference(&ctx).await.into_vec();
    assert_eq!(actions.len(), 3);

    let calls = summarizer.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].transcript.contains("old request"));
    assert!(calls[0].transcript.contains("old reply"));
    assert!(!calls[0].transcript.contains("recent request"));
    assert!(!calls[0].transcript.contains("recent reply"));
}

#[tokio::test]
async fn plugin_before_inference_frontier_mode_uses_previous_summary_for_delta_only() {
    use tirea_contract::runtime::AgentBehavior;

    let summarizer = Arc::new(TestSummarizer::new("updated frontier summary"));
    let plugin = ContextPlugin::new(ContextWindowPolicy {
        max_context_tokens: 4_000,
        max_output_tokens: 512,
        min_recent_messages: 4,
        enable_prompt_cache: false,
        autocompact_threshold: Some(30),
        compaction_mode: ContextCompactionMode::CompactToSafeFrontier,
        ..ContextWindowPolicy::default()
    })
    .with_summarizer(summarizer.clone());

    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-2".into(),
            summary: "previous summary".into(),
            original_token_count: 100,
            created_at_ms: 1,
        }],
        artifact_refs: vec![],
    };
    let state_value = serde_json::to_value(&state).unwrap();
    let doc = DocCell::new(json!({ "__context": state_value }));
    let messages: Vec<Arc<Message>> = vec![
        Arc::new(make_msg_with_id(
            Role::User,
            &"old request already summarized ".repeat(80),
            "msg-1",
        )),
        Arc::new(make_msg_with_id(
            Role::Assistant,
            &"old reply already summarized ".repeat(80),
            "msg-2",
        )),
        Arc::new(make_msg_with_id(
            Role::User,
            &"new request to merge into summary ".repeat(120),
            "msg-3",
        )),
        Arc::new(make_msg_with_id(
            Role::Assistant,
            &"new reply to merge into summary ".repeat(120),
            "msg-4",
        )),
    ];
    let config = RunPolicy::new();
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &messages, &config, &doc);

    let actions = plugin.before_inference(&ctx).await.into_vec();
    assert_eq!(actions.len(), 3);

    let calls = summarizer.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].previous_summary.as_deref(),
        Some("previous summary")
    );
    assert!(!calls[0].transcript.contains("already summarized"));
    assert!(calls[0].transcript.contains("new request"));
    assert!(calls[0].transcript.contains("new reply"));
}

#[tokio::test]
async fn plugin_before_inference_frontier_mode_excludes_open_tool_round_from_summary() {
    use tirea_contract::runtime::AgentBehavior;

    let summarizer = Arc::new(TestSummarizer::new("frontier summary"));
    let plugin = ContextPlugin::new(ContextWindowPolicy {
        max_context_tokens: 4_000,
        max_output_tokens: 512,
        min_recent_messages: 4,
        enable_prompt_cache: false,
        autocompact_threshold: Some(30),
        compaction_mode: ContextCompactionMode::CompactToSafeFrontier,
        ..ContextWindowPolicy::default()
    })
    .with_summarizer(summarizer.clone());

    let messages: Vec<Arc<Message>> = vec![
        Arc::new(make_msg_with_id(
            Role::User,
            &"very old request with enough content to exceed the threshold ".repeat(120),
            "msg-1",
        )),
        Arc::new(assistant_with_tool_calls(
            "msg-2",
            vec![tirea_contract::thread::ToolCall::new(
                "call-1",
                "search",
                json!({"q": "rust"}),
            )],
        )),
        Arc::new(make_msg_with_id(Role::User, "later user", "msg-3")),
    ];
    let config = RunPolicy::new();
    let doc = DocCell::new(json!({}));
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &messages, &config, &doc);

    let actions = plugin.before_inference(&ctx).await.into_vec();
    assert_eq!(actions.len(), 3);

    let calls = summarizer.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].transcript.contains("very old request"));
    assert!(!calls[0].transcript.contains("tool_calls:"));
    assert!(!calls[0].transcript.contains("later user"));
}

#[tokio::test]
async fn plugin_after_tool_execute_adds_artifact_ref_for_large_result() {
    use tirea_contract::runtime::AgentBehavior;

    let plugin = ContextPlugin::new(ContextWindowPolicy::default())
        .with_artifact_compact_threshold_tokens(10);
    let config = RunPolicy::new();
    let doc = DocCell::new(json!({}));
    let result = ToolResult::success(
        "read_file",
        json!({
            "path": "src/lib.rs",
            "content": "x".repeat(400)
        }),
    );
    let ctx = ReadOnlyContext::new(Phase::AfterToolExecute, "t1", &[], &config, &doc)
        .with_tool_info("read_file", "call-1", None)
        .with_tool_result(&result);

    let actions = plugin.after_tool_execute(&ctx).await.into_vec();
    assert_eq!(actions.len(), 1);
    assert!(matches!(actions[0], AfterToolExecuteAction::State(_)));
}

#[test]
fn latest_boundary_wins() {
    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-4".into(),
            summary: "New summary".into(),
            original_token_count: 100,
            created_at_ms: 2000,
        }],
        artifact_refs: vec![],
    };
    let transform = ContextTransform::new(state, large_budget_policy());

    let messages = vec![
        make_msg_with_id(Role::System, "sys", "sys-1"),
        make_msg_with_id(Role::User, "q1", "msg-1"),
        make_msg_with_id(Role::Assistant, "a1", "msg-2"),
        make_msg_with_id(Role::User, "q2", "msg-3"),
        make_msg_with_id(Role::Assistant, "a2", "msg-4"),
        make_msg_with_id(Role::User, "q3", "msg-5"),
    ];

    let output = transform.transform(messages, &[]);
    assert_eq!(output.messages.len(), 3);
    assert!(output.messages[1].content.contains("New summary"));
    assert_eq!(output.messages[2].content, "q3");
}

// -- Lazy trim tests --

fn thread_with_context_state(
    messages: Vec<Message>,
    cm_state: &ContextState,
) -> Thread {
    let state = json!({
        <ContextState as State>::PATH: serde_json::to_value(cm_state).unwrap()
    });
    Thread::with_initial_state("test-thread", state).with_messages(messages)
}

#[test]
fn trim_no_boundary_is_noop() {
    let messages = vec![
        Message::user("hello").with_id("msg-1".into()),
        Message::assistant("hi").with_id("msg-2".into()),
    ];
    let mut thread = thread_with_context_state(messages, &ContextState::default());
    let original_len = thread.messages.len();

    trim_thread_to_latest_boundary(&mut thread);

    assert_eq!(thread.messages.len(), original_len);
    assert_eq!(thread.messages[0].content, "hello");
    assert_eq!(thread.messages[1].content, "hi");
}

#[test]
fn trim_replaces_pre_boundary_messages() {
    let messages = vec![
        Message::user("q1").with_id("msg-1".into()),
        Message::assistant("a1").with_id("msg-2".into()),
        Message::user("q2").with_id("msg-3".into()),
        Message::assistant("a2").with_id("msg-4".into()),
    ];
    let cm_state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-2".into(),
            summary: "Summary of q1/a1".into(),
            original_token_count: 50,
            created_at_ms: 1000,
        }],
        artifact_refs: vec![],
    };
    let mut thread = thread_with_context_state(messages, &cm_state);

    trim_thread_to_latest_boundary(&mut thread);

    assert_eq!(thread.messages.len(), 3); // summary + msg-3 + msg-4
    assert!(thread.messages[0].content.contains(SUMMARY_MESSAGE_OPEN));
    assert!(thread.messages[0].content.contains("Summary of q1/a1"));
    assert!(thread.messages[0]
        .content
        .contains(super::SUMMARY_MESSAGE_CLOSE));
    assert_eq!(thread.messages[0].role, Role::System);
}

#[test]
fn trim_preserves_post_boundary_messages() {
    let messages = vec![
        Message::user("q1").with_id("msg-1".into()),
        Message::assistant("a1").with_id("msg-2".into()),
        Message::user("q2").with_id("msg-3".into()),
        Message::assistant("a2").with_id("msg-4".into()),
    ];
    let cm_state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-2".into(),
            summary: "Summary".into(),
            original_token_count: 50,
            created_at_ms: 1000,
        }],
        artifact_refs: vec![],
    };
    let mut thread = thread_with_context_state(messages, &cm_state);

    trim_thread_to_latest_boundary(&mut thread);

    assert_eq!(thread.messages[1].content, "q2");
    assert_eq!(thread.messages[1].id.as_deref(), Some("msg-3"));
    assert_eq!(thread.messages[2].content, "a2");
    assert_eq!(thread.messages[2].id.as_deref(), Some("msg-4"));
}

#[test]
fn trim_boundary_message_not_found_is_noop() {
    let messages = vec![
        Message::user("q1").with_id("msg-1".into()),
        Message::assistant("a1").with_id("msg-2".into()),
    ];
    let cm_state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "nonexistent-id".into(),
            summary: "Summary".into(),
            original_token_count: 50,
            created_at_ms: 1000,
        }],
        artifact_refs: vec![],
    };
    let mut thread = thread_with_context_state(messages, &cm_state);

    trim_thread_to_latest_boundary(&mut thread);

    assert_eq!(thread.messages.len(), 2);
    assert_eq!(thread.messages[0].content, "q1");
}

#[test]
fn trim_state_parse_failure_is_noop() {
    let state = json!({ <ContextState as State>::PATH: "not-valid-json-object" });
    let mut thread = Thread::with_initial_state("test-thread", state)
        .with_message(Message::user("q1").with_id("msg-1".into()));

    trim_thread_to_latest_boundary(&mut thread);

    assert_eq!(thread.messages.len(), 1);
    assert_eq!(thread.messages[0].content, "q1");
}

#[test]
fn trim_idempotent() {
    let messages = vec![
        Message::user("q1").with_id("msg-1".into()),
        Message::assistant("a1").with_id("msg-2".into()),
        Message::user("q2").with_id("msg-3".into()),
    ];
    let cm_state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-2".into(),
            summary: "Summary".into(),
            original_token_count: 50,
            created_at_ms: 1000,
        }],
        artifact_refs: vec![],
    };
    let mut thread = thread_with_context_state(messages, &cm_state);

    trim_thread_to_latest_boundary(&mut thread);
    assert_eq!(thread.messages.len(), 2); // summary + msg-3

    // Second call: boundary message "msg-2" no longer in messages → noop.
    trim_thread_to_latest_boundary(&mut thread);
    assert_eq!(thread.messages.len(), 2);
    assert!(thread.messages[0].content.contains("Summary"));
    assert_eq!(thread.messages[1].content, "q2");
}

#[test]
fn unsummarized_start_returns_zero_after_trim() {
    let summary = Arc::new(Message::internal_system("summary of earlier"));
    let msg_a = Arc::new(Message::user("q1").with_id("msg-3".into()));
    let msg_b = Arc::new(Message::assistant("a1").with_id("msg-4".into()));
    let messages = vec![summary, msg_a, msg_b];

    // State still has a boundary referencing a message that was trimmed.
    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-2".into(),
            summary: "old summary".into(),
            original_token_count: 50,
            created_at_ms: 1000,
        }],
        artifact_refs: vec![],
    };

    let result = unsummarized_start_index(&messages, &state);
    assert_eq!(result, Some(0));
}

#[test]
fn compaction_plan_after_trim() {
    let summary = Arc::new(Message::internal_system("summary of earlier"));
    let msg_a = Arc::new(Message::user("q2").with_id("msg-3".into()));
    let msg_b = Arc::new(Message::assistant("a2").with_id("msg-4".into()));
    let msg_c = Arc::new(Message::user("q3").with_id("msg-5".into()));
    let msg_d = Arc::new(Message::assistant("a3").with_id("msg-6".into()));
    let messages = vec![summary, msg_a, msg_b, msg_c, msg_d];

    let state = ContextState {
        boundaries: vec![CompactBoundary {
            covers_through_message_id: "msg-2".into(),
            summary: "old summary".into(),
            original_token_count: 50,
            created_at_ms: 1000,
        }],
        artifact_refs: vec![],
    };

    let tool_states = HashMap::new();
    let suspended = HashMap::new();
    let plan = find_compaction_plan(
        &messages,
        &state,
        &tool_states,
        &suspended,
        ContextCompactionMode::CompactToSafeFrontier,
        0,
    );

    assert!(plan.is_some(), "should produce a compaction plan after trim");
    let plan = plan.unwrap();
    assert_eq!(plan.start_index, 0);
    assert!(plan.boundary_index > 0);
}

// -- Truncation / policy tests (migrated from context_window.rs) --

#[test]
fn default_policy_values() {
    let plugin = ContextPlugin::new(ContextWindowPolicy::default());
    assert_eq!(plugin.policy.max_context_tokens, 200_000);
    assert_eq!(plugin.policy.max_output_tokens, 16_384);
    assert!(plugin.policy.enable_prompt_cache);
    assert_eq!(
        plugin.policy.compaction_mode,
        ContextCompactionMode::KeepRecentRawSuffix
    );
    assert_eq!(plugin.policy.compaction_raw_suffix_messages, 2);
}

#[test]
fn for_model_claude() {
    let plugin = ContextPlugin::for_model("claude-3-opus");
    assert_eq!(plugin.policy.max_context_tokens, 200_000);
    assert!(plugin.policy.enable_prompt_cache);
    assert_eq!(
        plugin.policy.compaction_mode,
        ContextCompactionMode::KeepRecentRawSuffix
    );
}

#[test]
fn for_model_gpt4o() {
    let plugin = ContextPlugin::for_model("gpt-4o-mini");
    assert_eq!(plugin.policy.max_context_tokens, 128_000);
    assert!(!plugin.policy.enable_prompt_cache);
    assert_eq!(
        plugin.policy.compaction_mode,
        ContextCompactionMode::KeepRecentRawSuffix
    );
}

#[test]
fn for_model_unknown_uses_defaults() {
    let plugin = ContextPlugin::for_model("some-custom-model");
    assert_eq!(plugin.policy.max_context_tokens, 200_000);
    assert_eq!(
        plugin.policy.compaction_mode,
        ContextCompactionMode::KeepRecentRawSuffix
    );
}

#[tokio::test]
async fn before_inference_emits_transform() {
    let plugin = ContextPlugin::new(ContextWindowPolicy {
        max_context_tokens: 100_000,
        max_output_tokens: 8_192,
        min_recent_messages: 5,
        enable_prompt_cache: false,
        autocompact_threshold: None,
        ..ContextWindowPolicy::default()
    });

    use tirea_contract::runtime::AgentBehavior;
    let config = RunPolicy::new();
    let doc = DocCell::new(json!({}));
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
    let actions = plugin.before_inference(&ctx).await;
    assert_eq!(actions.len(), 1);

    // Verify the transform works by calling it directly.
    let action = actions.into_vec().pop().unwrap();
    match action {
        BeforeInferenceAction::AddRequestTransform(transform) => {
            let messages = vec![
                Message::system("You are helpful."),
                Message::user("Hello"),
                Message::assistant("Hi!"),
            ];
            let output = transform.transform(messages, &[]);
            // With 100k budget and tiny messages, nothing should be truncated.
            assert_eq!(output.messages.len(), 3);
            assert!(!output.enable_prompt_cache);
        }
        _ => panic!("expected AddRequestTransform"),
    }
}

#[test]
fn transform_with_tool_descriptors_reduces_budget() {
    let policy = ContextWindowPolicy {
        max_context_tokens: 300,
        max_output_tokens: 50,
        min_recent_messages: 1,
        enable_prompt_cache: false,
        autocompact_threshold: None,
        ..ContextWindowPolicy::default()
    };

    let transform_no_tools =
        ContextTransform::new(ContextState::default(), policy.clone());
    let transform_with_tools =
        ContextTransform::new(ContextState::default(), policy);

    let mut messages = vec![Message::system("System.")];
    for i in 0..30 {
        messages.push(Message::user(format!("Question {i} with padding text")));
        messages.push(Message::assistant(format!("Answer {i}")));
    }

    let tools = vec![
        ToolDescriptor::new("search", "Search", "Search the web").with_parameters(
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                }
            }),
        ),
        ToolDescriptor::new("calc", "Calculator", "Evaluate math").with_parameters(
            json!({
                "type": "object",
                "properties": {
                    "expression": { "type": "string" }
                }
            }),
        ),
    ];

    let output_no_tools = transform_no_tools.transform(messages.clone(), &[]);
    let output_with_tools = transform_with_tools.transform(messages, &tools);

    assert!(
        output_with_tools.messages.len() <= output_no_tools.messages.len(),
        "tool descriptors should reduce available budget, keeping fewer messages"
    );
}

#[test]
fn transform_truncates_when_over_budget() {
    let transform = ContextTransform::new(
        ContextState::default(),
        ContextWindowPolicy {
            max_context_tokens: 50, // Very small budget
            max_output_tokens: 10,
            min_recent_messages: 1,
            enable_prompt_cache: true,
            autocompact_threshold: None,
            ..ContextWindowPolicy::default()
        },
    );

    let mut messages = vec![Message::system("System prompt content here.")];
    for i in 0..20 {
        messages.push(Message::user(format!(
            "Message number {i} with some content"
        )));
        messages.push(Message::assistant(format!("Response number {i}")));
    }

    let output = transform.transform(messages, &[]);
    // With a 50-token budget, most messages should be truncated.
    assert!(output.messages.len() < 42); // less than 1 system + 40 history
    assert!(output.enable_prompt_cache);
    // System message is always preserved.
    assert_eq!(output.messages[0].role, Role::System);
}
