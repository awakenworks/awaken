---
title: "上报 Tool 进度"
description: "当你需要在工具执行过程中，把进度或活动快照实时流回前端时，使用本页。"
---

当你需要在工具执行过程中，把进度或活动快照实时流回前端时，使用本页。

## 前置条件

- 已有一个 `Tool` 实现，并能访问 `ToolCallContext`
- 已添加 `awaken`

## 步骤

1. 用 `report_progress` 上报结构化进度：

```rust
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult, ToolOutput};
use awaken::contract::progress::ProgressStatus;
use async_trait::async_trait;
use serde_json::{Value, json};

struct IndexTool;

#[async_trait]
impl Tool for IndexTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("index_docs", "Index Docs", "Index a document set")
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        ctx.report_progress(ProgressStatus::Running, Some("Starting indexing"), None).await;

        for i in 0..10 {
            let fraction = (i + 1) as f64 / 10.0;
            ctx.report_progress(
                ProgressStatus::Running,
                Some(&format!("Indexed batch {}/10", i + 1)),
                Some(fraction),
            ).await;
        }

        ctx.report_progress(ProgressStatus::Done, Some("Indexing complete"), Some(1.0)).await;
        Ok(ToolResult::success("index_docs", json!({"indexed": 10})).into())
    }
}
```

2. 用 `report_activity` 上报完整活动快照：

```rust
ctx.report_activity("code-generation", "fn hello() {\n    println!(\"hi\");\n}").await;
```

3. 用 `report_activity_delta` 上报增量补丁：

```rust
use serde_json::json;

ctx.report_activity_delta(
    "code-generation",
    json!([
        { "op": "add", "path": "/line", "value": "    println!(\"world\");" }
    ]),
).await;
```

4. 用 `ProgressStatus` 反映 tool call 生命周期：

| 变体 | 含义 |
|---|---|
| `Pending` | 已排队，尚未开始 |
| `Running` | 正在执行 |
| `Done` | 成功完成 |
| `Failed` | 执行失败 |
| `Cancelled` | 被取消 |

## 原理

`report_progress` 内部会构造 `ToolCallProgressState`，再由运行时包装成 `AgentEvent::ActivitySnapshot`：

```rust
pub struct ToolCallProgressState {
    pub schema: String,
    pub node_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub status: ProgressStatus,
    pub progress: Option<f64>,
    pub loaded: Option<u64>,
    pub total: Option<u64>,
    pub message: Option<String>,
    pub parent_node_id: Option<String>,
    pub parent_call_id: Option<String>,
}
```

活动类型常量是 `"tool-call-progress"`。

## 验证

订阅 SSE 事件流，确认在工具执行期间能收到 `ActivitySnapshot`，并且 `status` 从 `"running"` 变成 `"done"`。

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| 没有事件 | `activity_sink` 为空 | 确保运行时配置了事件 sink |
| 前端没有更新进度 | 前端未处理该活动类型 | 过滤 `activity_type == "tool-call-progress"` |

## 关键文件

- `crates/awaken-contract/src/contract/progress.rs`
- `crates/awaken-contract/src/contract/tool.rs`

## 相关

- [Tool Trait](/reference/tool-trait/)
- [事件](/reference/events/)
- [添加 Tool](/add-a-tool/)
