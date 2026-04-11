# Configure Stop Policies

Use this when you need to control when an agent run terminates based on round count, token usage, elapsed time, or error frequency.

## Prerequisites

- `awaken` crate added to `Cargo.toml`
- Familiarity with `Plugin` and `AgentRuntimeBuilder`

## Overview

Stop policies evaluate after each inference step and decide whether the run should continue or terminate. The system provides four built-in policies and a trait for custom implementations.

Built-in policies:

| Policy | Triggers when |
|---|---|
| `MaxRoundsPolicy` | Step count exceeds `max` rounds |
| `TokenBudgetPolicy` | Total tokens (input + output) exceed `max_total` |
| `TimeoutPolicy` | Elapsed wall time exceeds `max_ms` milliseconds |
| `ConsecutiveErrorsPolicy` | Consecutive inference errors reach `max` |

## Steps

1. Create policies programmatically.

```rust,ignore
use std::sync::Arc;
use awaken::policies::{
    MaxRoundsPolicy, TokenBudgetPolicy, TimeoutPolicy,
    ConsecutiveErrorsPolicy, StopPolicy,
};

let policies: Vec<Arc<dyn StopPolicy>> = vec![
    Arc::new(MaxRoundsPolicy::new(25)),
    Arc::new(TokenBudgetPolicy::new(100_000)),
    Arc::new(TimeoutPolicy::new(300_000)), // 5 minutes in ms
    Arc::new(ConsecutiveErrorsPolicy::new(3)),
];
```

2. Register a `StopConditionPlugin` with the runtime builder.

```rust,ignore
use awaken::policies::StopConditionPlugin;
use awaken::AgentRuntimeBuilder;

let mut spec = spec;
spec.plugin_ids.push("stop-condition".into());

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("stop-condition", Arc::new(StopConditionPlugin::new(policies)))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(provider))
    .build()?;
```

For the common case of limiting rounds only, use the convenience wrapper:

```rust,ignore
use awaken::policies::MaxRoundsPlugin;

let mut spec = spec;
spec.plugin_ids.push("stop-condition:max-rounds".into());

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("stop-condition:max-rounds", Arc::new(MaxRoundsPlugin::new(10)))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(provider))
    .build()?;
```

Custom stop-condition plugins must be listed in `AgentSpec.plugin_ids`. The
built-in `AgentSpec.max_rounds` guard is still injected automatically; use these
plugins when you need additional policy types.

3. Use declarative `StopConditionSpec` values.

The `policies_from_specs` function converts declarative specs into policy instances. This is useful when loading configuration from JSON or YAML.

```rust,ignore
use awaken_contract::contract::lifecycle::StopConditionSpec;
use awaken::policies::{policies_from_specs, StopConditionPlugin};

let specs = vec![
    StopConditionSpec::MaxRounds { rounds: 10 },
    StopConditionSpec::Timeout { seconds: 300 },
    StopConditionSpec::TokenBudget { max_total: 100_000 },
    StopConditionSpec::ConsecutiveErrors { max: 3 },
];

let policies = policies_from_specs(&specs);
let plugin = StopConditionPlugin::new(policies);
```

The full set of `StopConditionSpec` variants:

```rust,ignore
pub enum StopConditionSpec {
    MaxRounds { rounds: usize },
    Timeout { seconds: u64 },
    TokenBudget { max_total: usize },
    ConsecutiveErrors { max: usize },
    StopOnTool { tool_name: String },      // not yet implemented
    ContentMatch { pattern: String },       // not yet implemented
    LoopDetection { window: usize },        // not yet implemented
}
```

`StopOnTool`, `ContentMatch`, and `LoopDetection` are defined in the contract but not yet backed by policy implementations. `policies_from_specs` silently skips unimplemented variants.

## The StopPolicy Trait

Implement `StopPolicy` to create custom stop conditions:

```rust,ignore
use awaken::policies::{StopPolicy, StopDecision, StopPolicyStats};

pub struct MyCustomPolicy {
    pub threshold: u64,
}

impl StopPolicy for MyCustomPolicy {
    fn id(&self) -> &str {
        "my_custom"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if stats.total_output_tokens > self.threshold {
            StopDecision::Stop {
                code: "my_custom".into(),
                detail: format!("output tokens {} exceeded {}", stats.total_output_tokens, self.threshold),
            }
        } else {
            StopDecision::Continue
        }
    }
}
```

The trait requires `Send + Sync + 'static`. Evaluation must be synchronous -- it is pure computation on the provided stats.

## StopPolicyStats

Every policy receives a `StopPolicyStats` snapshot with fields populated by the internal `StopConditionHook`:

| Field | Type | Description |
|---|---|---|
| `step_count` | `u32` | Number of inference steps completed so far |
| `total_input_tokens` | `u64` | Cumulative prompt tokens across all steps |
| `total_output_tokens` | `u64` | Cumulative completion tokens across all steps |
| `elapsed_ms` | `u64` | Wall time since the first step, in milliseconds |
| `consecutive_errors` | `u32` | Current streak of consecutive inference errors (resets on success) |
| `last_tool_names` | `Vec<String>` | Tool names called in the most recent inference response |
| `last_response_text` | `String` | Text content of the most recent inference response |

## StopDecision

```rust,ignore
pub enum StopDecision {
    Continue,
    Stop { code: String, detail: String },
}
```

When any policy returns `StopDecision::Stop`, the hook converts it to `TerminationReason::Stopped` with the given code and detail, then updates the run lifecycle to `Done`. The agent loop exits after the current step. Policies are evaluated in order; the first `Stop` wins.

## How Stop Policies Interact with the Agent Loop

1. The `StopConditionPlugin` registers a `PhaseHook` on `Phase::AfterInference`.
2. After each LLM inference, the hook increments `step_count`, accumulates token usage, and tracks consecutive errors in `StopConditionStatsState`.
3. The hook builds a `StopPolicyStats` snapshot and calls `evaluate` on each registered policy.
4. If any policy returns `Stop`, the hook emits a `RunLifecycleUpdate::Done` state command with the stop code, which terminates the run.
5. If all policies return `Continue`, the agent loop proceeds to the next step.

A policy with `max` or `max_total` set to `0` is treated as disabled and always returns `Continue`.

## Common Errors

| Error | Cause | Fix |
|---|---|---|
| Run never stops | No stop policy registered and LLM keeps calling tools | Register at least `MaxRoundsPolicy` or `MaxRoundsPlugin` |
| `StateError::KeyAlreadyRegistered` | Both `StopConditionPlugin` and `MaxRoundsPlugin` registered | Use only one; they share the same state key |
| Timeout fires too early | `TimeoutPolicy` takes milliseconds, `StopConditionSpec::Timeout` takes seconds | When using `TimeoutPolicy::new()` directly, pass milliseconds |

## Key Files

- `crates/awaken-runtime/src/policies/mod.rs` -- module root and public exports
- `crates/awaken-runtime/src/policies/policy.rs` -- `StopPolicy` trait, built-in policies, `policies_from_specs`
- `crates/awaken-runtime/src/policies/plugin.rs` -- `StopConditionPlugin` and `MaxRoundsPlugin`
- `crates/awaken-runtime/src/policies/state.rs` -- `StopConditionStatsState` and its state key
- `crates/awaken-runtime/src/policies/hook.rs` -- internal `StopConditionHook` that drives evaluation
- `crates/awaken-contract/src/contract/lifecycle.rs` -- `StopConditionSpec` enum

## Related

- [Add a Plugin](./add-a-plugin.md)
- [Build an Agent](./build-an-agent.md)
