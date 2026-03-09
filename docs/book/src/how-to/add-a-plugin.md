# Add a Plugin

Use this for cross-cutting behavior such as policy checks, approval gates, reminders, and observability.

## Prerequisites

- You know which phase should emit behavior (`RunStart`, `BeforeInference`, `BeforeToolExecute`, `AfterToolExecute`, `RunEnd`, etc.).
- Plugin side effects are explicit and bounded.

## Steps

1. Implement `AgentBehavior` and assign a stable `id()`.
2. Return phase actions with `ActionSet<...>` from the phase hooks you need.
3. Register behavior in `AgentOsBuilder::with_registered_behavior("id", plugin)`.
4. Attach behavior id in `AgentDefinition.behavior_ids` or `with_behavior_id(...)`.

## Minimal Pattern

```rust,ignore
use async_trait::async_trait;
use tirea::contracts::runtime::phase::{ActionSet, BeforeInferenceAction};
use tirea::contracts::{AgentBehavior, ReadOnlyContext};

struct AuditBehavior;

#[async_trait]
impl AgentBehavior for AuditBehavior {
    fn id(&self) -> &str {
        "audit"
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        ActionSet::single(BeforeInferenceAction::AddSystemContext(
            "Audit: request entering inference".to_string(),
        ))
    }
}
```

## Verify

- Behavior hook runs at the intended phase.
- Event/thread output contains expected behavior side effects.
- Runs are unchanged when behavior preconditions are not met.

## Common Errors

- Registering behavior but forgetting to include its id in `AgentDefinition.behavior_ids`.
- Using the wrong phase (effect appears too early or too late).
- Unbounded mutations in a behavior, making runs hard to reason about.

## Related Example

- `examples/src/travel.rs` shows a production `LLMMetryPlugin` registration path
- `examples/src/starter_backend/mod.rs` wires permission and tool-policy behaviors into multiple agents

## Key Files

- `crates/tirea-contract/src/runtime/behavior.rs`
- `crates/tirea-agentos/src/composition/builder.rs`
- `crates/tirea-extension-reminder/src/lib.rs`
- `crates/tirea-extension-permission/src/plugin.rs`

## Related

- [Actions](../reference/actions.md)
- [Run Lifecycle and Phases](../explanation/run-lifecycle-and-phases.md)
- [Build an Agent](./build-an-agent.md)
- [Debug a Run](./debug-a-run.md)
