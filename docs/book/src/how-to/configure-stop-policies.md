# Configure Stop Policies

Use this when a run must terminate on explicit loop, budget, timeout, or domain-specific conditions.

## What is auto-wired

- `AgentDefinition.max_rounds` is lowered into `StopConditionSpec::MaxRounds` during agent wiring unless you already declared an explicit `max_rounds` stop spec.
- `AgentOsBuilder` wires the internal `stop_policy` behavior automatically when stop specs or stop-condition ids are present.
- `stop_policy` is a reserved behavior id. Register stop policies through builder APIs instead of attaching a behavior id manually.

## Prerequisites

- You know whether the stop rule is declarative (`StopConditionSpec`) or custom (`StopPolicy` trait).
- You have a way to observe terminal run status through events or the Run API.

## Steps

1. Add declarative stop specs on the agent definition.

```rust,ignore
use tirea::composition::AgentDefinition;
use tirea::runtime::StopConditionSpec;

let agent = AgentDefinition::new("deepseek-chat")
    .with_stop_condition_specs(vec![
        StopConditionSpec::Timeout { seconds: 30 },
        StopConditionSpec::LoopDetection { window: 4 },
        StopConditionSpec::StopOnTool {
            tool_name: "finish".to_string(),
        },
    ]);
```

2. Keep `max_rounds` aligned with your stop strategy.

- `max_rounds` still acts as the default loop-depth guard.
- If you already added `StopConditionSpec::MaxRounds`, do not expect `max_rounds` to stack on top of it.

3. Register reusable custom stop policies when declarative specs are not enough.

```rust,ignore
use std::sync::Arc;
use tirea::composition::AgentOsBuilder;
use tirea::runtime::{StopPolicy, StopPolicyInput};
use tirea::contracts::StoppedReason;

struct AlwaysStop;

impl StopPolicy for AlwaysStop {
    fn id(&self) -> &str {
        "always"
    }

    fn evaluate(&self, _input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
        Some(StoppedReason::new("always_stop"))
    }
}

let os = AgentOsBuilder::new()
    .with_stop_policy("always", Arc::new(AlwaysStop))
    .with_agent(
        "assistant",
        AgentDefinition::new("deepseek-chat").with_stop_condition_id("always"),
    )
    .build()?;
```

4. Observe the terminal reason from events or run records.

- `AgentEvent::RunFinish { termination, .. }`
- `GET /v1/runs/:id` (`termination_code`, `termination_detail`)

## Verify

- The run terminates with the expected stopped reason instead of timing out implicitly elsewhere.
- `GET /v1/runs/:id` reflects the same terminal reason that you observed in the event stream.
- A new run starts with fresh stop-policy runtime state rather than inheriting counters from the previous run.

## Common Errors

- Trying to register `stop_policy` as a normal behavior id.
- Expecting `max_rounds` and `StopConditionSpec::MaxRounds` to both apply independently.
- Using stop policies for tool authorization. Authorization belongs in tool/policy behaviors, not termination logic.
- Forgetting that stop-policy runtime bookkeeping is run-scoped and will be reset on the next run.

## Related Example

- `examples/src/starter_backend/mod.rs` defines a `stopper` agent that terminates on `StopConditionSpec::StopOnTool { tool_name: "finish" }`

## Key Files

- `crates/tirea-agentos/src/runtime/stop_policy_plugin.rs`
- `crates/tirea-agentos/src/composition/wiring.rs`
- `crates/tirea-agentos/src/composition/agent_definition.rs`
- `crates/tirea-agentos/src/runtime/tests.rs`

## Related

- [Config](../reference/config.md)
- [Run Lifecycle and Phases](../explanation/run-lifecycle-and-phases.md)
- [HITL and Decision Flow](../explanation/hitl-and-decision-flow.md)
