# Add a Plugin

Use this when you need to extend the agent lifecycle with state keys, phase hooks, scheduled actions, or effect handlers.

## Prerequisites

- `awaken` crate added to `Cargo.toml`
- Familiarity with `Phase` variants and `StateKey`

## Steps

1. Define a state key.

```rust,no_run
use awaken::{StateKey, MergeStrategy, StateError, JsonValue};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditLog {
    pub entries: Vec<String>,
}

pub struct AuditLogKey;

impl StateKey for AuditLogKey {
    type Value = AuditLog;
    const KEY: &'static str = "audit_log";
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;

    type Update = AuditLog;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }

    fn encode(value: &Self::Value) -> Result<JsonValue, StateError> {
        serde_json::to_value(value).map_err(|e| StateError::KeyEncode { key: Self::KEY.into(), message: e.to_string() })
    }

    fn decode(json: JsonValue) -> Result<Self::Value, StateError> {
        serde_json::from_value(json).map_err(|e| StateError::KeyDecode { key: Self::KEY.into(), message: e.to_string() })
    }
}
```

2. Implement a phase hook.

```rust,ignore
use async_trait::async_trait;
use awaken::{PhaseHook, PhaseContext, StateCommand, StateError};

pub struct AuditHook;

#[async_trait]
impl PhaseHook for AuditHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut log = ctx.state::<AuditLogKey>().cloned().unwrap_or(AuditLog {
            entries: Vec::new(),
        });
        log.entries.push(format!("Phase executed at {:?}", ctx.phase));
        let mut cmd = StateCommand::new();
        cmd.update::<AuditLogKey>(log);
        Ok(cmd)
    }
}
```

3. Implement the Plugin trait.

```rust,ignore
use awaken::{Plugin, PluginDescriptor, PluginRegistrar, Phase, StateError, StateKeyOptions, KeyScope};

pub struct AuditPlugin;

impl Plugin for AuditPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "audit" }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<AuditLogKey>(StateKeyOptions {
            scope: KeyScope::Run,
            ..Default::default()
        })?;

        registrar.register_phase_hook(
            "audit",
            Phase::PostInference,
            AuditHook,
        )?;

        Ok(())
    }
}
```

4. Register the plugin and activate it on an agent.

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::{AgentSpec, AgentRuntimeBuilder};

let mut spec = AgentSpec::new("assistant")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("audit");
spec.plugin_ids.push("audit".into());

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("audit", Arc::new(AuditPlugin))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model_binding("claude-sonnet", ModelBinding {
        provider_id: "anthropic".into(),
        upstream_model: "claude-sonnet-4-20250514".into(),
    })
    .build()?;
```

`plugin_ids` loads the plugin for the agent. `with_hook_filter` only filters the
hooks, tools, and request transforms from plugins that have already been loaded.

## Verify

Run the agent and inspect the state snapshot. The `audit_log` key should contain entries added by the hook after each inference phase.

## Common Errors

| Error | Cause | Fix |
|---|---|---|
| `StateError::KeyAlreadyRegistered` | Two plugins register the same `StateKey` | Use a unique `KEY` constant per state key |
| `StateError::UnknownKey` | Accessing a key that was never registered | Ensure the plugin calling `register_key` is activated on the agent |
| Hook not firing | Plugin not loaded or hook filtered out | Add the plugin ID to `plugin_ids`; include it in `with_hook_filter` when using hook filters |

## Related Example

`crates/awaken-ext-observability/` -- the built-in observability plugin registers phase hooks and state keys.

## Key Files

- `crates/awaken-runtime/src/plugins/lifecycle.rs` -- `Plugin` trait
- `crates/awaken-runtime/src/plugins/registry.rs` -- `PluginRegistrar`
- `crates/awaken-runtime/src/hooks/phase_hook.rs` -- `PhaseHook` trait

## Related

- [Build an Agent](./build-an-agent.md)
- [Add a Tool](./add-a-tool.md)
