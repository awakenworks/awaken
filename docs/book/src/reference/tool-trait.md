# Tool Trait

The `Tool` trait is the primary extension point for giving agents capabilities.
Tools are stateless functions that receive JSON arguments and a read-only context,
and return a `ToolResult`.

## Trait definition

```rust,ignore
#[async_trait]
pub trait Tool: Send + Sync {
    /// Return the descriptor for this tool.
    fn descriptor(&self) -> ToolDescriptor;

    /// Validate arguments before execution. Default: accept all.
    fn validate_args(&self, _args: &Value) -> Result<(), ToolError> {
        Ok(())
    }

    /// Execute the tool with the given arguments and context.
    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> Result<ToolResult, ToolError>;
}
```

**Crate path:** `awaken::contract::tool::Tool`

## ToolDescriptor

Describes a tool's identity and parameter schema. Registered with the runtime
and sent to the LLM as available functions.

```rust,ignore
pub struct ToolDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    /// JSON Schema for parameters.
    pub parameters: Value,
    pub category: Option<String>,
}
```

Builder methods:

```rust,ignore
ToolDescriptor::new(id, name, description) -> Self
    .with_parameters(schema: Value) -> Self
    .with_category(category: impl Into<String>) -> Self
```

## ToolResult

Returned by `Tool::execute`. Carries the execution outcome back to the agent loop.

```rust,ignore
pub struct ToolResult {
    pub tool_name: String,
    pub status: ToolStatus,
    pub data: Value,
    pub message: Option<String>,
    pub suspension: Option<Box<SuspendTicket>>,
}
```

### ToolStatus

```rust,ignore
pub enum ToolStatus {
    Success,  // Execution succeeded
    Pending,  // Suspended, waiting for external resume
    Error,    // Execution failed; content sent back to LLM
}
```

### Constructors

| Method | Status | Use case |
|---|---|---|
| `ToolResult::success(name, data)` | `Success` | Normal completion |
| `ToolResult::success_with_message(name, data, msg)` | `Success` | Completion with description |
| `ToolResult::error(name, message)` | `Error` | Recoverable failure |
| `ToolResult::error_with_code(name, code, message)` | `Error` | Structured error with code |
| `ToolResult::suspended(name, message)` | `Pending` | HITL suspension |
| `ToolResult::suspended_with(name, message, ticket)` | `Pending` | Suspension with ticket |

### Predicates

- `is_success() -> bool`
- `is_pending() -> bool`
- `is_error() -> bool`
- `to_json() -> Value`

## ToolError

Errors returned from `validate_args` or `execute`. Unlike `ToolResult::Error`
(which is sent to the LLM), a `ToolError` aborts the tool call.

```rust,ignore
pub enum ToolError {
    InvalidArguments(String),
    ExecutionFailed(String),
    Denied(String),
    NotFound(String),
    Internal(String),
}
```

## ToolCallContext

Read-only context provided to a tool during execution.

```rust,ignore
pub struct ToolCallContext {
    pub call_id: String,
    pub tool_name: String,
    pub run_identity: RunIdentity,
    pub agent_spec: Arc<AgentSpec>,
    pub snapshot: Snapshot,
    pub activity_sink: Option<Arc<dyn EventSink>>,
}
```

### Methods

```rust,ignore
/// Read a typed state key from the snapshot.
fn state<K: StateKey>(&self) -> Option<&K::Value>

/// Report an activity snapshot for this tool call.
async fn report_activity(&self, activity_type: &str, content: &str)

/// Report an incremental activity delta.
async fn report_activity_delta(&self, activity_type: &str, patch: Value)

/// Report structured tool call progress.
async fn report_progress(
    &self,
    status: ProgressStatus,
    message: Option<&str>,
    progress: Option<f64>,
)
```

## Examples

### Minimal tool

```rust,ignore
use async_trait::async_trait;
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult};
use serde_json::{Value, json};

struct Greet;

#[async_trait]
impl Tool for Greet {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("greet", "greet", "Greet a user by name")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext,
    ) -> Result<ToolResult, ToolError> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("name required".into()))?;
        Ok(ToolResult::success("greet", json!({ "greeting": format!("Hello, {name}!") })))
    }
}
```

### Reading state from context

```rust,ignore
use async_trait::async_trait;
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult};
use awaken::state::StateKey;
use serde::{Serialize, Deserialize};
use serde_json::{Value, json};

// Assume a state key is defined elsewhere:
// struct UserPreferences;
// impl StateKey for UserPreferences { ... }

struct GetPreferences;

#[async_trait]
impl Tool for GetPreferences {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("get_prefs", "get_preferences", "Get user preferences")
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolCallContext,
    ) -> Result<ToolResult, ToolError> {
        // Read typed state through the snapshot
        // let prefs = ctx.state::<UserPreferences>().cloned().unwrap_or_default();
        // Ok(ToolResult::success("get_prefs", serde_json::to_value(&prefs).unwrap()))
        Ok(ToolResult::success("get_prefs", json!({})))
    }
}
```

## Related

- [First Tool Tutorial](../tutorials/first-tool.md)
- [Add a Tool](../how-to/add-a-tool.md)
