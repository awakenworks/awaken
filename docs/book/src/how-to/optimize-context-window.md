# Optimize the Context Window

Use this when you need to control how the runtime manages conversation history to stay within model token limits.

## Prerequisites

- `awaken` crate added to `Cargo.toml`
- An agent configured with `AgentSpec`

## ContextWindowPolicy

Every agent has a `ContextWindowPolicy` that controls how conversation history is managed. Set it on your `AgentSpec`:

```rust,ignore
use awaken::ContextWindowPolicy;

let policy = ContextWindowPolicy {
    max_context_tokens: 200_000,
    max_output_tokens: 16_384,
    min_recent_messages: 10,
    enable_prompt_cache: true,
    autocompact_threshold: Some(100_000),
    compaction_mode: ContextCompactionMode::KeepRecentRawSuffix,
    compaction_raw_suffix_messages: 2,
};
```

### Fields

| Field | Type | Default | Description |
|---|---|---|---|
| `max_context_tokens` | `usize` | `200_000` | Model's total context window size in tokens |
| `max_output_tokens` | `usize` | `16_384` | Tokens reserved for model output |
| `min_recent_messages` | `usize` | `10` | Minimum number of recent messages to always preserve, even if over budget |
| `enable_prompt_cache` | `bool` | `true` | Whether to enable prompt caching |
| `autocompact_threshold` | `Option<usize>` | `None` | Token count that triggers auto-compaction. `None` disables auto-compaction |
| `compaction_mode` | `ContextCompactionMode` | `KeepRecentRawSuffix` | Strategy used when auto-compaction fires |
| `compaction_raw_suffix_messages` | `usize` | `2` | Number of recent raw messages to preserve in suffix compaction mode |

## Truncation

When the conversation exceeds the available token budget, the runtime automatically drops the oldest messages to fit. The budget is calculated as:

```text
available = max_context_tokens - max_output_tokens - tool_schema_tokens
```

### What truncation preserves

- **System messages** are never truncated. All system messages at the start of the history survive regardless of budget.
- **Recent messages** -- at least `min_recent_messages` history messages are kept, even if they exceed the budget.
- **Tool call/result pairs** -- the split point is adjusted so that an assistant message with tool calls is never separated from its corresponding tool result messages.
- **Dangling tool calls** -- after truncation, any orphaned tool calls (whose results were dropped) are patched to prevent invalid message sequences.

### Artifact compaction

Before truncation runs, oversized tool results are compacted automatically. A tool result whose text exceeds `ARTIFACT_COMPACT_THRESHOLD_TOKENS` (2048 tokens, estimated at ~8192 characters) is truncated to a preview of at most 1600 characters or 24 lines, whichever is shorter. The preview includes a compaction indicator showing the original size.

Non-tool messages (system, user, assistant) are never subject to artifact compaction.

## Compaction

Compaction summarizes older conversation history into a condensed summary message, reducing token usage while preserving context. Unlike truncation (which drops messages), compaction replaces them with a summary.

### Enabling auto-compaction

Set `autocompact_threshold` to trigger compaction when total message tokens exceed that value:

```rust,ignore
let policy = ContextWindowPolicy {
    autocompact_threshold: Some(100_000),
    compaction_mode: ContextCompactionMode::KeepRecentRawSuffix,
    compaction_raw_suffix_messages: 4,
    ..Default::default()
};
```

### ContextCompactionMode

Two strategies are available:

- **`KeepRecentRawSuffix`** (default) -- keeps the most recent `compaction_raw_suffix_messages` messages as raw history. Everything before the compaction boundary is summarized.
- **`CompactToSafeFrontier`** -- compacts all messages up to the safe frontier (the latest point where all tool call/result pairs are complete).

The compaction boundary is chosen so that no tool call is separated from its result. The boundary finder walks the message history and only places boundaries where all open tool calls have been resolved.

### CompactionConfig

The compaction subsystem is configured through `CompactionConfig`, stored in the agent spec's `sections["compaction"]` and read via `CompactionConfigKey`:

```rust,ignore
use awaken::CompactionConfig;

let config = CompactionConfig {
    summarizer_system_prompt: "You are a conversation summarizer. \
        Preserve all key facts, decisions, tool results, and action items. \
        Be concise but complete.".into(),
    summarizer_user_prompt: "Summarize the following conversation:\n\n{messages}".into(),
    summary_max_tokens: Some(1024),
    summary_upstream_model: Some("claude-3-haiku".into()),
    min_savings_ratio: 0.3,
};
```

| Field | Type | Default | Description |
|---|---|---|---|
| `summarizer_system_prompt` | `String` | Conversation summarizer prompt | System prompt for the summarizer LLM call |
| `summarizer_user_prompt` | `String` | `"Summarize...\n\n{messages}"` | User prompt template; `{messages}` is replaced with the conversation transcript |
| `summary_max_tokens` | `Option<u32>` | `None` | Maximum tokens for the summary response |
| `summary_model` | `Option<String>` | `None` | Model for summarization (defaults to the agent's model) |
| `min_savings_ratio` | `f64` | `0.3` | Minimum token savings ratio (0.0-1.0) to accept a compaction |

The compaction pass only runs when the expected savings ratio exceeds `min_savings_ratio`. A minimum gain of 1024 tokens (`MIN_COMPACTION_GAIN_TOKENS`) is also required to justify the summarization LLM call.

### DefaultSummarizer

The built-in `DefaultSummarizer` reads prompts from `CompactionConfig` and supports cumulative summarization. When a previous summary exists, it asks the LLM to update the existing summary with new conversation rather than re-summarizing everything from scratch.

The transcript renderer filters out `Visibility::Internal` messages before sending to the summarizer, since system-injected context is re-injected each turn and should not be included in summaries.

### Summary storage

Compaction summaries are stored as `<conversation-summary>` tagged internal system messages. On load, `trim_to_compaction_boundary` drops all messages before the latest summary message, so already-summarized history is never re-loaded into the context window.

Compaction boundaries are tracked durably via `CompactionState`, recording the summary text, pre/post token counts, and timestamp for each compaction event.

## Truncation recovery

When the LLM stops due to `MaxTokens` after producing partial text or incomplete tool calls (argument JSON was truncated mid-generation), the runtime can automatically retry by injecting a continuation prompt asking the model to break its work into smaller pieces and continue. The retry count is tracked by `TruncationState` and bounded by a configurable maximum.

## Key Files

- `crates/awaken-contract/src/contract/inference.rs` -- `ContextWindowPolicy`, `ContextCompactionMode`
- `crates/awaken-runtime/src/context/transform/mod.rs` -- `ContextTransform` (truncation)
- `crates/awaken-runtime/src/context/transform/compaction.rs` -- artifact compaction
- `crates/awaken-runtime/src/context/compaction.rs` -- boundary finding, load-time trimming
- `crates/awaken-runtime/src/context/summarizer.rs` -- `ContextSummarizer`, `DefaultSummarizer`
- `crates/awaken-runtime/src/context/plugin.rs` -- `CompactionPlugin`, `CompactionConfig`, `CompactionState`
- `crates/awaken-runtime/src/context/truncation.rs` -- `TruncationState`, continuation prompts

## Related

- [Build an Agent](./build-an-agent.md)
- [Add a Plugin](./add-a-plugin.md)
- [State Keys](../reference/state-keys.md)
