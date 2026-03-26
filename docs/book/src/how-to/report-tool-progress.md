# Report Tool Progress

Use this when you need to stream progress updates or activity snapshots from a tool back to the frontend during execution.

## Prerequisites

- A `Tool` implementation with access to `ToolCallContext`
- `awaken` crate added to `Cargo.toml`

## Steps

1. Report structured progress with `report_progress`.

   Call `report_progress` inside your `Tool::execute` method to emit a
   `ToolCallProgressState` snapshot. The runtime wraps it in an
   `AgentEvent::ActivitySnapshot` with `activity_type = "tool-call-progress"`.

```rust,ignore
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult};
use awaken::contract::progress::ProgressStatus;
use async_trait::async_trait;
use serde_json::{Value, json};

struct IndexTool;

#[async_trait]
impl Tool for IndexTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("index_docs", "Index Docs", "Index a document set")
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        ctx.report_progress(ProgressStatus::Running, Some("Starting indexing"), None).await;

        for i in 0..10 {
            // ... do work ...
            let fraction = (i + 1) as f64 / 10.0;
            ctx.report_progress(
                ProgressStatus::Running,
                Some(&format!("Indexed batch {}/10", i + 1)),
                Some(fraction),
            ).await;
        }

        ctx.report_progress(ProgressStatus::Done, Some("Indexing complete"), Some(1.0)).await;
        Ok(ToolResult::success("index_docs", json!({"indexed": 10})))
    }
}
```

   The `progress` parameter is a normalized `f64` between `0.0` and `1.0`.
   Pass `None` when progress is indeterminate.

2. Report custom activity snapshots with `report_activity`.

   Use `report_activity` for full-state activity updates that are not
   structured progress. The runtime emits an `AgentEvent::ActivitySnapshot`
   with `replace: Some(true)`.

```rust,ignore
ctx.report_activity("code-generation", "fn hello() {\n    println!(\"hi\");\n}").await;
```

   The `activity_type` string identifies the kind of activity. Frontends use
   it to choose how to render the content.

3. Report incremental activity deltas with `report_activity_delta`.

   Use `report_activity_delta` to send a JSON patch instead of replacing the
   full content. The runtime emits an `AgentEvent::ActivityDelta`. If you pass
   a single JSON value, it is wrapped in an array automatically.

```rust,ignore
use serde_json::json;

ctx.report_activity_delta(
    "code-generation",
    json!([
        { "op": "add", "path": "/line", "value": "    println!(\"world\");" }
    ]),
).await;
```

4. Use `ProgressStatus` variants to reflect the tool call lifecycle.

| Variant | Meaning |
|---|---|
| `Pending` | Tool call is queued but has not started |
| `Running` | Tool call is actively executing |
| `Done` | Tool call completed successfully |
| `Failed` | Tool call encountered an error |
| `Cancelled` | Tool call was cancelled before completion |

   `ProgressStatus` serializes to snake_case strings (`"pending"`, `"running"`,
   `"done"`, `"failed"`, `"cancelled"`).

## How it works

`report_progress` builds a `ToolCallProgressState` and emits it as an
`AgentEvent::ActivitySnapshot`:

```rust,ignore
pub struct ToolCallProgressState {
    pub schema: String,           // "tool-call-progress.v1"
    pub node_id: String,          // set to call_id
    pub call_id: String,
    pub tool_name: String,
    pub status: ProgressStatus,
    pub progress: Option<f64>,    // 0.0 - 1.0, None if indeterminate
    pub loaded: Option<u64>,      // absolute loaded count
    pub total: Option<u64>,       // absolute total count
    pub message: Option<String>,
    pub parent_node_id: Option<String>,
    pub parent_call_id: Option<String>,
}
```

The activity type is the constant `TOOL_CALL_PROGRESS_ACTIVITY_TYPE`, which
equals `"tool-call-progress"`. Optional fields (`progress`, `loaded`, `total`,
`message`, `parent_node_id`, `parent_call_id`) are omitted from JSON when `None`.

All three reporting methods are no-ops when the context has no `activity_sink`
configured.

## Verify

Subscribe to the SSE event stream and confirm that `ActivitySnapshot` events
with `activity_type = "tool-call-progress"` appear while the tool executes,
and that the `status` field transitions from `"running"` to `"done"`.

## Common Errors

| Error | Cause | Fix |
|---|---|---|
| No events appear | `activity_sink` is `None` on the context | Ensure the runtime is configured with an event sink |
| Progress not updating in frontend | Frontend does not handle `ActivitySnapshot` with this activity type | Filter on `activity_type == "tool-call-progress"` |

## Key Files

- `crates/awaken-contract/src/contract/progress.rs` -- `ToolCallProgressState`, `ProgressStatus`, `TOOL_CALL_PROGRESS_ACTIVITY_TYPE`
- `crates/awaken-contract/src/contract/tool.rs` -- `ToolCallContext::report_progress`, `report_activity`, `report_activity_delta`

## Related

- [Tool Trait Reference](../reference/tool-trait.md)
- [Events Reference](../reference/events.md)
- [Add a Tool](./add-a-tool.md)
