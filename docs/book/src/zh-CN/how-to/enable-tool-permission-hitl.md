# 启用工具权限 HITL

当你需要控制 agent 能调用哪些工具，并对敏感操作启用人工审批时，使用本页。

## 前置条件

- 已有可运行的 awaken runtime
- `awaken` 启用了 `permission`

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["permission"] }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## 步骤

1. 注册 permission 插件：

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_permission::PermissionPlugin;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, Plugin};

let agent_spec = AgentSpec::new("my-agent")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("permission");

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
    .with_plugin("permission", Arc::new(PermissionPlugin) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

permission 插件会注册一个 `ToolGateHook`，在每次工具真正执行前评估规则。

2. 以内联方式定义规则：

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
```

3. 也可以从 YAML 文件加载：

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
```

4. 通过 agent spec 激活：

```rust,ignore
let agent_spec = AgentSpec::new("my-agent")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("permission");
```

5. 理解规则优先级：

1. `Deny`
2. `Allow`
3. `Ask`

匹配 DSL 支持：

| Pattern | 匹配方式 |
|---------|----------|
| `read_file` | 精确匹配工具名 |
| `file_*` | 工具名 glob |
| `mcp__github__*` | MCP 工具 glob |
| `Bash(npm *)` | 主参数 glob |
| `Edit(file_path ~ "src/**")` | 命名字段 glob |
| `Bash(command =~ "(?i)rm")` | 命名字段 regex |
| `/mcp__(gh\|gl)__.*/` | 工具名 regex |

## 验证

1. 用一个命中 `deny` 的工具测试，调用应在执行前被阻断
2. 用一个命中 `ask` 的工具测试，run 应进入等待审批状态
3. 通过 mailbox 接口提交审批
4. 确认 run 恢复执行

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| 所有工具都被拦住 | `default_behavior: deny` 且无 allow 规则 | 显式给安全工具加 allow |
| 规则没有生效 | 插件没注册 | 注册 `PermissionPlugin` 并激活 |
| pattern 无效 | glob / regex 语法错 | 对照 DSL 语法检查 |
| ask 一直不恢复 | 没有 mailbox consumer | 让前端或 API 客户端消费审批请求 |

## 相关示例

- `crates/awaken-ext-permission/tests/`

## 关键文件

- `crates/awaken-ext-permission/src/lib.rs`
- `crates/awaken-ext-permission/src/config.rs`
- `crates/awaken-ext-permission/src/rules.rs`
- `crates/awaken-ext-permission/src/plugin/plugin.rs`
- `crates/awaken-ext-permission/src/plugin/checker.rs`（`PermissionToolGateHook`）
- `crates/awaken-tool-pattern/`

## 相关

- [添加 Plugin](./add-a-plugin.md)
- [HITL 与 Mailbox](../explanation/hitl-and-mailbox.md)
- [Tool Trait](../reference/tool-trait.md)
