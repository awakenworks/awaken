# Use the Reminder Plugin

Use this when you want the agent to receive automatic context messages after tool execution based on pattern matching -- for example, reminding the agent to run `cargo check` after editing a `.toml` file, or warning about destructive commands.

## Prerequisites

- A working awaken agent runtime (see [Build an Agent](./build-an-agent.md))
- Feature `reminder` enabled on the `awaken` crate

```toml
[dependencies]
awaken = { version = "0.1", features = ["reminder"] }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Steps

1. Register the reminder plugin with rules.

```rust,ignore
use std::sync::Arc;
use awaken::{AgentRuntimeBuilder, Plugin};
use awaken::ext_reminder::{ReminderPlugin, ReminderRulesConfig};

let json = r#"{
    "rules": [
        {
            "tool": "Bash(command ~ 'rm *')",
            "output": { "status": "success" },
            "message": {
                "target": "suffix_system",
                "content": "A deletion command just succeeded. Verify the result."
            }
        }
    ]
}"#;

let config = ReminderRulesConfig::from_str(json, Some("json"))
    .expect("failed to parse reminder config");
let rules = config.into_rules().expect("invalid rules");

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_plugin("reminder", Arc::new(ReminderPlugin::new(rules)) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

The plugin registers an `AfterToolExecute` phase hook. After every tool call, it evaluates each rule against the tool name, arguments, and result. When a rule matches, it schedules an `AddContextMessage` action that injects the configured message into the prompt.

2. Define rules with tool patterns.

   The `tool` field uses the same pattern DSL as the permission system:

| Pattern | Matches |
|---------|---------|
| `"Bash"` | Exact tool name `Bash` |
| `"*"` | Any tool |
| `"mcp__*"` | Glob on tool name (all MCP tools) |
| `"Bash(command ~ 'rm *')"` | Tool name + primary argument glob |
| `"Edit(file_path ~ '*.toml')"` | Named field glob |

```rust,ignore
let json = r#"{
    "rules": [
        {
            "name": "toml-edit-reminder",
            "tool": "Edit(file_path ~ '*.toml')",
            "output": "any",
            "message": {
                "target": "system",
                "content": "You edited a TOML file. Run cargo check to verify.",
                "cooldown_turns": 3
            }
        }
    ]
}"#;
```

The optional `name` field gives the rule a human-readable identifier. When omitted, one is auto-generated from the index and tool pattern (e.g. `rule-0-Edit(file_path ~ '*.toml')`).

3. Configure output matching.

   The `output` field controls whether the rule fires based on the tool result. Set it to `"any"` to match all outputs, or use a structured object with `status` and/or `content`:

```rust,ignore
// Match only errors containing "permission denied"
let json = r#"{
    "rules": [
        {
            "tool": "*",
            "output": {
                "status": "error",
                "content": "*permission denied*"
            },
            "message": {
                "target": "suffix_system",
                "content": "Permission denied. Consider using sudo or checking file ownership."
            }
        }
    ]
}"#;
```

**Status values:** `"success"`, `"error"`, `"pending"`, `"any"`.

**Content matching** supports two forms:

- **Text glob** (string shorthand): `"*permission denied*"` -- matches the stringified tool output against a glob pattern.
- **JSON fields** (structured): matches specific fields in JSON output.

```rust,ignore
// JSON field matching
let json = r#"{
    "rules": [
        {
            "tool": "*",
            "output": {
                "status": "error",
                "content": {
                    "fields": [
                        { "path": "error.code", "op": "exact", "value": "403" }
                    ]
                }
            },
            "message": {
                "target": "suffix_system",
                "content": "HTTP 403 Forbidden. Check authentication credentials."
            }
        }
    ]
}"#;
```

Field match operations: `"glob"` (default), `"exact"`, `"regex"`, `"not_glob"`, `"not_exact"`, `"not_regex"`. The `path` uses dot-separated JSON field names (e.g. `"error.code"`). When multiple fields are specified, all must match (AND logic).

When both `status` and `content` are present, both must match for the rule to fire.

4. Choose a message target.

   The `target` field determines where the injected message appears in the prompt:

| Target | Placement |
|--------|-----------|
| `"system"` | Prepended to the system prompt section |
| `"suffix_system"` | Appended after the system prompt |
| `"session"` | Inserted as a session-scoped system message |
| `"conversation"` | Inserted as a conversation-scoped system message |

5. Use cooldown to avoid repetition.

   Set `cooldown_turns` on the message to suppress re-injection for a number of turns after firing:

```rust,ignore
let json = r#"{
    "rules": [
        {
            "tool": "*",
            "output": "any",
            "message": {
                "target": "system",
                "content": "Remember to be careful with file operations.",
                "cooldown_turns": 5
            }
        }
    ]
}"#;
```

When `cooldown_turns` is `0` (default), the message is injected on every match.

6. Load rules from a file.

```rust,ignore
use awaken::ext_reminder::ReminderRulesConfig;

let config = ReminderRulesConfig::from_file("reminders.json")
    .expect("failed to load reminder config");
let rules = config.into_rules().expect("invalid rules");
```

7. Activate the plugin on an agent spec.

```rust,ignore
use awaken::registry_spec::AgentSpec;

let agent_spec = AgentSpec::new("my-agent")
    .with_model("anthropic/claude-sonnet")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("reminder");
```

The `with_hook_filter("reminder")` call activates the reminder plugin's phase hook for this agent.

## Verify

1. Configure a rule matching a tool you can easily trigger (e.g. `"*"` with `"any"` output).
2. Run the agent and invoke a tool.
3. Inspect the prompt or enable `tracing` at debug level. You should see:
   ```
   reminder rule matched, scheduling context message
   ```
4. Confirm the context message appears in the agent's next inference prompt at the expected target location.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| `InvalidPattern` on startup | Malformed tool pattern string | Check syntax against the pattern table above; ensure quotes are escaped in JSON |
| `InvalidTarget` on startup | Unknown message target | Use one of: `system`, `suffix_system`, `session`, `conversation` |
| `InvalidOutput` on startup | Unrecognized output matcher string | Use `"any"` or a structured `{ "status": ..., "content": ... }` object |
| `InvalidOp` on startup | Unknown field match operation | Use one of: `glob`, `exact`, `regex`, `not_glob`, `not_exact`, `not_regex` |
| Rule never fires | Plugin not activated on agent | Add `with_hook_filter("reminder")` to the agent spec |
| Rule fires too often | No cooldown configured | Set `cooldown_turns` to a positive value |

## Related Example

- `crates/awaken-ext-reminder/src/config.rs` -- contains test cases with various rule configurations

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-ext-reminder/src/lib.rs` | Module root and public re-exports |
| `crates/awaken-ext-reminder/src/config.rs` | `ReminderRulesConfig`, JSON loading, `ReminderConfigKey` |
| `crates/awaken-ext-reminder/src/rule.rs` | `ReminderRule` struct definition |
| `crates/awaken-ext-reminder/src/output_matcher.rs` | `OutputMatcher`, `ContentMatcher`, status/content matching logic |
| `crates/awaken-ext-reminder/src/plugin/plugin.rs` | `ReminderPlugin` registration (`AfterToolExecute` hook) |
| `crates/awaken-ext-reminder/src/plugin/hook.rs` | `ReminderHook` -- pattern + output evaluation per tool call |
| `crates/awaken-tool-pattern/` | Shared glob/regex pattern matching library |

## Related

- [Enable Tool Permission HITL](./enable-tool-permission-hitl.md) -- uses the same tool pattern DSL
- [Add a Plugin](./add-a-plugin.md)
- [Build an Agent](./build-an-agent.md)
