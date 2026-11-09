# Awaken Runtime SDK

Run an immutable Agent Snapshot with your model and tools. `Runtime::new()` and
`Runtime::default()` use the same policy. Customize continuation/retry behavior
explicitly through the `with_*` methods.

The SDK exposes canonical types at its root, plus `model`, `tools` and
`permission` modules for adapters. Their owners remain the existing contract
crates; the SDK does not duplicate types, persist a second state, or depend on a
model provider, server, or builtin-tool implementation.

## Start a conversation

```rust
use std::sync::Arc;
use awaken_runtime::{ExecutableAgentSnapshot, ModelBinding, Runtime, RuntimeRunContext};
use awaken_runtime::model::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_store_inmem::MemoryCommitCoordinator;

struct Model;
#[async_trait::async_trait]
impl LlmExecutor for Model {
    async fn infer(&self, _: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("Hello"), usage: None, stop_reason: None,
        })
    }
}

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let clock = awaken_time::system_clock();
let runtime = Runtime::new().with_clock(clock.clone()).with_llm(Arc::new(Model));
let agent = ExecutableAgentSnapshot::builder("assistant")
    .model(ModelBinding::parse("demo", "stub", "stub")?)
    .build();
let store = Arc::new(MemoryCommitCoordinator::new(clock));
let context = RuntimeRunContext::new().with_store(store.clone());
let conversation = runtime.conversation(&agent, "conversation", context)?;
let first = conversation.run("Hello").await?;
let second = conversation.run("Continue").await?;
assert_eq!(first.thread_id(), second.thread_id());
println!("{:?}: {:?}", second.run_id(), second.end_cause());
let transcript = second.transcript(store.as_ref())?;
# Ok(())
# }
```

`RunOutcome` is a point-in-time projection of a committed record. Its transcript
method reads the Thread's current conversation, including other Runs. It does
not represent a cached per-Run output buffer. Refresh a receipt with
`RunOutcome::read`; read its current ticket with `resume_ticket`.

## Register a typed tool

Implement `tools::Tool` with one argument type (`Deserialize + JsonSchema`) and
one serializable output type. `ToolRegistration::new("app", tool)?` produces a
paired descriptor and executable. Add `registration.descriptor().clone()` to the
Snapshot builder, and use `registration.install(runtime)?` to install the tool.
ID, description and schema come from the same type.
Invalid schemas and duplicate IDs are reported immediately. Authorization still
uses the configured permission gate. `RawTool` remains available for dynamic
integrations; the old builtin `erase` import forwards to the same tool-contract adapter.

## Approval, cancellation and recovery

Use `Conversation::run_to_completion_async` with an owned ticket and an async
callback returning `Result<ResumeResult, ApprovalError>`. For example:

```rust,no_run
# use awaken_runtime::{ExecutableAgentSnapshot, Runtime, RuntimeRunContext, ResumeResult};
# async fn example(runtime: &Runtime, agent: &ExecutableAgentSnapshot, context: RuntimeRunContext) -> Result<(), awaken_runtime::Error> {
let conversation = runtime.conversation(agent, "conversation", context)?;
let outcome = conversation.run_to_completion_async("Do the task",
    |ticket| async move {
        // Await your UI, approval service or policy query here.
        let _ = ticket;
        Ok(ResumeResult::allow())
    }).await?;
# Ok(())
# }
```

Inject the same `WallClock` into `Runtime::with_clock` and the commit store at
your application's composition root. The SDK requires it before calling an
approval callback; an absent clock returns `missing_clock` without consuming
the committed ticket. Ordinary runs and committed terminal replay need no clock.
Use `ApprovalOptions` with `run_to_completion_async_with_options` to set a
per-callback timeout. Timeout and callback
failure leave the committed Run Awaiting with its ticket available. Cancellation
through the context commits a terminal outcome through the existing control
path; it does not prove an external operation stopped. Dropping the caller's
future is not a durable cancellation request.

For restart/replay, retain the Snapshot, Run ID and Thread ID and call
`Conversation::resume_with_id_async`. The Snapshot must match the committed
ticket; terminal replay returns committed truth without another model/tool invocation.
`resume_with_id_async_with_options` also accepts the callback wait policy.
The synchronous callbacks use the same driver. All completion-driving entries
require a history reader before starting; `with_store` binds it with the writer.

## Errors and compatibility

`Err(error)` describes admission/infrastructure failure. An `Ok` result can carry
an `Ended(Error(...))` or `Indeterminate` Run outcome: inspect the committed state.
Use `error.kind()`, `error.code()` and `std::error::Error::source` for diagnostics.
Classification is not permission to retry an effect; retain existing idempotency,
execution-evidence and commit rules.

Use `Conversation` and `Runtime::conversation` for the SDK binding. The former
`EmbeddedSession` and `Runtime::session` source entries have been removed;
existing callers must migrate to the canonical names. This is a source-breaking
API change and requires explicit release qualification. Other existing contract
import paths and legacy string error variants remain. The development API adds
`execution::Error::Diagnostic`; downstream exhaustive matches must handle it.
New diagnostic/options/outcome types have
private fields or non-exhaustive enums. Before publishing a stable release,
review the API snapshot and apply the project's versioning policy.

The coordinator's commit rejection, resolver failure, admission diagnostics and
resume rejection retain their typed source. Legacy string variants provide a
conservative fallback classification; converting an arbitrary legacy message to
a precise retry category would require facts its original port did not retain.

For copyable dependency declarations and executable application tasks, see
[`examples/sdk-consumer`](../../../../examples/sdk-consumer/README.md) in the repository.

## Validation layers

| Layer | Design and acceptance | Execution |
| --- | --- | --- |
| Local SDK regression | Constructor equivalence, explicit retry limits, missing/duplicate configuration, schema rejection and typed commit failure; no model/tool effect on rejected admission | `cargo test -p awaken-runtime --test sdk_defaults --test sdk_api` |
| Public consumer | Single/multiple turns, typed tool, Unicode streaming, cancellation, async approval, filesystem restart and web policy; every required case in the consumer gate must execute | `python3 scripts/ci/check_sdk_consumer.py` in quick/push/full Rust gates |
| Approval lifecycle | Allow/deny, timeout, callback failure, cancellation, concurrent reply, cold recovery, identity mismatch, expiry at `deadline-1`, `deadline`, `deadline+1`; assert committed state/ticket and exact effect count | SDK regressions plus existing `awaiting`, `run_identity`, `terminal_observer` integration suites |
| Protocol projection | A2A versions and HTTP/RPC bindings project the same kind/code; localized or misleading prose cannot change classification | A2A unit matrix and `v1_sdk_compat` real-route tests |
| Workspace/platform | Production/default and maximal feature graphs, warning-free clippy, workspace nextest and doctests; required live backend/crash suites use owned services | Canonical full Rust gate and Windows/MSVC qualification |

Run the local layer while editing. Push/full gates verify the complete supported
feature graph and consumer; a selected local run is not full-workspace evidence.
Live PostgreSQL/NATS, backend crash/fencing and external model-provider suites
retain their integration requirements. These matrices assert behavior and effects;
their test counts do not claim measured line or branch coverage or a new formal proof.
