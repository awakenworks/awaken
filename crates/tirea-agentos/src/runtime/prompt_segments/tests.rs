use super::PromptSegmentsPlugin;
use serde_json::json;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::inference::{
    PromptSegmentConsumePolicy, PromptSegmentState, StoredPromptSegment,
};
use tirea_contract::runtime::phase::{BeforeInferenceAction, Phase};
use tirea_contract::RunPolicy;
use tirea_state::DocCell;

fn segment(key: &str, content: &str, consume: PromptSegmentConsumePolicy) -> StoredPromptSegment {
    StoredPromptSegment {
        namespace: "test".into(),
        key: key.into(),
        content: content.into(),
        cooldown_turns: 0,
        target: tirea_contract::runtime::inference::ContextMessageTarget::Session,
        consume,
    }
}

#[tokio::test]
async fn injects_persisted_segments_as_context_messages() {
    let plugin = PromptSegmentsPlugin;
    let config = RunPolicy::new();
    let doc = DocCell::new(json!({
        "__prompt_segments": {
            "items": [
                segment("a", "hello", PromptSegmentConsumePolicy::Persistent)
            ]
        }
    }));
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

    let actions: Vec<_> = plugin.before_inference(&ctx).await.into_iter().collect();
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        BeforeInferenceAction::AddContextMessage(entry) => {
            assert_eq!(entry.key, "test:a");
            assert_eq!(entry.content, "hello");
        }
        _ => panic!("expected AddContextMessage"),
    }
}

#[tokio::test]
async fn emits_consume_action_for_after_emit_segments() {
    let plugin = PromptSegmentsPlugin;
    let config = RunPolicy::new();
    let state = PromptSegmentState {
        items: vec![segment("a", "hello", PromptSegmentConsumePolicy::AfterEmit)],
    };
    let doc = DocCell::new(json!({
        "__prompt_segments": serde_json::to_value(state).unwrap()
    }));
    let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

    let actions: Vec<_> = plugin.before_inference(&ctx).await.into_iter().collect();
    assert_eq!(actions.len(), 2);
    assert!(actions
        .iter()
        .any(|a| matches!(a, BeforeInferenceAction::AddContextMessage(_))));
    assert!(actions
        .iter()
        .any(|a| matches!(a, BeforeInferenceAction::State(_))));
}
