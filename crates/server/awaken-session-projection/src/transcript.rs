//! Transcript coordinates shared by folding and ordering.

use std::collections::HashMap;

use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;

/// The (Run, step, response) coordinate of every assistant Message owned by
/// one of `run_ids`, keyed by Message id. A response ordinal encoded in the
/// Message id is exact; a full-step response takes the next ordinal after
/// every encoded one seen before it in source order.
///
/// Consumers: the assistant fact identity shared with live previews, the
/// transcript fold, and the canonical order of those facts.
#[must_use]
pub fn assistant_response_coordinates(
    messages: &[Message],
    run_ids: &[RunId],
) -> HashMap<String, (String, usize, usize)> {
    let candidates: std::collections::HashSet<&str> =
        run_ids.iter().map(|run_id| run_id.0.as_str()).collect();
    let mut next_response = HashMap::<(String, usize), usize>::new();
    let mut coordinates = HashMap::new();
    for message in messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
    {
        let Some((run_id, step, response)) = message.id.assistant_coordinates() else {
            continue;
        };
        if !candidates.contains(run_id) {
            continue;
        }
        let next = next_response.entry((run_id.to_owned(), step)).or_default();
        let response = match response {
            Some(response) => {
                *next = (*next).max(response + 1);
                response
            }
            None => *next,
        };
        coordinates.insert(message.id.0.clone(), (run_id.to_owned(), step, response));
    }
    coordinates
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;

    #[test]
    fn indexed_assistant_coordinates_match_ordered_response_rules() {
        // Causes: C1 selected/foreign Run; C2 Assistant/other role; C3 full,
        // partial, malformed identity; C4 duplicate or reordered responses;
        // C5 Run names overlap/contain delimiters; C6 selected Run repeated.
        // Rules A1=!C1 or !C2 or malformed => omitted without advancing;
        // A2=selected Assistant partial => exact encoded ordinal and next=max;
        // A3=selected Assistant full => current next, initially zero;
        // A4=duplicate message => last coordinate wins in source order;
        // A5=overlap/duplicate Runs => exact owner only, never double advance.
        // Exhaust all streams of length 0..=4 over these ten cause partitions.
        // The oracle states the retained prefix-scan behavior; it is test-only,
        // not a second projection path or persisted source of coordinates.
        fn oracle(
            messages: &[Message],
            runs: &[RunId],
        ) -> std::collections::HashMap<String, (String, usize, usize)> {
            let mut runs = runs.to_vec();
            runs.sort_by_key(|run| std::cmp::Reverse(run.0.len()));
            let mut next = std::collections::HashMap::<(String, usize), usize>::new();
            let mut result = std::collections::HashMap::new();
            for message in messages
                .iter()
                .filter(|message| message.role == Role::Assistant)
            {
                for run in &runs {
                    if let Some((step, response)) = message.id.assistant_truncated_response_of(run)
                    {
                        let value = next.entry((run.0.clone(), step)).or_default();
                        *value = (*value).max(response + 1);
                        result.insert(message.id.0.clone(), (run.0.clone(), step, response));
                        break;
                    }
                    if let Some(step) = message.id.assistant_step_of(run) {
                        result.insert(
                            message.id.0.clone(),
                            (
                                run.0.clone(),
                                step,
                                *next.entry((run.0.clone(), step)).or_default(),
                            ),
                        );
                        break;
                    }
                }
            }
            result
        }
        let root = RunId("root".into());
        let nested = RunId("root-assistant-0".into());
        let foreign = RunId("foreign".into());
        let alphabet = [
            (MessageId::assistant(&root, 0), Role::Assistant),
            (MessageId::assistant_truncated(&root, 0, 0), Role::Assistant),
            (MessageId::assistant_truncated(&root, 0, 2), Role::Assistant),
            (MessageId::assistant(&root, 1), Role::Assistant),
            (MessageId::assistant(&nested, 0), Role::Assistant),
            (
                MessageId::assistant_truncated(&nested, 0, 1),
                Role::Assistant,
            ),
            (MessageId::assistant(&foreign, 0), Role::Assistant),
            (MessageId::assistant_truncated(&root, 0, 9), Role::User),
            (MessageId::assistant(&root, 0), Role::Tool),
            (MessageId("root-assistant-bad".into()), Role::Assistant),
        ]
        .map(|(id, role)| Message::text(id, role, "coordinate fixture"));
        let runs = [root.clone(), nested.clone(), root.clone()];
        for len in 0..=4_u32 {
            for code in 0..alphabet.len().pow(len) {
                let mut code = code;
                let messages: Vec<_> = (0..len)
                    .map(|_| {
                        let message = alphabet[code % alphabet.len()].clone();
                        code /= alphabet.len();
                        message
                    })
                    .collect();
                assert_eq!(
                    assistant_response_coordinates(&messages, &runs),
                    oracle(&messages, &runs)
                );
            }
        }
        let messages = alphabet.to_vec();
        assert!(
            assistant_response_coordinates(&messages, &[]).is_empty(),
            "A1 no selected Runs"
        );
        let expected = oracle(&messages, &runs);
        for count in [0, 10, 10_000] {
            let mut expanded: Vec<_> = (0..count)
                .map(|i| RunId(format!("unrelated-{i}")))
                .collect();
            expanded.extend(runs.clone());
            assert_eq!(
                assistant_response_coordinates(&messages, &expanded),
                expected,
                "A5 unrelated Run cardinality {count}"
            );
        }
    }
}
