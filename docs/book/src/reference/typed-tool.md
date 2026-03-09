# Typed Tool

`TypedTool` is the preferred way to implement most production tools with a fixed argument shape.

Use plain `Tool` when you need fully custom JSON handling, dynamic schemas, or custom `execute_effect` behavior. Use `TypedTool` when arguments can be modeled as one Rust struct.

## Why Prefer `TypedTool`

- Generates JSON Schema automatically from the Rust argument type
- Deserializes JSON into a typed struct before your business logic runs
- Centralizes validation in Rust instead of manual `Value` parsing
- Keeps descriptor metadata and argument schema aligned

## Trait Shape

```rust,ignore
#[async_trait]
pub trait TypedTool: Send + Sync {
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

```rust,ignore
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

Assume `Counter` is the same state type introduced in [First Tool](../tutorials/first-tool.md).

```rust,ignore
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tirea::contracts::runtime::tool_call::{ToolError, ToolResult, TypedTool};
use tirea::contracts::ToolCallContext;

#[derive(Debug, Deserialize, JsonSchema)]
struct RenameArgs {
    label: String,
}

struct RenameCounter;

#[async_trait]
impl TypedTool for RenameCounter {
    type Args = RenameArgs;

    fn tool_id(&self) -> &str { "rename_counter" }
    fn name(&self) -> &str { "Rename Counter" }
    fn description(&self) -> &str { "Update the counter label" }

    fn validate(&self, args: &Self::Args) -> Result<(), String> {
        if args.label.trim().is_empty() {
            return Err("label cannot be empty".to_string());
        }
        Ok(())
    }

    async fn execute(
        &self,
        args: RenameArgs,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let state = ctx.state::<Counter>("counter");
        state
            .set_label(args.label.clone())
            .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;

        Ok(ToolResult::success(
            "rename_counter",
            json!({ "label": args.label }),
        ))
    }
}
```

## Reading And Writing State

Inside a tool, state access usually follows one of these patterns:

- `ctx.state_of::<T>()` when the state type already declares its canonical path with `#[tirea(path = "...")]`
- `ctx.state::<T>("some.path")` when you want to read or write the same state shape at an explicit path

Example:

```rust,ignore
#[derive(Debug, Clone, Serialize, Deserialize, State)]
#[tirea(path = "workspace_file")]
struct WorkspaceFile {
    path: String,
    content: String,
}

let file = ctx.state_of::<WorkspaceFile>();
let current = file.content().unwrap_or_default();
file.set_content(format!("{current}\n// updated"))?;

let draft = ctx.state::<WorkspaceFile>("drafts.active_file");
let draft_path = draft.path().unwrap_or_default();
```

A practical rule:

- keep durable business data in typed state;
- read the current value first;
- derive the next value in Rust;
- write through generated setters so the runtime records a patch.

## State Scope Examples

State scope is declared on the state type, not on the tool.

```rust,ignore
#[derive(Debug, Clone, Serialize, Deserialize, State)]
#[tirea(path = "files", action = "FileAccessAction", scope = "thread")]
struct FileAccessState {
    opened_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, State)]
#[tirea(path = "__run.editor", action = "EditorRunAction", scope = "run")]
struct EditorRunState {
    current_goal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, State)]
#[tirea(path = "approval", action = "ApprovalAction", scope = "tool_call")]
struct ApprovalState {
    requested: bool,
}
```

Use each scope for a different kind of data:

- `thread`: durable user-visible state, such as opened files, notes, trips, reports
- `run`: temporary execution state for one run, such as a plan, current objective, or runtime bookkeeping
- `tool_call`: per-invocation scratch state, especially for suspended calls, approvals, and resumable workflows

If you need the full cleanup semantics, see [Persistence and Versioning](../explanation/persistence-and-versioning.md) and [Derive Macro](./derive-macro.md).

## Hard Constraint Example: Must Read Before Write

Coding agents often need invariants such as "a file must be read before it can be edited". The simplest robust pattern is to persist read-tracking in thread-scoped state and reject writes that do not satisfy that precondition.

```rust,ignore
#[derive(Debug, Clone, Serialize, Deserialize, State)]
#[tirea(path = "file_access", action = "FileAccessAction", scope = "thread")]
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

#[async_trait]
impl TypedTool for ReadFileTool {
    type Args = ReadFileArgs;

    fn tool_id(&self) -> &str { "read_file" }
    fn name(&self) -> &str { "Read File" }
    fn description(&self) -> &str { "Read a file and record access" }

    async fn execute(
        &self,
        args: ReadFileArgs,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let content = std::fs::read_to_string(&args.path)
            .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;

        let access = ctx.state_of::<FileAccessState>();
        let mut opened = access.opened_paths().unwrap_or_default();
        if !opened.contains(&args.path) {
            opened.push(args.path.clone());
            access
                .set_opened_paths(opened)
                .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
        }

        Ok(ToolResult::success(
            "read_file",
            json!({ "path": args.path, "content": content }),
        ))
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
        let access = ctx.state_of::<FileAccessState>();
        let opened = access.opened_paths().unwrap_or_default();
        if !opened.contains(&args.path) {
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

## When To Use Plain `Tool` Instead

Use `Tool` directly when at least one of these is true:

- Input shape is not known at compile time
- You need to accept arbitrary JSON and inspect it manually
- You need custom schema handling not derived from one Rust type
- You need to override `execute_effect` directly

## Result Versus Effect

In Tirea, a tool does not have to be limited to "JSON in, JSON result out".

- `execute(...) -> ToolResult` is the simple form
- `execute_effect(...) -> ToolExecutionEffect` is the richer form

`ToolExecutionEffect` lets a tool return:

- a `ToolResult`
- state actions
- non-state actions applied in `AfterToolExecute`

This matters when a tool must do more than report a result. For example:

- a skill activation tool may inject instructions into the message stream
- a tool may update reducer-backed state that later plugins read
- a domain tool may emit runtime actions in addition to patch-based state writes

## Example: Result Plus Actions

```rust,ignore
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
            .with_action(AnyStateAction::new::<SkillState>(
                SkillStateAction::Activate("docx".to_string()),
            ))
            .with_action(AfterToolExecuteAction::AddUserMessage(
                "Skill instructions...".to_string(),
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
