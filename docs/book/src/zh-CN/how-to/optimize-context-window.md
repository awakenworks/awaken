# 优化上下文窗口

当你需要控制运行时如何管理会话历史，以避免超过模型上下文上限时，使用本页。

## 前置条件

- 已添加 `awaken`
- 已有一个 `AgentSpec`

## ContextWindowPolicy

每个 agent 都可以配置 `ContextWindowPolicy`：

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

### 字段

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `max_context_tokens` | `usize` | `200_000` | 模型上下文总窗口（token 数） |
| `max_output_tokens` | `usize` | `16_384` | 为模型输出预留的 token |
| `min_recent_messages` | `usize` | `10` | 始终保留的最近消息数，即使超出预算 |
| `enable_prompt_cache` | `bool` | `true` | 是否启用 prompt cache |
| `autocompact_threshold` | `Option<usize>` | `None` | 达到该 token 数时触发自动压缩，`None` 表示禁用 |
| `compaction_mode` | `ContextCompactionMode` | `KeepRecentRawSuffix` | 自动压缩策略 |
| `compaction_raw_suffix_messages` | `usize` | `2` | 后缀压缩模式下保留的原始消息数 |

## 截断（Truncation）

当消息总量超过预算时，运行时会自动丢弃最旧消息。预算大致为：

```text
available = max_context_tokens - max_output_tokens - tool_schema_tokens
```

### 截断时会保留什么

- 所有 system messages
- 至少 `min_recent_messages` 条最近消息
- 成对的 tool call / tool result
- 不会留下悬挂的 tool calls

### Artifact 压缩

在真正截断前，过大的 tool result 会先被压缩成 preview，以减少 token 占用。system / user / assistant 普通消息不会做这种压缩。

## 压缩（Compaction）

压缩不会简单丢消息，而是把更早的历史总结成一条摘要消息。

### 启用自动压缩

```rust,ignore
let policy = ContextWindowPolicy {
    autocompact_threshold: Some(100_000),
    compaction_mode: ContextCompactionMode::KeepRecentRawSuffix,
    compaction_raw_suffix_messages: 4,
    ..Default::default()
};
```

### ContextCompactionMode

- `KeepRecentRawSuffix`：保留最近 N 条原始消息，其余压缩
- `CompactToSafeFrontier`：压缩到安全边界为止

安全边界的含义是：不会把某条 tool call 和它对应的结果拆开。

### CompactionConfig

压缩子系统通过 `CompactionConfig` 配置：

```rust,ignore
use awaken::CompactionConfig;

let config = CompactionConfig {
    summarizer_system_prompt: "You are a conversation summarizer. \
        Preserve all key facts, decisions, tool results, and action items. \
        Be concise but complete.".into(),
    summarizer_user_prompt: "Summarize the following conversation:\n\n{messages}".into(),
    summary_max_tokens: Some(1024),
    summary_model: Some("claude-3-haiku".into()),
    min_savings_ratio: 0.3,
};
```

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `summarizer_system_prompt` | `String` | 内置摘要 prompt | 摘要 LLM 的 system prompt |
| `summarizer_user_prompt` | `String` | `"Summarize...\n\n{messages}"` | 摘要用户 prompt 模板；`{messages}` 会被替换为对话记录 |
| `summary_max_tokens` | `Option<u32>` | `None` | 摘要响应的最大 token 数 |
| `summary_model` | `Option<String>` | `None` | 摘要所用模型，默认沿用 agent 模型 |
| `min_savings_ratio` | `f64` | `0.3` | 接受一次压缩所需的最低 token 节省比例（0.0-1.0） |

### DefaultSummarizer

内置 `DefaultSummarizer` 会读取 `CompactionConfig`，并支持“在已有摘要上继续增量总结”。

### 摘要存储

压缩结果会被保存成带 `<conversation-summary>` 标签的内部 system message。重新加载时，历史中较早的已摘要部分不会再被重新放回上下文窗口。

## 截断恢复

如果 LLM 因 `MaxTokens` 截断，而且已经产生部分文本或生成到一半的 tool call 参数不完整，运行时可以自动注入 continuation prompt 并重试，直到达到最大重试次数。

## 关键文件

- `crates/awaken-contract/src/contract/inference.rs`
- `crates/awaken-runtime/src/context/transform/mod.rs`
- `crates/awaken-runtime/src/context/transform/compaction.rs`
- `crates/awaken-runtime/src/context/compaction.rs`
- `crates/awaken-runtime/src/context/summarizer.rs`
- `crates/awaken-runtime/src/context/plugin.rs`
- `crates/awaken-runtime/src/context/truncation.rs`

## 相关

- [构建 Agent](./build-an-agent.md)
- [添加 Plugin](./add-a-plugin.md)
- [状态键](../reference/state-keys.md)
