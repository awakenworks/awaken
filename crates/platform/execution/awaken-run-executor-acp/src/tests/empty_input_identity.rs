//! Stable projection identity from an original committed Running prefix.
use super::*;

#[tokio::test]
async fn acp_result_ids_survive_repeated_empty_input_activations() {
    // Graph: original Running prefix + input + restarted wire sequence ->
    // Tool Message identity -> Managed deduplication. R1 two independent
    // projections from the same immutable prefix/input/frames are identical;
    // R2 accepted Running progress distinguishes the next empty-input result;
    // R3 distinct accepted user inputs obey the same rule; R4 a Tool-only
    // prefix still advances identity without an Assistant message. All four
    // input-present x ToolUse-present rows are evaluated over three prefixes.
    //
    // Each alternative uses ONE recorder for its reader and writer. The
    // original prefix accepts only staged Running progress between projections;
    // no terminal Run is reopened. This is a controlled normalization fixture,
    // not physical replay permission; channel_entry owns terminal absorption.
    for (has_input, has_call) in [(false, false), (false, true), (true, false), (true, true)] {
        let prefix = Arc::new(RecordingCoordinator::default());
        let mut active = activation();
        let mut frames = vec![
            r#"{"type":"tool_call","id":"same-wire-id","name":"read","input":{"path":"report.md"}}"#.into(),
            r#"{"type":"tool_result","id":"same-wire-id","content":"read contents","is_error":false}"#.into(),
            r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
        ];
        if !has_call {
            frames.remove(0);
        }
        let mut result_ids = std::collections::HashSet::new();
        for ordinal in 0..3 {
            active.input = if has_input {
                vec![Message::text(
                    MessageId(format!("input-{ordinal}")),
                    Role::User,
                    "do it",
                )]
            } else {
                vec![]
            };
            let original = prefix.commits.lock().clone();
            let mut alternatives = Vec::new();
            for _ in 0..2 {
                let branch = Arc::new(RecordingCoordinator {
                    commits: awaken_sync::LeafLock::new(original.clone()),
                });
                let result = exec(frames.clone())
                    .execute(
                        active.clone(),
                        RuntimeRunContext::new()
                            .with_reader(branch.clone())
                            .with_commit(branch.clone()),
                    )
                    .await
                    .unwrap();
                assert_eq!(result, RunState::Ended(EndCause::NaturalEnd));
                alternatives.push(branch.settled_commits().last().unwrap().messages.clone());
            }
            assert_eq!(alternatives[0], alternatives[1], "R1 same-prefix replay");
            let messages = alternatives.remove(0);
            let tool = messages.iter().find(|m| m.role == Role::Tool).unwrap();
            assert!(
                result_ids.insert(tool.id.clone()),
                "R2/R3 duplicate result id: {}",
                tool.id.0
            );
            assert_eq!(tool.id.assistant_step_of(&active.run_id), None, "R4");
            crate::commit::commit(
                &RuntimeRunContext::new()
                    .with_reader(prefix.clone())
                    .with_commit(prefix.clone()),
                vec![],
                None,
                &active.thread_id,
                RunDisposition::running(active.run_id.clone()),
                messages,
                vec![],
            )
            .await
            .unwrap();
        }
        assert_eq!(result_ids.len(), 3);
    }
}
