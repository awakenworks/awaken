# awaken-doctest coverage map

Each `examples/*.rs` here is a smoke test that fixes one public API surface
the documentation cites. CI runs `cargo build --examples -p awaken-doctest`
and `cargo test --locked -p awaken-doctest --examples`; the latter actually
**executes** each example's `main()` (each example is registered with
`harness = false` in `Cargo.toml`), so runtime asserts inside `main()`
fail CI alongside compilation errors. Any rename, signature change, or
runtime panic in the constructed shape fails CI before the docs go stale.

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
| `agent_spec.rs`         | `awaken::registry_spec::{AgentSpec, ProviderSpec, ModelSpec}`                                                            | `reference/config.md`, `reference/provider-model-config.md`                   |
| `effect_spec.rs`        | `awaken::model::{EffectSpec, TypedEffect::from_spec, TypedEffect::decode}`                                               | `reference/effects.md`                                                        |
| `scheduled_action.rs`   | `awaken::{ScheduledActionSpec, Phase, TypedScheduledActionHandler, PhaseContext}` — spec + handler impl + payload round-trip | `reference/scheduled-actions.md`                                              |
| `state_key.rs`          | `awaken::{StateKey, MergeStrategy, KeyScope}` apply/encode                                                               | `reference/state-keys.md`                                                     |
| `cancellation.rs`       | `awaken::CancellationToken` + `CancellationHandle` pair                                                                  | `reference/cancellation.md`                                                   |
| `error_variants.rs`     | `ToolError`, `StorageError`, `ResolveError` message-format contracts                                                     | `reference/errors.md`                                                         |
| `event_sink.rs`         | `EventSink` / `VecEventSink` / `NullEventSink` + representative `AgentEvent` families (lifecycle, text, reasoning, tool-call start/ready/done, cancel) | `reference/events.md`                                                         |
| `http_app_builder.rs`   | `AgentRuntimeBuilder` → `Mailbox` → `AppState::new` wiring (with `InMemoryStore` + `InMemoryMailboxStore` + `ServerConfig`) | `reference/http-api.md`                                                       |
| `remote_endpoint.rs`    | `awaken::registry_spec::{RemoteAuth, RemoteEndpoint}` + bearer helper                                                    | `reference/protocols/a2a.md`                                                  |
| `ai_sdk_payloads.rs`    | `UIStreamEvent` cross-section: `MessageStart`, `TextDelta`, `ToolInputStart`/`Available`, `ToolOutputAvailable`, `StartStep`, `Finish` | `reference/protocols/ai-sdk-v6.md`                                            |
| `thread_store_trait.rs` | `awaken::contract::storage::ThreadStore` save/load via `InMemoryStore` + `Message::user`                                 | `reference/thread-model.md`                                                   |
| `tool_resume.rs`        | `awaken::contract::suspension::{ToolCallResume, ResumeDecisionAction}` decision payload                                  | `reference/tool-execution-modes.md`                                           |
| `plugin_registrar.rs`   | `awaken::{Plugin, PluginDescriptor, PluginRegistrar, AgentSpec}` trait shape                                             | `how-to/add-a-plugin.md`                                                      |
| `mcp_server_spec.rs`    | `awaken::registry_spec::{McpServerSpec, McpTransportKind}` stdio + http variants                                         | `how-to/use-mcp-tools.md`                                                     |
| `skill_spec.rs`         | `awaken::{SkillSpec, SkillArgumentSpec}` + `allowed_tools`                                                               | `how-to/use-skills-subsystem.md`                                              |
| `deferred_tool.rs`      | `awaken_ext_deferred_tools::{DeferredToolsConfig, DeferredToolsConfigKey, DeferralRule, ToolLoadMode}`                   | `how-to/use-deferred-tools.md`                                                |
| `state_command.rs`      | `awaken::state::StateCommand` + `schedule_action` / `emit` / `extend`                                                    | `explanation/state-management.md`                                             |
| `run_lifecycle.rs`      | `awaken::contract::identity::{RunIdentity, RunOrigin}` + `RunStatus` + `Phase::ALL`                                      | `explanation/run-lifecycle-and-phases.md`                                     |

### Scope of "covered"

These are **smoke tests**, not behavioural tests. Each example pins:

- the trait / struct **shape** (field names, generic params, method signatures),
- a **representative** subset of variants where the enum is wide
  (`AgentEvent`, `UIStreamEvent`), and
- one or two trivial round-trips / `assert_eq!`s that fail if a rename or
  silent serde drift breaks the wire format.

Behavioural correctness (state-store concurrency, MCP tool registry
lifecycle, real run loops, etc.) is covered by the unit/integration
tests in the owning crates, not here. The 16 previously-listed TODO
surfaces now each have a paired example; when docs cite a new public
type, add another smoke test (and the `[[example]] harness = false`
block in `Cargo.toml`).

## Adding coverage

1. Pick the smallest reasonable shape — just construct the value(s) and
   call the canonical method. No live LLM, no network, no filesystem.
2. Drop into `examples/<surface>.rs`, follow the existing four files for
   format.
3. Cross off the row above; add the new row to the "Covered" table with
   the docs pages it stabilises.
4. **Add a matching `[[example]]` entry with `harness = false`** to
   `crates/awaken-doctest/Cargo.toml`. Without it `cargo test --examples`
   would skip the runtime path and only compile-check the example.
5. Run `cargo test --locked -p awaken-doctest --example <name>` to confirm
   it links AND its `main()` returns zero.

The bar is intentionally low: shape construction + a trivial `assert_eq!`
on round-tripped types is enough. The point is to catch _renamed types_,
_changed signatures_, **and silent shape-drift** (e.g. an `EffectSpec`
decode that compiles but no longer round-trips), not to run scenarios.
