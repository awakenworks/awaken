# Typed Tool

`TypedTool` auto-generates JSON Schema from a Rust argument struct and handles deserialization.

## Why Prefer `TypedTool`

- Generates JSON Schema automatically from the Rust argument type
- Deserializes JSON into a typed struct before your business logic runs
- Centralizes validation in Rust instead of manual `Value` parsing
- Keeps descriptor metadata and argument schema aligned

## Trait Shape

```rust,edition2021
# extern crate async_trait;
# extern crate schemars;
# extern crate serde;
# extern crate tirea;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tirea::contracts::runtime::tool_call::{ToolError, ToolResult};
use tirea::contracts::ToolCallContext;

#[async_trait]
pub trait ExampleTypedTool: Send + Sync {
    type Args: for<'de> Deserialize<'de> + JsonSchema + Send;

    fn tool_id(&self) -> &str;
    fn name(&self) -> &str;
    fn description(&self) -> &str;

    fn validate(&self, _args: &Self::Args) -> Result<(), String> {
        Ok(())
    }

    async fn execute(
        &self,
        args: Self::Args,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError>;
}
```

A blanket implementation converts every `TypedTool` into a normal `Tool`, so registration is unchanged.

## Minimal Example

```rust,edition2021
# extern crate async_trait;
# extern crate schemars;
# extern crate serde;
# extern crate serde_json;
# extern crate tirea;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tirea::contracts::runtime::tool_call::{ToolError, ToolResult, TypedTool};
use tirea::contracts::ToolCallContext;

#[derive(Debug, Deserialize, JsonSchema)]
struct SelectTripArgs {
    trip_id: String,
}

struct SelectTripTool;

#[async_trait]
impl TypedTool for SelectTripTool {
    type Args = SelectTripArgs;

    fn tool_id(&self) -> &str {
        "select_trip"
    }

    fn name(&self) -> &str {
        "Select Trip"
    }

    fn description(&self) -> &str {
        "Select a trip as the currently active trip"
    }

    async fn execute(
        &self,
        args: SelectTripArgs,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success(
            "select_trip",
            json!({ "selected": args.trip_id }),
        ))
    }
}

let tool = SelectTripTool;
assert_eq!(tool.tool_id(), "select_trip");
```

## Validation Flow

`TypedTool` validation happens in this order:

1. The runtime deserializes incoming JSON into `Args` with `serde_json::from_value`.
2. `validate(&Args)` runs for business rules that schema alone cannot express.
3. `execute(Args, ctx)` runs with a typed value.

That means:

- Missing required fields fail at deserialization time
- Type mismatches fail at deserialization time
- Cross-field or domain rules belong in `validate`

## State-Writing Example

This example shows a state-writing tool using the plain `Tool` trait with `execute_effect`. State mutations must go through `ToolExecutionEffect` + `AnyStateAction` — the runtime rejects direct writes via `ctx.state::<T>().set_*()`.

```rust,edition2021
# extern crate async_trait;
# extern crate serde;
# extern crate serde_json;
# extern crate tirea;
# extern crate tirea_state;
# extern crate tirea_state_derive;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tirea::contracts::{AnyStateAction, ToolCallContext};
use tirea::contracts::runtime::tool_call::{ToolExecutionEffect, ToolError, ToolResult};
use tirea::prelude::*;
use tirea_state_derive::State;

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "CounterAction")]
struct Counter {
    value: i64,
    label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum CounterAction {
    Increment(i64),
    Rename(String),
}

impl Counter {
    fn reduce(&mut self, action: CounterAction) {
        match action {
            CounterAction::Increment(n) => self.value += n,
            CounterAction::Rename(name) => self.label = name,
        }
    }
}

struct RenameCounter;

#[async_trait]
impl Tool for RenameCounter {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("rename_counter", "Rename Counter", "Update the counter label")
            .with_parameters(json!({
                "type": "object",
                "properties": { "label": { "type": "string" } },
                "required": ["label"]
            }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx).await?.result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let label = args["label"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("label is required".to_string()))?;

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "rename_counter",
            json!({ "label": label }),
        ))
        .with_action(AnyStateAction::new::<Counter>(
            CounterAction::Rename(label.to_string()),
        )))
    }
}

let tool = RenameCounter;
assert_eq!(tool.descriptor().id, "rename_counter");
```

## Reading State

Inside a tool, state reads follow one of two patterns:

Use `ctx.snapshot_of::<T>()` to read the current state as a deserialized Rust value:

```rust,edition2021
# extern crate serde;
# extern crate serde_json;
# extern crate tirea;
# extern crate tirea_state;
# extern crate tirea_state_derive;
use serde::{Deserialize, Serialize};
use tirea::contracts::ToolCallContext;
use tirea_state_derive::State;

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
struct WorkspaceFile {
    path: String,
}

fn read_file(ctx: &ToolCallContext<'_>) {
    let file: WorkspaceFile = ctx.snapshot_of::<WorkspaceFile>().unwrap_or_default();
    let _ = file.path;
}
```

For advanced cases where the same state type is reused at different paths, use `ctx.snapshot_at::<T>("some.path")`.

### Writing State

All state writes use the action-based pattern:

1. Read current state via `snapshot_of`
2. Return `ToolExecutionEffect::new(result).with_action(AnyStateAction::new::<T>(action))`
3. The runtime applies the action through the state's `reduce` method

> The runtime rejects direct state writes through `ctx.state::<T>().set_*()`. All mutations must go through the action pipeline.

## State Scope Examples

State scope is declared on the state type, not on the tool.

```rust,edition2021
# extern crate serde;
# extern crate serde_json;
# extern crate tirea_state;
# extern crate tirea_state_derive;
use serde::{Deserialize, Serialize};
use tirea_state::{StateScope, StateSpec};
use tirea_state_derive::State;

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "FileAccessAction", scope = "thread")]
struct FileAccessState {
    opened_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum FileAccessAction {
    Open(String),
}

impl FileAccessState {
    fn reduce(&mut self, action: FileAccessAction) {
        match action {
            FileAccessAction::Open(path) => self.opened_paths.push(path),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "EditorRunAction", scope = "run")]
struct EditorRunState {
    current_goal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum EditorRunAction {
    SetGoal(String),
}

impl EditorRunState {
    fn reduce(&mut self, action: EditorRunAction) {
        match action {
            EditorRunAction::SetGoal(goal) => self.current_goal = Some(goal),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "ApprovalAction", scope = "tool_call")]
struct ApprovalState {
    requested: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ApprovalAction {
    Request,
}

impl ApprovalState {
    fn reduce(&mut self, action: ApprovalAction) {
        match action {
            ApprovalAction::Request => self.requested = true,
        }
    }
}

assert_eq!(<FileAccessState as StateSpec>::SCOPE, StateScope::Thread);
assert_eq!(<EditorRunState as StateSpec>::SCOPE, StateScope::Run);
assert_eq!(<ApprovalState as StateSpec>::SCOPE, StateScope::ToolCall);
```

Use each scope for a different kind of data:

- `thread`: durable user-visible state, such as opened files, notes, trips, reports
- `run`: temporary execution state for one run, such as a plan, current objective, or runtime bookkeeping
- `tool_call`: per-invocation scratch state, especially for suspended calls, approvals, and resumable workflows

If you need the full cleanup semantics, see [Persistence and Versioning](../explanation/persistence-and-versioning.md) and [Derive Macro](./derive-macro.md).

## Hard Constraint Example: Must Read Before Write

Coding agents often need invariants such as "a file must be read before it can be edited". The simplest robust pattern is to persist read-tracking in thread-scoped state and reject writes that do not satisfy that precondition.

```rust,ignore
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "FileAccessAction", scope = "thread")]
struct FileAccessState {
    opened_paths: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadFileArgs {
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteFileArgs {
    path: String,
    content: String,
}

struct ReadFileTool;
struct WriteFileTool;

// ReadFileTool uses the plain Tool trait (not TypedTool) so it can implement
// execute_effect and emit state actions. Deserialize the typed args manually.
#[async_trait]
impl Tool for ReadFileTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("read_file", "Read File", "Read a file and record access")
            .with_parameters(json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext<'_>) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx).await?.result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let typed_args: ReadFileArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let content = std::fs::read_to_string(&typed_args.path)
            .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;

        let access: FileAccessState = ctx.snapshot_of::<FileAccessState>().unwrap_or_default();
        let mut effect = ToolExecutionEffect::new(ToolResult::success(
            "read_file",
            json!({ "path": typed_args.path, "content": content }),
        ));

        if !access.opened_paths.contains(&typed_args.path) {
            effect = effect.with_action(AnyStateAction::new::<FileAccessState>(
                FileAccessAction::MarkOpened(typed_args.path),
            ));
        }

        Ok(effect)
    }
}

#[async_trait]
impl TypedTool for WriteFileTool {
    type Args = WriteFileArgs;

    fn tool_id(&self) -> &str { "write_file" }
    fn name(&self) -> &str { "Write File" }
    fn description(&self) -> &str { "Write a file only after it was read" }

    fn validate(&self, args: &Self::Args) -> Result<(), String> {
        if args.path.trim().is_empty() {
            return Err("path cannot be empty".to_string());
        }
        Ok(())
    }

    async fn execute(
        &self,
        args: WriteFileArgs,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let access: FileAccessState = ctx.snapshot_of::<FileAccessState>().unwrap_or_default();
        if !access.opened_paths.contains(&args.path) {
            return Err(ToolError::Denied(format!(
                "write_file requires a prior read_file for {}",
                args.path
            )));
        }

        std::fs::write(&args.path, &args.content)
            .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;

        Ok(ToolResult::success(
            "write_file",
            json!({ "path": args.path, "written": true }),
        ))
    }
}
```

This pattern is usually enough when:

- the invariant is domain-specific
- the check depends on state accumulated by other tools
- you want the rule to survive across multiple runs in the same thread

If the same policy must apply to many tools uniformly, move the gate into a plugin or `BeforeToolExecute` policy instead of duplicating the check in each tool.

## `ToolExecutionEffect`

`ToolExecutionEffect` lets a tool return:

- a `ToolResult`
- state actions
- non-state actions applied in `AfterToolExecute`

## Example: State Action + Message Injection

```rust,ignore
use tirea::contracts::runtime::inference::ContextMessage;
use tirea::contracts::runtime::phase::AfterToolExecuteAction;
use tirea::contracts::runtime::state::AnyStateAction;
use tirea::contracts::runtime::tool_call::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolExecutionEffect, ToolResult,
};

struct ActivateSkillTool;

#[async_trait]
impl Tool for ActivateSkillTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("activate_skill", "Activate Skill", "Activate a skill")
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("activate_skill", serde_json::json!({ "ok": true })))
    }

    async fn execute_effect(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        Ok(
            ToolExecutionEffect::new(ToolResult::success(
                "activate_skill",
                serde_json::json!({ "ok": true }),
            ))
            // SkillState and SkillStateAction come from tirea-extension-skills
            .with_action(AnyStateAction::new::<SkillState>(
                SkillStateAction::Activate("docx".to_string()),
            ))
            .with_action(AfterToolExecuteAction::AddMessage(
                ContextMessage::conversation_user("Skill instructions..."),
            )),
        )
    }
}
```

The real skill implementation in this repository goes further and also applies permission-domain actions:

- [`crates/tirea-extension-skills/src/tools.rs`](/home/chaizhenhua/Codes/uncarve/crates/tirea-extension-skills/src/tools.rs)
- [`crates/tirea-extension-permission/src/state.rs`](/home/chaizhenhua/Codes/uncarve/crates/tirea-extension-permission/src/state.rs)

Prefer these built-in `AfterToolExecuteAction` variants for common post-tool side effects before introducing a custom `Action` type.

## Temporary Permission Changes

One common pattern is: a tool activates a capability, then temporarily widens what the agent is allowed to call.

The important design distinction is:

- the tool may emit permission-domain state actions as part of its effect
- the plugin remains responsible for enforcing those permissions at execution time

That separation keeps policy enforcement centralized while still allowing domain tools to request policy-relevant state changes.

## Registration

Register `TypedTool` exactly like any other tool:

```rust,ignore
use tirea::composition::{tool_map, AgentOsBuilder};

let os = AgentOsBuilder::new()
    .with_tools(tool_map([SelectTripTool]))
    .build()?;
```

## Complete Example In This Repository

- `examples/src/travel/tools.rs` contains `SelectTripTool`, a real `TypedTool` implementation.
- `crates/tirea-contract/src/runtime/tool_call/tool.rs` contains the authoritative trait definition.

## Common Mistakes

- Deriving `Deserialize` but forgetting `JsonSchema` on `Args`
- Putting business validation into `execute` instead of `validate`
- Falling back to untyped `Value` parsing even though the input is fixed
- Assuming `validate_args` still uses JSON Schema at runtime for `TypedTool`

For `TypedTool`, the runtime skips `Tool::validate_args` and relies on deserialization plus `validate(&Args)`.

## Related

- [First Tool](../tutorials/first-tool.md)
- [Add a Tool](../how-to/add-a-tool.md)
- [Capability Matrix](./capability-matrix.md)
