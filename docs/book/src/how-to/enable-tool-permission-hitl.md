# Enable Tool Permission HITL

Use this when you need to control which tools an agent can invoke, with human-in-the-loop approval for sensitive operations.

## Prerequisites

- A working awaken agent runtime (see [First Agent](../tutorials/first-agent.md))
- Feature `permission` enabled on the `awaken` crate (enabled by default)

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["permission"] }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Steps

1. Register the permission plugin.

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_permission::PermissionPlugin;
use awaken::registry_spec::{AgentSpec, ModelSpec};
use awaken::{AgentRuntimeBuilder, Plugin};

let agent_spec = AgentSpec::new("my-agent")
    .with_model("gpt-4o-mini")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("permission");

let runtime = AgentRuntimeBuilder::new()
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model(
        "gpt-4o-mini",
        ModelSpec {
            id: "gpt-4o-mini".into(),
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
        },
    )
    .with_agent_spec(agent_spec)
    .with_plugin("permission", Arc::new(PermissionPlugin) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

The plugin registers a `ToolGateHook` that evaluates permission rules before every tool call.

2. Define permission rules inline.

```rust,ignore
use awaken::ext_permission::{PermissionRulesConfig, PermissionRuleEntry, ToolPermissionBehavior};

let config = PermissionRulesConfig {
    default_behavior: ToolPermissionBehavior::Ask,
    rules: vec![
        PermissionRuleEntry {
            tool: "read_file".into(),
            behavior: ToolPermissionBehavior::Allow,
            scope: Default::default(),
        },
        PermissionRuleEntry {
            tool: "file_*".into(),
            behavior: ToolPermissionBehavior::Ask,
            scope: Default::default(),
        },
        PermissionRuleEntry {
            tool: "delete_*".into(),
            behavior: ToolPermissionBehavior::Deny,
            scope: Default::default(),
        },
    ],
};

let ruleset = config.into_ruleset().expect("invalid rules");
```

3. Load rules from a YAML file (alternative).

   Create `permissions.yaml`:

```yaml
default_behavior: ask
rules:
  - tool: "read_file"
    behavior: allow
  - tool: "Bash(npm *)"
    behavior: allow
  - tool: "file_*"
    behavior: ask
  - tool: "delete_*"
    behavior: deny
  - tool: "mcp__*"
    behavior: ask
```

Load it in code:

```rust,ignore
use awaken::ext_permission::PermissionRulesConfig;

let config = PermissionRulesConfig::from_file("permissions.yaml")
    .expect("failed to load permissions");
let ruleset = config.into_ruleset().expect("invalid rules");
```

4. Configure via agent spec.

   Permission rules can also be embedded in the agent spec using the `permission` plugin config key:

```rust,ignore
use awaken::registry_spec::AgentSpec;

let agent_spec = AgentSpec::new("my-agent")
    .with_model("gpt-4o-mini")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("permission");
```

5. Understand rule evaluation.

   Rules are evaluated with firewall-like priority:

1. **Deny** -- highest priority, blocks the tool call immediately
2. **Allow** -- permits the tool call without user interaction
3. **Ask** -- suspends the tool call and waits for human approval

The pattern syntax supports:

| Pattern | Matches |
|---------|---------|
| `read_file` | Exact tool name |
| `file_*` | Glob on tool name |
| `mcp__github__*` | Glob for MCP-prefixed tools |
| `Bash(npm *)` | Tool name with primary argument glob |
| `Edit(file_path ~ "src/**")` | Named field glob |
| `Bash(command =~ "(?i)rm")` | Named field regex |
| `/mcp__(gh\|gl)__.*/` | Regex tool name |

## Verify

1. Register a tool that matches a `deny` rule and attempt to invoke it.
2. The tool call should be blocked before execution, with the run terminating for deny rules.
3. Register a tool matching an `ask` rule. The run should suspend, waiting for human approval via the mailbox.
4. Send approval through the mailbox endpoint and confirm the run resumes.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| All tools blocked | `default_behavior: deny` with no `allow` rules | Add explicit `allow` rules for safe tools |
| Rules not evaluated | Plugin not registered | Register `PermissionPlugin` and add `"permission"` to `plugin_ids` in agent spec |
| Invalid pattern error | Malformed glob or regex | Check syntax against the pattern table above |
| Ask rule never resolves | No mailbox consumer | Wire up a frontend or API client to respond to mailbox items |

## Related Example

- `crates/awaken-ext-permission/tests/`

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-ext-permission/src/lib.rs` | Module root and public re-exports |
| `crates/awaken-ext-permission/src/config.rs` | `PermissionRulesConfig` and YAML/JSON loading |
| `crates/awaken-ext-permission/src/rules.rs` | Pattern syntax, `ToolPermissionBehavior`, rule evaluation |
| `crates/awaken-ext-permission/src/plugin/plugin.rs` | `PermissionPlugin` registration |
| `crates/awaken-ext-permission/src/plugin/checker.rs` | `PermissionToolGateHook` (`ToolGate`) |
| `crates/awaken-tool-pattern/` | Shared glob/regex pattern matching library |

## Related

- [Add a Plugin](./add-a-plugin.md)
- [HITL and Mailbox](../explanation/hitl-and-mailbox.md)
- [Tool Trait Reference](../reference/tool-trait.md)
