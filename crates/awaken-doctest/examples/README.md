# awaken-doctest coverage map

Each `examples/*.rs` here is a smoke test that fixes one public API surface
the documentation cites. CI runs `cargo build --examples -p awaken-doctest`
and `cargo test --locked -p awaken-doctest --examples`; any rename or
signature change in the runtime fails compilation here before the docs go
stale.

This map exists because we retired the old `book_doctests!()` macro (which
compiled every `rust` fence in `docs/book/src/**/*.md`) when the Starlight
migration stripped `rust,ignore` modifiers for Shiki compatibility. The
explicit `examples/` approach trades broad coverage for precision; this
file keeps the gap visible.

## Covered surfaces

| Example                 | Public API surface                                                                                                       | Documentation site                                                            |
|-------------------------|--------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------|
| `tool_basic.rs`         | `awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult}`                     | `reference/tool-trait.md`, `how-to/add-a-tool.md`                             |
| `typed_tool.rs`         | `awaken::contract::tool::TypedTool` + schemars-derived `Args`                                                            | `reference/tool-trait.md`, `how-to/add-a-tool.md`                             |
| `agent_spec.rs`         | `awaken::registry_spec::{AgentSpec, ProviderSpec, ModelBindingSpec}`                                                     | `reference/config.md`, `reference/provider-model-config.md`                   |
| `effect_spec.rs`        | `awaken::model::{EffectSpec, TypedEffect::from_spec, TypedEffect::decode}`                                               | `reference/effects.md`                                                        |

## Uncovered surfaces — TODO

The list below tracks docs surfaces that cite Rust APIs but currently have
no `examples/*.rs` smoke test. Add one when you change the cited API or
when a doc fix lands.

| Documentation page                                | Cited surface                                          | Suggested example name              |
|---------------------------------------------------|--------------------------------------------------------|-------------------------------------|
| `reference/scheduled-actions.md`                  | `ScheduledActionSpec`, `TypedScheduledActionHandler`   | `scheduled_action.rs`               |
| `reference/state-keys.md`                         | `TypedStateKey`, `StateScope`, register macros         | `state_key.rs`                      |
| `reference/cancellation.md`                       | `CancellationToken`, `ToolCallResume`                  | `cancellation.rs`                   |
| `reference/errors.md`                             | `ToolError`, `StorageError`, `ResolveError` variants   | `error_variants.rs`                 |
| `reference/events.md`                             | `EventSink`, agent / tool / phase event payloads       | `event_sink.rs`                     |
| `reference/http-api.md`                           | `AppState`, `AppBuilder`, route registration           | `http_app_builder.rs`               |
| `reference/protocols/a2a.md`                      | `RemoteAuth`, `RemoteEndpoint` parsing                 | `remote_endpoint.rs`                |
| `reference/protocols/ai-sdk-v6.md`                | `/v1/ai-sdk/*` request/response shapes                 | `ai_sdk_payloads.rs`                |
| `reference/thread-model.md`                       | `ThreadStore` trait, `Thread`, `Message` shapes        | `thread_store_trait.rs`             |
| `reference/tool-execution-modes.md`               | `ToolCallContext` resume fields, suspension flow       | `tool_resume.rs`                    |
| `how-to/add-a-plugin.md`                          | `PluginRegistrar`, `Plugin` trait, hook signatures     | `plugin_registrar.rs`               |
| `how-to/use-mcp-tools.md`                         | `McpServerSpec`, `McpToolRegistryManager`              | `mcp_server_spec.rs`                |
| `how-to/use-skills-subsystem.md`                  | `SkillSpec`, skill discovery                           | `skill_spec.rs`                     |
| `how-to/use-deferred-tools.md`                    | `awaken_ext_deferred_tools` surface                    | `deferred_tool.rs`                  |
| `explanation/state-management.md`                 | `Snapshot`, `StateCommand`, `MutationOp`               | `state_command.rs`                  |
| `explanation/run-lifecycle-and-phases.md`         | `RunIdentity`, `Phase`, `RunStatus`                    | `run_lifecycle.rs`                  |

## Adding coverage

1. Pick the smallest reasonable shape — just construct the value(s) and
   call the canonical method. No live LLM, no network, no filesystem.
2. Drop into `examples/<surface>.rs`, follow the existing four files for
   format.
3. Cross off the row above; add the new row to the "Covered" table with
   the docs pages it stabilises.
4. Run `cargo build --examples -p awaken-doctest` to confirm it links.

The bar is intentionally low: a successful compile is enough. The point
is to catch _renamed types_ and _changed signatures_, not to run scenarios.
