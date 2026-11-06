//! Relevance-selection contract and deterministic wire helpers.
//!
//! This bounded context deliberately owns no model invocation. The embedding
//! runtime implements [`RecallSelector`] through the same ordinary published
//! Agent execution path used by every other auxiliary task.

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Message, Role};

use crate::localfs::Entry;

/// Selects which saved memories are relevant to a user's message. Implemented by
/// the host through one ordinary published Agent so the Memory bounded context
/// remains free of both model adapters and the auxiliary-run substrate.
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

/// Strictly parse `NONE` or comma-separated bracketed indices. Any prose,
/// duplicate, out-of-range index, or count violation rejects the whole reply.
/// Exposed so an agent-based selector uses the same fail-closed protocol.
pub fn parse_indices(reply: &str, n: usize, max: usize) -> Vec<usize> {
    let reply = reply.trim();
    if reply == "NONE" {
        return Vec::new();
    }
    let parsed = reply
        .split(',')
        .map(str::trim)
        .map(|token| {
            token
                .strip_prefix('[')
                .and_then(|token| token.strip_suffix(']'))
                .filter(|token| !token.is_empty() && token.chars().all(|c| c.is_ascii_digit()))
                .and_then(|token| token.parse::<usize>().ok())
        })
        .collect::<Option<Vec<_>>>();
    let Some(indices) = parsed else {
        return Vec::new();
    };
    if indices.is_empty()
        || indices.len() > max
        || indices.iter().any(|index| *index >= n)
        || indices
            .iter()
            .enumerate()
            .any(|(position, index)| indices[..position].iter().any(|previous| previous == index))
    {
        return Vec::new();
    }
    indices
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
        "User message:\n{query}\n\nSaved memories:\n{lines}\n\nReturn up to {max} relevant indices. Reply ONLY with NONE or comma-separated bracketed indices; no explanation."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    use crate::localfs::MemoryDir;

    #[test]
    fn parse_indices_accepts_only_the_documented_wire_format() {
        assert_eq!(parse_indices("[0], [3]", 5, 10), vec![0, 3]);
        assert_eq!(parse_indices("NONE", 5, 10), Vec::<usize>::new());
        for invalid in [
            "none",
            "relevant: [2]",
            "[0], [0]",
            "[0], [9]",
            "[0], [1], [2]",
            "0, 1",
            "[x]",
            "",
        ] {
            assert!(parse_indices(invalid, 5, 2).is_empty(), "{invalid:?}");
        }
    }

    #[test]
    fn parse_indices_upper_bound_is_exclusive_and_multi_digit() {
        // index == n is out of range (exclusive); n-1 is in range.
        assert_eq!(parse_indices("[5]", 5, 10), Vec::<usize>::new());
        assert_eq!(parse_indices("[4]", 5, 10), vec![4]);
        // Multi-digit indices parse as whole numbers, not per-digit.
        assert_eq!(parse_indices("[10], [3]", 20, 10), vec![10, 3]);
    }

    #[test]
    fn query_from_takes_the_last_user_message_or_empty() {
        use awaken_agent_contract::agent::message::Id as MessageId;
        assert_eq!(query_from(&[]), "");
        let convo = vec![
            Message::text(MessageId("u1".into()), Role::User, "first"),
            Message::text(MessageId("a1".into()), Role::Assistant, "reply"),
            Message::text(MessageId("u2".into()), Role::User, "second"),
        ];
        assert_eq!(query_from(&convo), "second");
    }

    #[test]
    fn manifest_gist_is_first_nonempty_line() {
        let root = std::env::temp_dir().join(format!(
            "awaken-manifest-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = MemoryDir::new(&root);
        store.write("m", "first line\nsecond line").unwrap();
        let m = manifest(&store.entries());
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, 0);
        assert_eq!(m[0].1, "first line");
    }

    #[test]
    fn select_input_numbers_the_manifest() {
        let m = vec![(0usize, "alpha".to_string()), (1usize, "beta".to_string())];
        let s = select_input("hi", &m, 2);
        assert!(s.contains("User message:\nhi"), "{s}");
        assert!(s.contains("[0] alpha"), "{s}");
        assert!(s.contains("[1] beta"), "{s}");
        assert!(s.contains("up to 2 relevant"), "{s}");
    }
}
