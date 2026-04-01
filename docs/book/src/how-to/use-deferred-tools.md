# Use Deferred Tools

Use this when your agent has many tools and you want to reduce context window usage by hiding tool schemas from the LLM until they are needed. The deferred-tools plugin classifies tools as Eager (always sent) or Deferred (hidden until requested). A `ToolSearch` tool lets the LLM discover deferred tools on demand.

## Prerequisites

- A working awaken agent runtime (see [First Agent](../tutorials/first-agent.md))
- The `awaken-ext-deferred-tools` crate

```toml
[dependencies]
awaken-ext-deferred-tools = { version = "0.1" }
awaken = { version = "0.1" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Steps

1. Create the plugin and register it.

Collect all tool descriptors your agent exposes, then pass them to `DeferredToolsPlugin::new`. The plugin uses these to classify tools and populate the deferred registry at activation time.

```rust,ignore
use std::sync::Arc;
use awaken::{AgentRuntimeBuilder, Plugin};
use awaken_ext_deferred_tools::DeferredToolsPlugin;

// Collect descriptors from all tools registered on the agent.
let seed_tools = vec![
    weather_tool.descriptor(),
    search_tool.descriptor(),
    debug_tool.descriptor(),
    // ... all tool descriptors
];

let runtime = AgentRuntimeBuilder::new()
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
    DeferredToolsConfig, DeferralRule, ToolLoadMode,
};

let config = DeferredToolsConfig {
    rules: vec![
        DeferralRule { tool: "get_weather".into(), mode: ToolLoadMode::Eager },
        DeferralRule { tool: "debug_*".into(), mode: ToolLoadMode::Deferred },
    ],
    default_mode: ToolLoadMode::Deferred,
    ..Default::default()
};
```

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

With auto-enable, the plugin estimates per-tool schema cost as `len(parameters_json) / 4` tokens and sums the savings for all deferrable tools. If the total exceeds the overhead of maintaining the `ToolSearch` tool and deferred tool list in context, deferral activates automatically.

4. Understand how ToolSearch works.

The plugin automatically registers a `ToolSearch` tool. The LLM calls it with a query string to find deferred tools:

| Query format | Behavior |
|--------------|----------|
| `"select:Tool1,Tool2"` | Fetch specific tools by exact ID |
| `"+required rest terms"` | Require a keyword, rank by remaining terms |
| `"plain keywords"` | General keyword search across id, name, description |

When `ToolSearch` returns results, matched tools are promoted to Eager for the remainder of the session. The tool returns up to `max_results` (default 5) matching tool schemas in a `<functions>` block.

```text
LLM: I need to check the weather. Let me search for relevant tools.
     -> calls ToolSearch { query: "weather forecast" }

ToolSearch returns matching schemas, promotes get_weather to Eager.
Next inference: get_weather schema is included in the tool list.
```

5. Configure automatic re-deferral (DiscBeta).

The plugin tracks per-tool usage statistics using a discounted Beta distribution. Tools that were promoted via `ToolSearch` but stop being used get automatically re-deferred. This is adaptive and requires no manual tuning.

Re-deferral triggers when all of the following are true:
- The tool is currently Eager (was promoted from Deferred)
- The tool is not configured as always-Eager in rules
- The tool has been idle for at least `defer_after` turns
- The posterior upper credible interval (90%) falls below `breakeven_p * thresh_mult`

The breakeven frequency is `(c - c_bar) / gamma`, where `c` is the full schema cost and `c_bar` is the name-only cost. A tool is worth keeping eager only if it is used often enough that the savings from avoiding `ToolSearch` calls exceed the per-turn overhead.

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
