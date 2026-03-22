use super::PromptSegmentState;
use super::PromptSegmentsPlugin;
use serde_json::json;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::inference::{ContextMessage, ContextMessageTarget};
use tirea_contract::runtime::phase::{BeforeInferenceAction, Phase};
use tirea_contract::RunPolicy;
use tirea_state::DocCell;

#[tokio::test]
async fn injects_persisted_segments_as_context_messages() {
    let plugin = PromptSegmentsPlugin;
    let config = RunPolicy::new();
    let state = PromptSegmentState {
        items: vec![ContextMessage::session("a", "hello")],
    };
    let doc = DocCell::new(json!({
        "__prompt_segments": serde_json::to_value(state).unwrap()
    }));
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

    let actions: Vec<_> = plugin.before_inference(&ctx).await.into_iter().collect();
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        BeforeInferenceAction::AddContextMessage(entry) => {
            assert_eq!(entry.key, "a");
            assert_eq!(entry.content, "hello");
            assert_eq!(entry.target, ContextMessageTarget::Session);
        }
        _ => panic!("expected AddContextMessage"),
    }
}

#[tokio::test]
async fn marks_after_emit_segments_on_context_messages() {
    let plugin = PromptSegmentsPlugin;
    let config = RunPolicy::new();
    let state = PromptSegmentState {
        items: vec![ContextMessage::session("a", "hello").with_consume_after_emit(true)],
    };
    let doc = DocCell::new(json!({
        "__prompt_segments": serde_json::to_value(state).unwrap()
    }));
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

    let actions: Vec<_> = plugin.before_inference(&ctx).await.into_iter().collect();
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        BeforeInferenceAction::AddContextMessage(entry) => {
            assert!(entry.consume_after_emit);
        }
        _ => panic!("expected AddContextMessage"),
    }
}
