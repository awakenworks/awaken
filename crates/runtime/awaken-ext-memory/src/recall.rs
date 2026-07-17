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
        // Measure the block budget in CHARS, matching `truncate`'s per-entry cap and
        // the `total_chars` name/docs — not UTF-8 bytes, which over-count multibyte
        // text. +2 for the joining blank line; stop before exceeding the total cap,
        // but always keep at least one memory so recall is never empty when non-empty.
        let piece_chars = piece.chars().count();
        if !rendered.is_empty() && used + piece_chars + 2 > bounds.total_chars {
            break;
        }
        used += piece_chars + 2;
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

    #[test]
    fn per_entry_cap_is_exact_at_the_boundary() {
        // Content length == cap: kept verbatim, no truncation marker (off-by-one).
        let store = store_with(&[("a", "abcd")]);
        let bounds = RecallBounds {
            per_entry_chars: 4,
            ..RecallBounds::default()
        };
        let block = recall_block(&store, &bounds).unwrap();
        assert!(block.contains("abcd"), "got: {block}");
        assert!(
            !block.contains("[truncated]"),
            "content exactly at the cap must not be truncated: {block}"
        );
    }

    #[test]
    fn max_entries_zero_still_shows_one() {
        // `max_entries.max(1)` guarantees at least one memory even at zero.
        let store = store_with(&[("old", "OLD"), ("new", "NEW")]);
        let bounds = RecallBounds {
            max_entries: 0,
            ..RecallBounds::default()
        };
        let block = recall_block(&store, &bounds).unwrap();
        assert!(block.contains("NEW"), "newest is the one kept: {block}");
        assert!(!block.contains("OLD"));
        assert!(block.contains("+1 older memories not shown"));
    }

    #[test]
    fn single_entry_over_total_cap_is_still_shown_without_an_omit_note() {
        // The first memory is always kept even when it alone exceeds total_chars,
        // and nothing was omitted so there is no trailing note.
        let store = store_with(&[("big", &"Z".repeat(100))]);
        let bounds = RecallBounds {
            total_chars: 10,
            per_entry_chars: 0,
            ..RecallBounds::default()
        };
        let block = recall_block(&store, &bounds).unwrap();
        assert!(block.contains(&"Z".repeat(100)), "first entry always kept");
        assert!(!block.contains("older memories not shown"));
    }

    // --- recall_relevant: the select_over threshold and empty-selection path ---

    use async_trait::async_trait;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
    };
    use awaken_runtime_contract::resolved::ModelBinding;

    struct PanicModel;
    #[async_trait]
    impl LlmExecutor for PanicModel {
        async fn infer(&self, _r: ChatRequest) -> LlmResult<ChatResponse> {
            panic!("the model must not be called at or below select_over");
        }
    }
    struct ReplyModel(&'static str);
    #[async_trait]
    impl LlmExecutor for ReplyModel {
        async fn infer(&self, _r: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text(self.0),
                usage: None,
                stop_reason: None,
            })
        }
    }
    fn model() -> ModelBinding {
        ModelBinding::new("p", "m", "b")
    }

    #[tokio::test]
    async fn recall_relevant_at_or_below_select_over_skips_the_model() {
        // 2 entries, select_over == 2 → entries.len() <= select_over → render, no call.
        let store = store_with(&[("a", "AAA"), ("b", "BBB")]);
        let bounds = RecallBounds {
            select_over: 2,
            ..RecallBounds::default()
        };
        let block = recall_relevant(&store, &bounds, &PanicModel, &model(), "q")
            .await
            .unwrap();
        assert!(block.contains("AAA") && block.contains("BBB"));
    }

    #[tokio::test]
    async fn recall_relevant_above_select_over_selects_via_the_model() {
        // 3 entries > select_over(1); max_entries(2) < 3 forces a real model call.
        let store = store_with(&[("a", "AAA"), ("b", "BBB"), ("c", "CCC")]);
        let bounds = RecallBounds {
            select_over: 1,
            max_entries: 2,
            ..RecallBounds::default()
        };
        // Entries are newest-first (c,b,a); index 0 is "CCC".
        let block = recall_relevant(&store, &bounds, &ReplyModel("[0]"), &model(), "q")
            .await
            .unwrap();
        assert!(block.contains("CCC"), "picked memory shown: {block}");
        assert!(!block.contains("AAA"), "unpicked memory absent: {block}");
    }

    #[tokio::test]
    async fn recall_relevant_returns_none_when_selection_picks_nothing() {
        let store = store_with(&[("a", "AAA"), ("b", "BBB"), ("c", "CCC")]);
        let bounds = RecallBounds {
            select_over: 1,
            max_entries: 2,
            ..RecallBounds::default()
        };
        // Model replies NONE → no indices parsed → recall_relevant yields None.
        let out = recall_relevant(&store, &bounds, &ReplyModel("NONE"), &model(), "q").await;
        assert!(out.is_none());
    }

    // --- char cap: `render` measures the total cap in CHARS, matching the
    //     `total_chars` name/docs and `truncate`'s per-entry char cap. ---

    /// Build an in-memory entry directly (no filesystem, no mtime) so ordering is
    /// deterministic and the exact content bytes are under test control.
    fn entry(content: &str) -> crate::localfs::Entry {
        crate::localfs::Entry {
            path: std::path::PathBuf::from("x.md"),
            content: content.to_string(),
            modified: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn total_cap_counts_chars_not_bytes_for_multibyte_content() {
        // `RecallBounds::total_chars` is a *character* budget, so `render` measures
        // the block in chars (like `truncate`'s per-entry cap), NOT UTF-8 bytes.
        //
        // Two entries of 10 chars each. "あ" is 3 bytes → 30 bytes / 10 chars per entry.
        // total_chars = 25.
        //   * The char budget (correct): 10 + 2 + 10 = 22 chars <= 25 → both kept.
        //   * A byte budget (the old bug): newest uses 32 bytes; the older would push
        //     past 25 → wrongly dropped.
        let entries = [entry(&"あ".repeat(10)), entry(&"い".repeat(10))];
        let bounds = RecallBounds {
            per_entry_chars: 0,
            total_chars: 25,
            ..RecallBounds::default()
        };
        let block = render(&entries, &bounds).unwrap();
        // Both fit the 25-*character* budget, so both are kept and nothing is omitted.
        assert!(block.contains(&"あ".repeat(10)), "newest kept: {block}");
        assert!(
            block.contains(&"い".repeat(10)),
            "older kept under the char-measured cap: {block}"
        );
        assert!(
            !block.contains("older memories not shown"),
            "20 chars + join fit a 25-char budget, so nothing is omitted: {block}"
        );
    }

    #[test]
    fn ascii_control_keeps_both_at_the_same_char_counts() {
        // The ASCII twin of the case above: identical 10+10 char counts, but here
        // bytes == chars, so 20 <= 25 and BOTH entries are kept. The only difference
        // from the multibyte case is byte width — isolating the byte-vs-char defect.
        let entries = [entry(&"a".repeat(10)), entry(&"b".repeat(10))];
        let bounds = RecallBounds {
            per_entry_chars: 0,
            total_chars: 25,
            ..RecallBounds::default()
        };
        let block = render(&entries, &bounds).unwrap();
        assert!(block.contains(&"a".repeat(10)), "newest kept: {block}");
        assert!(
            block.contains(&"b".repeat(10)),
            "older kept under char==byte: {block}"
        );
        assert!(
            !block.contains("older memories not shown"),
            "nothing omitted: {block}"
        );
    }
}
