//! Relevance selection (optimization ③): when the store grows past a threshold,
//! pick which memories are relevant to the user's message with a **single model
//! call** — not a sub-agent. Cheap, structured-ish, and fail-open on error.

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, ChatRole, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;

use crate::store::Entry;

/// Selects which saved memories are relevant to a user's message. Implemented by
/// the host — over a single model call or a `memory-selector` sub-agent — so the
/// memory crate stays free of the aux-agent substrate (like goal's grader port).
#[async_trait]
pub trait RecallSelector: Send + Sync {
    /// Return the indices (into the manifest) of the memories relevant to `query`,
    /// at most `max`. Empty means none are relevant.
    async fn select(&self, query: &str, manifest: &[(usize, String)], max: usize) -> Vec<usize>;
}

/// A one-line-per-memory manifest `(index, gist)` for a selector to choose from.
pub fn manifest(entries: &[Entry]) -> Vec<(usize, String)> {
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| (i, gist(e)))
        .collect()
}

/// The user's message from a conversation (their most recent user text), used as
/// the relevance query.
pub fn query_from(conversation: &[Message]) -> String {
    conversation
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.text_content())
        .unwrap_or_default()
}

const SELECT_SYSTEM: &str = "\
You select which of a user's saved memories are relevant to their current message. \
Reply with ONLY the bracketed indices of the relevant memories (e.g. `[0], [3]`), \
comma-separated, at most the requested count. If none are relevant, reply NONE. \
Do not explain.";

/// The one-line gist of a memory used in the selection manifest.
fn gist(entry: &Entry) -> String {
    entry
        .content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(140)
        .collect()
}

/// Parse the bracketed/loose integers in `reply` that fall in `0..n`, de-duplicated
/// and in first-seen order, capped at `max`. Empty when the model picked none.
/// Exposed so an agent-based selector can parse its sub-agent's reply.
pub fn parse_indices(reply: &str, n: usize, max: usize) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut num = String::new();
    let flush = |num: &mut String, out: &mut Vec<usize>| {
        if let Ok(i) = num.parse::<usize>()
            && i < n
            && !out.contains(&i)
        {
            out.push(i);
        }
        num.clear();
    };
    for c in reply.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            flush(&mut num, &mut out);
        }
    }
    flush(&mut num, &mut out);
    out.truncate(max);
    out
}

/// The user-message body handed to a `memory-selector` sub-agent: the query plus
/// the numbered manifest. The agent replies with the relevant bracketed indices.
pub fn select_input(query: &str, manifest: &[(usize, String)], max: usize) -> String {
    let lines = manifest
        .iter()
        .map(|(i, g)| format!("[{i}] {g}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "User message:\n{query}\n\nSaved memories:\n{lines}\n\nReturn up to {max} relevant indices."
    )
}

/// Choose the memories relevant to `query`, returning their indices into `entries`.
///
/// Fail-open: if the store is small (`<= max`) or the model call errors, returns
/// the newest `max` indices rather than losing memory. When the model runs and
/// picks nothing, returns empty (trusting the "none relevant" signal).
pub async fn select_relevant(
    llm: &dyn LlmExecutor,
    model: &ModelBinding,
    query: &str,
    entries: &[Entry],
    max: usize,
) -> Vec<usize> {
    let newest_max = || (0..entries.len().min(max)).collect::<Vec<_>>();
    if entries.len() <= max {
        return (0..entries.len()).collect();
    }
    let manifest = entries
        .iter()
        .enumerate()
        .map(|(i, e)| format!("[{i}] {}", gist(e)))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "User message:\n{query}\n\nSaved memories:\n{manifest}\n\nReturn up to {max} relevant indices."
    );
    let request = ChatRequest {
        model_binding: model.clone(),
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: vec![ContentBlock::text(SELECT_SYSTEM)],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::text(prompt)],
            },
        ],
        tools: Vec::new(),
    };
    match llm.infer(request).await {
        Ok(response) => parse_indices(&response.output.text_content(), entries.len(), max),
        Err(_) => newest_max(), // network/backend error: don't lose memory
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatResponse, Result as LlmResult};
    use std::time::SystemTime;

    use crate::store::MemoryStore;

    struct ReplyModel(&'static str);
    #[async_trait]
    impl LlmExecutor for ReplyModel {
        async fn infer(&self, _r: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text(self.0),
                usage: None,
            })
        }
    }
    struct ErrModel;
    #[async_trait]
    impl LlmExecutor for ErrModel {
        async fn infer(&self, _r: ChatRequest) -> LlmResult<ChatResponse> {
            Err(awaken_runtime_contract::llm::Error::Inference(
                "boom".into(),
            ))
        }
    }

    fn entries(n: usize) -> Vec<Entry> {
        let root = std::env::temp_dir().join(format!(
            "awaken-select-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = MemoryStore::new(&root);
        for i in 0..n {
            store
                .write(&format!("m{i}"), &format!("memory number {i}"))
                .unwrap();
        }
        store.entries()
    }

    fn model() -> ModelBinding {
        ModelBinding::new("p", "m", "b")
    }

    #[test]
    fn parse_indices_extracts_in_range_deduped_capped() {
        assert_eq!(parse_indices("[0], [3], [3], [9]", 5, 10), vec![0, 3]);
        assert_eq!(parse_indices("NONE", 5, 10), Vec::<usize>::new());
        assert_eq!(parse_indices("1 2 3 4", 10, 2), vec![1, 2]);
    }

    #[tokio::test]
    async fn small_store_selects_everything_without_a_model_call() {
        let e = entries(3);
        let picked = select_relevant(&ErrModel, &model(), "q", &e, 5).await;
        assert_eq!(picked, vec![0, 1, 2]); // <= max, no call, all kept
    }

    #[tokio::test]
    async fn model_choice_is_honored_when_over_threshold() {
        let e = entries(20);
        let picked = select_relevant(&ReplyModel("relevant: [2], [7]"), &model(), "q", &e, 5).await;
        assert_eq!(picked, vec![2, 7]);
    }

    #[tokio::test]
    async fn model_error_fails_open_to_newest_max() {
        let e = entries(20);
        let picked = select_relevant(&ErrModel, &model(), "q", &e, 3).await;
        assert_eq!(picked, vec![0, 1, 2]); // newest `max`, memory not lost
    }
}
