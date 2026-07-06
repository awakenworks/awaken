//! Bounded recall: load saved memories into one context block for a new
//! conversation, but keep it bounded so a growing store never floods the window.
//!
//! Bounds (optimization ①): newest-first, each entry truncated to a per-entry cap,
//! the whole block capped, and a trailing note when memories were dropped. This is
//! a pure function of the store's entries; injection (committed vs request-only)
//! is the caller's concern.

use serde::{Deserialize, Serialize};

use crate::localfs::MemoryDir;

/// How large a recalled memory block may get. Configured (e.g. from a plugin
/// config section) rather than hard-coded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallBounds {
    /// Truncate each memory to this many characters (0 = unbounded per entry).
    pub per_entry_chars: usize,
    /// Cap the whole block to this many characters.
    pub total_chars: usize,
    /// Include at most this many memories (newest first).
    pub max_entries: usize,
    /// Once the store holds more than this many memories, use relevance selection
    /// (a single model call) instead of injecting the newest ones wholesale.
    pub select_over: usize,
}

impl Default for RecallBounds {
    fn default() -> Self {
        Self {
            per_entry_chars: 1500,
            total_chars: 8000,
            max_entries: 40,
            select_over: 12,
        }
    }
}

fn truncate(text: &str, cap: usize) -> String {
    if cap == 0 || text.chars().count() <= cap {
        return text.to_string();
    }
    let kept: String = text.chars().take(cap).collect();
    format!("{kept}… [truncated]")
}

/// Build a bounded recall block from a store, or `None` when nothing is saved.
/// Newest memories are kept; older ones are dropped once a cap is hit, with a
/// trailing note recording how many were omitted.
pub fn recall_block(store: &MemoryDir, bounds: &RecallBounds) -> Option<String> {
    render(&store.entries(), bounds)
}

/// Relevance-aware recall (optimizations ① + ③): when the store is small, inject
/// the newest memories bounded; once it grows past `bounds.select_over`, pick the
/// relevant ones for `query` with a single model call, then bound-render those.
pub async fn recall_relevant(
    store: &MemoryDir,
    bounds: &RecallBounds,
    llm: &dyn awaken_runtime_contract::llm::LlmExecutor,
    model: &awaken_runtime_contract::resolved::ModelBinding,
    query: &str,
) -> Option<String> {
    let entries = store.entries();
    if entries.len() <= bounds.select_over {
        return render(&entries, bounds);
    }
    let picked =
        crate::select::select_relevant(llm, model, query, &entries, bounds.max_entries).await;
    if picked.is_empty() {
        return None;
    }
    let selected: Vec<crate::localfs::Entry> = picked
        .into_iter()
        .filter_map(|i| entries.get(i).cloned())
        .collect();
    render(&selected, bounds)
}

/// Render an already-ordered (newest-first) slice of entries into a bounded recall
/// block. Shared by whole-store recall and relevance-selected recall.
pub fn render(entries: &[crate::localfs::Entry], bounds: &RecallBounds) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let total = entries.len();
    let mut rendered: Vec<String> = Vec::new();
    let mut used = 0usize;
    let header =
        "Memories from earlier conversations (use them if relevant to the user's request):";
    let mut shown = 0usize;
    for entry in entries.iter().take(bounds.max_entries.max(1)) {
        let piece = truncate(&entry.content, bounds.per_entry_chars);
        // +2 for the joining blank line; stop before exceeding the total cap, but
        // always keep at least one memory so recall is never empty when non-empty.
        if !rendered.is_empty() && used + piece.len() + 2 > bounds.total_chars {
            break;
        }
        used += piece.len() + 2;
        rendered.push(piece);
        shown += 1;
    }
    let omitted = total - shown;
    let mut block = format!("{header}\n\n{}", rendered.join("\n\n"));
    if omitted > 0 {
        block.push_str(&format!(
            "\n\n(+{omitted} older memories not shown; ask if you need them.)"
        ));
    }
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn store_with(entries: &[(&str, &str)]) -> MemoryDir {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("awaken-recall-{stamp}"));
        let store = MemoryDir::new(&root);
        for (name, content) in entries {
            store.write(name, content).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        store
    }

    #[test]
    fn empty_store_recalls_nothing() {
        assert!(recall_block(&store_with(&[]), &RecallBounds::default()).is_none());
    }

    #[test]
    fn per_entry_truncation_applies() {
        let store = store_with(&[("a", "abcdefghij")]);
        let bounds = RecallBounds {
            per_entry_chars: 4,
            ..RecallBounds::default()
        };
        let block = recall_block(&store, &bounds).unwrap();
        assert!(block.contains("abcd… [truncated]"), "got: {block}");
        assert!(!block.contains("abcdefghij"));
    }

    #[test]
    fn max_entries_keeps_newest_and_notes_the_rest() {
        let store = store_with(&[("old", "OLD"), ("mid", "MID"), ("new", "NEW")]);
        let bounds = RecallBounds {
            max_entries: 1,
            ..RecallBounds::default()
        };
        let block = recall_block(&store, &bounds).unwrap();
        assert!(block.contains("NEW"), "newest kept: {block}");
        assert!(!block.contains("OLD"));
        assert!(
            block.contains("+2 older memories not shown"),
            "note: {block}"
        );
    }

    #[test]
    fn total_cap_drops_older_entries() {
        let store = store_with(&[("old", &"O".repeat(50)), ("new", &"N".repeat(50))]);
        let bounds = RecallBounds {
            total_chars: 60,
            per_entry_chars: 0,
            ..RecallBounds::default()
        };
        let block = recall_block(&store, &bounds).unwrap();
        // Newest fits; older is dropped by the total cap and noted.
        assert!(block.contains(&"N".repeat(50)));
        assert!(!block.contains(&"O".repeat(50)));
        assert!(block.contains("+1 older memories not shown"));
    }
}
