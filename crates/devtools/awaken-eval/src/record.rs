//! Record a real run into a replayable [`Case`] (#4b).
//!
//! Converts a completed Run's committed transcript into a fixture: the user input
//! starts the case and each assistant message becomes a [`ScriptedResponse`] (its
//! text plus any tool calls), so replay reproduces the same model responses
//! through the real engine. Expectations are left empty for an author to add.
//!
//! This is the mechanism for the `Purpose::EvalRecording` consent purpose
//! (`awaken-data-subject-application`): capturing real model I/O into a dataset.
//! The caller
//! applies the consent gate — this module only reconstructs data it is handed.

use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::message::{Message, Role};

use crate::{Case, ScriptedResponse, ScriptedToolCall};

/// Reconstruct a replayable [`Case`] from a Run's committed `transcript`. Each
/// assistant message becomes one [`ScriptedResponse`]; a user message is not
/// needed beyond `input` because committed user messages are the Run inputs. The
/// case starts empty of expectations — an author asserts what the replay must
/// satisfy.
#[must_use]
pub fn case_from_run(
    id: impl Into<String>,
    input: impl Into<String>,
    transcript: &[Message],
) -> Case {
    let script = transcript
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .map(assistant_response)
        .collect();
    Case {
        id: id.into(),
        instructions: String::new(),
        input: input.into(),
        script,
        expectations: Vec::new(),
    }
}

/// One recorded assistant message becomes its text and any tool calls.
fn assistant_response(message: &Message) -> ScriptedResponse {
    let tool_calls = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { name, input, .. } => Some(ScriptedToolCall {
                tool_id: name.clone(),
                arguments: input.clone(),
            }),
            _ => None,
        })
        .collect();
    ScriptedResponse {
        text: extract_text(&message.content),
        tool_calls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;

    fn assistant(blocks: Vec<ContentBlock>) -> Message {
        Message {
            id: MessageId("a".to_string()),
            role: Role::Assistant,
            content: blocks,
        }
    }

    #[test]
    fn a_transcript_records_assistant_text_and_tool_calls_in_order() {
        // Test design — Causes: a transcript contains one User input followed by
        // an assistant tool call and final assistant text. Effects: recording
        // excludes the User row and preserves both assistant script entries in
        // order with no invented expectations. Constraints/invariants: committed
        // assistant order is the sole replay script authority. Decision rule R1:
        // User+tool+text=>two ordered assistant entries and empty expectations.
        let transcript = vec![
            Message {
                id: MessageId("u".to_string()),
                role: Role::User,
                content: vec![ContentBlock::text("go")],
            },
            // Response 1: a tool call.
            assistant(vec![ContentBlock::tool_use(
                "c1",
                "search",
                serde_json::json!({"q": "x"}),
            )]),
            // Response 2: the final text.
            assistant(vec![ContentBlock::text("done: 42")]),
        ];

        let case = case_from_run("recorded", "go", &transcript);
        assert_eq!(case.id, "recorded");
        assert_eq!(case.input, "go");
        // Only assistant messages become script entries; the user message does not.
        assert_eq!(case.script.len(), 2);
        assert_eq!(case.script[0].tool_calls.len(), 1);
        assert_eq!(case.script[0].tool_calls[0].tool_id, "search");
        assert_eq!(case.script[1].text, "done: 42");
        // A fresh recording carries no expectations for the author to add.
        assert!(case.expectations.is_empty());
    }

    #[tokio::test]
    async fn a_recorded_case_replays_through_the_real_engine_and_reproduces() {
        use crate::Expectation;

        // A real run's transcript: a `search` tool call, then a text answer.
        let transcript = vec![
            assistant(vec![ContentBlock::tool_use(
                "c1",
                "search",
                serde_json::json!({"q": "x"}),
            )]),
            assistant(vec![ContentBlock::text("done: 42")]),
        ];
        let mut case = case_from_run("rec", "go", &transcript);
        // The author adds what the replay must satisfy.
        case.expectations = vec![
            Expectation::ToolCalled {
                tool_id: "search".to_string(),
            },
            Expectation::OutputContains {
                substring: "42".to_string(),
            },
        ];

        let score = crate::replay::run_case(&case).await;
        assert!(
            score.passed(),
            "the recorded run replays and reproduces: {:?}",
            score.results
        );
    }
}
