# Use Deferred Tools

Use this when your agent has many tools and you want to reduce context window usage by hiding tool schemas from the LLM until they are needed. The deferred-tools plugin classifies tools as Eager (always sent) or Deferred (hidden until requested). A `ToolSearch` tool lets the LLM discover deferred tools on demand.

## Prerequisites

- A working awaken agent runtime (see [First Agent](../tutorials/first-agent.md))
- The `awaken-ext-deferred-tools` crate

```toml
[dependencies]
awaken-ext-deferred-tools = { version = "0.1" }
awaken = { package = "awaken-agent", version = "0.1" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Steps

1. Create the plugin and register it.

Collect all tool descriptors your agent exposes, then pass them to `DeferredToolsPlugin::new`. The plugin uses these to classify tools and populate the deferred registry at activation time.

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, Plugin};
use awaken_ext_deferred_tools::DeferredToolsPlugin;

// Collect descriptors from all tools registered on the agent.
let seed_tools = vec![
    weather_tool.descriptor(),
    search_tool.descriptor(),
    debug_tool.descriptor(),
    // ... all tool descriptors
];
let mut agent_spec = AgentSpec::new("deferred-agent")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("Search for tools only when needed.")
    .with_hook_filter("ext-deferred-tools");
agent_spec.plugin_ids.push("ext-deferred-tools".into());

let runtime = AgentRuntimeBuilder::new()
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model_binding(
        "gpt-4o-mini",
        ModelBinding {
            provider_id: "openai".into(),
            upstream_model: "gpt-4o-mini".into(),
        },
    )
    .with_agent_spec(agent_spec)
    .with_plugin(
        "ext-deferred-tools",
        Arc::new(DeferredToolsPlugin::new(seed_tools)) as Arc<dyn Plugin>,
    )
    .build()
    .expect("failed to build runtime");
```

2. Configure tool loading rules.

Set rules on the agent spec via the `deferred_tools` config key. Rules are evaluated in order and first match wins. Tools that match no rule fall back to `default_mode`.

```rust,ignore
use awaken_ext_deferred_tools::{
    DeferredToolsConfig, DeferredToolsConfigKey, ToolLoadMode,
};
use awaken_ext_deferred_tools::config::DeferralRule;

let config = DeferredToolsConfig {
    rules: vec![
        DeferralRule { tool: "get_weather".into(), mode: ToolLoadMode::Eager },
        DeferralRule { tool: "debug_*".into(), mode: ToolLoadMode::Deferred },
    ],
    default_mode: ToolLoadMode::Deferred,
    ..Default::default()
};
agent_spec.set_config::<DeferredToolsConfigKey>(config)?;
```

Apply this before passing `agent_spec` into `AgentRuntimeBuilder`. The admin
console writes the same `deferred_tools` section. New runs pick up the saved
section after the config runtime publishes the next registry snapshot.

The `tool` field supports exact names and glob patterns (via `wildcard_match`). Common patterns:

| Pattern | Matches |
|---------|---------|
| `get_weather` | Exact tool ID |
| `debug_*` | Any tool starting with `debug_` |
| `mcp__github__*` | All GitHub MCP tools |

3. Understand auto-enable.

The `enabled` field on `DeferredToolsConfig` controls activation:

| Value | Behavior |
|-------|----------|
| `Some(true)` | Always enable deferred tools |
| `Some(false)` | Always disable |
| `None` (default) | Auto-enable when total token savings across deferred tools exceeds `beta_overhead` (default 1136 tokens) |

With auto-enable, the plugin uses the same token heuristic it stores in
`DiscBetaEntry`:

- Full schema cost: `c_i = max(len(parameters_json) / 4, 10)`
- Name-only cost: `c_bar_i = max(len(tool_name) / 4, 1)`
- Total savings: `sum(max(c_i - c_bar_i, 0))` across tools resolved to `Deferred`

If total savings is greater than `beta_overhead`, the plugin seeds
`DeferralState`, the deferred registry, and the DiscBeta state. If savings does
not clear the threshold, no deferral state is seeded and hooks are effectively
inactive for that agent. Set `enabled: Some(true)` to bypass the heuristic.

4. Understand how ToolSearch works.

The plugin automatically registers a `ToolSearch` tool. The LLM calls it with a query string to find deferred tools:

| Query format | Behavior |
|--------------|----------|
| `"select:Tool1,Tool2"` | Fetch specific tools by exact ID |
| `"+required rest terms"` | Require a keyword, rank by remaining terms |
| `"plain keywords"` | General keyword search across id, name, description |

When `ToolSearch` returns results, matched tools are promoted to Eager so their
schemas are included in later inference turns. They stay Eager until the
DiscBeta re-deferral policy observes enough idle evidence to hide them again.
The tool returns up to `max_results` (default 5) matching tool schemas in a
`<functions>` block.

```text
LLM: I need to check the weather. Let me search for relevant tools.
     -> calls ToolSearch { query: "weather forecast" }

ToolSearch returns matching schemas, promotes get_weather to Eager.
Next inference: get_weather schema is included in the tool list.
```

5. Understand the theory and DiscBeta re-deferral model.

Deferred tools optimize expected context cost. The model separates two
questions:

- **Initial placement:** should a tool's full schema be sent eagerly, or should
  only its name appear in the deferred-tool list?
- **Runtime retention:** after `ToolSearch` promotes a tool, is its observed
  usage frequent enough to keep paying the per-turn schema cost?

Promotion is always reactive: the plugin does not proactively promote tools
from probability estimates. The probability model only decides when an already
promoted tool should be moved back to Deferred.

For each seed tool, the plugin initializes a discounted Beta posterior:

```text
alpha_0 = max(p_i * n0, 0.01)
beta_0  = max((1 - p_i) * n0, 0.01)
```

`p_i` comes from `agent_priors[tool_id]` when present and defaults to `0.01`.
`n0` is the prior strength in equivalent observations.

After each inference turn, every tracked tool is discounted and then updated
with a Bernoulli observation for whether it appeared in that turn's tool calls:

```text
alpha_t = omega * alpha_{t-1} + 1   if the tool was called this turn
alpha_t = omega * alpha_{t-1}       otherwise

beta_t  = omega * beta_{t-1}        if the tool was called this turn
beta_t  = omega * beta_{t-1} + 1    otherwise
```

This makes old evidence decay. With the default `omega = 0.95`, the effective
memory is approximately `1 / (1 - omega) = 20` turns.

The posterior mean and normal-approximation variance are:

```text
p_hat = alpha / (alpha + beta)
var   = alpha * beta / ((alpha + beta)^2 * (alpha + beta + 1))
```

The implementation uses a 90% upper credible bound:

```text
upper_90 = min(1, p_hat + 1.282 * sqrt(var))
```

A promoted tool is re-deferred only when all of the following are true:

- The tool is currently Eager (was promoted from Deferred)
- The tool is not configured as always-Eager in rules
- The tool has been idle for at least `defer_after` turns
- `upper_90 < breakeven_p * thresh_mult`

The breakeven frequency is:

```text
breakeven_p = (c - c_bar) / gamma
```

`c` is the full schema cost, `c_bar` is the name-only cost, and `gamma` is the
estimated token cost of a `ToolSearch` call. Intuitively, a tool should stay
Eager only when its likely near-term use is high enough that avoiding future
`ToolSearch` calls outweighs carrying the full schema in every turn. Using the
upper credible bound makes the policy conservative: it waits until even an
optimistic estimate falls below the threshold.

Key parameters in `DiscBetaParams`:

| Parameter | Default | Purpose |
|-----------|---------|---------|
| `omega` | 0.95 | Discount factor per turn. Effective memory is approximately `1/(1-omega)` = 20 turns |
| `n0` | 5.0 | Prior strength in equivalent observations |
| `defer_after` | 5 | Minimum idle turns before considering re-deferral |
| `thresh_mult` | 0.5 | Multiplier on breakeven frequency for the deferral threshold |
| `gamma` | 2000.0 | Estimated token cost of a ToolSearch call |

These live under `DeferredToolsConfig.disc_beta`:

```rust,ignore
use awaken_ext_deferred_tools::{DeferredToolsConfig, DiscBetaParams};

let config = DeferredToolsConfig {
    disc_beta: DiscBetaParams {
        omega: 0.95,
        n0: 5.0,
        defer_after: 5,
        thresh_mult: 0.5,
        gamma: 2000.0,
    },
    beta_overhead: 1136.0,
    ..Default::default()
};
```

6. Enable cross-session learning.

Via `AgentToolPriors` (a `ProfileKey`), usage frequencies persist across sessions using EWMA (exponentially weighted moving average). At session end, the `PersistPriorsHook` writes per-tool presence frequencies to the profile store. At next session start, the `LoadPriorsHook` reads them back and initializes the Beta distribution with learned priors instead of the default 0.01.

This requires a `ProfileStore` to be configured on the runtime. No additional code is needed beyond the plugin registration — the hooks are wired automatically.

The EWMA smoothing factor is `lambda = max(0.1, 1/(n+1))`, where `n` is the session count. Early sessions contribute equally; after 10 sessions the factor stabilizes at 0.1, giving 90% weight to historical data.

## Verify

1. Run the agent and trigger an inference. Check logs for the `deferred_tools.list` context message, which lists all deferred tool names.

2. Read `DeferralState` from the runtime snapshot to see the current mode of each tool:

```rust,ignore
use awaken_ext_deferred_tools::state::{DeferralState, DeferralStateValue};

let state: &DeferralStateValue = snapshot.state::<DeferralState>()
    .expect("DeferralState not found");

for (tool_id, mode) in &state.modes {
    println!("{tool_id}: {mode:?}");
}
```

3. Ask the LLM a question that requires a deferred tool. Confirm `ToolSearch` is called and the tool is promoted to Eager in subsequent turns.

4. After several turns of inactivity, verify re-deferral by checking that the tool reverts to `Deferred` mode in the snapshot.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| All tools sent to LLM (no deferral) | `enabled: Some(false)` or total savings below `beta_overhead` | Set `enabled: Some(true)` or add more tools so savings exceed overhead |
| Plugin registers but no tools deferred | All rules resolve to `Eager` | Set `default_mode: ToolLoadMode::Deferred` or add `Deferred` rules |
| ToolSearch not available to LLM | Plugin not registered | Register `DeferredToolsPlugin` with seed tool descriptors |
| Tools never re-deferred | `defer_after` too high or tool usage is frequent | Lower `defer_after` or increase `thresh_mult` |
| Cross-session priors not loading | No `ProfileStore` configured | Wire a profile store into the runtime |
| ToolSearch returns no results | Tool not in deferred registry | Check that the tool was in the `seed_tools` list passed to the plugin |

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-ext-deferred-tools/src/lib.rs` | Module root and public re-exports |
| `crates/awaken-ext-deferred-tools/src/config.rs` | `DeferredToolsConfig`, `DeferralRule`, `ToolLoadMode`, `DiscBetaParams` |
| `crates/awaken-ext-deferred-tools/src/plugin/plugin.rs` | `DeferredToolsPlugin` registration |
| `crates/awaken-ext-deferred-tools/src/plugin/hooks.rs` | Phase hooks (BeforeInference, AfterToolExecute, AfterInference, RunStart, RunEnd) |
| `crates/awaken-ext-deferred-tools/src/tool_search.rs` | `ToolSearchTool` implementation and query parsing |
| `crates/awaken-ext-deferred-tools/src/policy.rs` | `ConfigOnlyPolicy` and `DiscBetaEvaluator` |
| `crates/awaken-ext-deferred-tools/src/state.rs` | State keys: `DeferralState`, `DeferralRegistry`, `DiscBetaState`, `ToolUsageStats`, `AgentToolPriors` |

## Related

- [Add a Plugin](./add-a-plugin.md)
- [Add a Tool](./add-a-tool.md)
- [Optimize Context Window](./optimize-context-window.md)
