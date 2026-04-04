# 使用 Reminder 插件

当你希望 agent 在工具执行后，根据模式匹配自动收到上下文提示时，使用本页。例如：修改 `.toml` 后提醒执行 `cargo check`，或在危险命令后给出警告。

## 前置条件

- 已有可运行的 awaken runtime
- `awaken` 启用了 `reminder`

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["reminder"] }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## 步骤

1. 用规则注册 reminder 插件：

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_reminder::{ReminderPlugin, ReminderRulesConfig};
use awaken::registry_spec::{AgentSpec, ModelSpec};
use awaken::{AgentRuntimeBuilder, Plugin};

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
let agent_spec = AgentSpec::new("my-agent")
    .with_model("claude-sonnet")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("reminder");

let runtime = AgentRuntimeBuilder::new()
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model(
        "claude-sonnet",
        ModelSpec {
            id: "claude-sonnet".into(),
            provider: "anthropic".into(),
            model: "claude-3-7-sonnet-latest".into(),
        },
    )
    .with_agent_spec(agent_spec)
    .with_plugin("reminder", Arc::new(ReminderPlugin::new(rules)) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

该插件会在 `AfterToolExecute` 阶段检查工具名、参数和结果；一旦命中规则，就会调度 `AddContextMessage` 注入提示。

2. 用工具模式定义规则：

| Pattern | 匹配 |
|---------|------|
| `"Bash"` | 精确工具名 |
| `"*"` | 任意工具 |
| `"mcp__*"` | 所有 MCP 工具 |
| `"Bash(command ~ 'rm *')"` | 主参数 glob |
| `"Edit(file_path ~ '*.toml')"` | 命名字段 glob |

3. 配置 output 匹配：

`output` 可以是：

- `"any"`
- `{ "status": ..., "content": ... }`

`status` 支持：`"success"`、`"error"`、`"pending"`、`"any"`

`content` 支持两类：

- 文本 glob
- JSON 字段匹配

4. 选择消息注入目标：

| Target | 位置 |
|--------|------|
| `"system"` | system prompt 前部 |
| `"suffix_system"` | system prompt 后部 |
| `"session"` | session 级 system message |
| `"conversation"` | conversation 级 system message |

5. 用 cooldown 避免重复注入：

```rust,ignore
{
  "message": {
    "target": "system",
    "content": "Remember to be careful with file operations.",
    "cooldown_turns": 5
  }
}
```

6. 也可以从文件加载规则：

```rust,ignore
use awaken::ext_reminder::ReminderRulesConfig;

let config = ReminderRulesConfig::from_file("reminders.json")
    .expect("failed to load reminder config");
```

7. 在 agent spec 上激活插件：

```rust,ignore
let agent_spec = AgentSpec::new("my-agent")
    .with_model("claude-sonnet")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("reminder");
```

## 验证

1. 配一个容易命中的规则，例如 `"*"` + `"any"`
2. 运行 agent 并调用一个工具
3. 打开 debug tracing，应该看到 reminder 命中的日志
4. 确认下一轮推理 prompt 中出现了提醒消息

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| `InvalidPattern` | 工具模式写错 | 按 DSL 语法检查引号和通配规则 |
| `InvalidTarget` | 消息目标无效 | 只能用 `system` / `suffix_system` / `session` / `conversation` |
| `InvalidOutput` | output 结构无效 | 使用 `"any"` 或结构化对象 |
| `InvalidOp` | 字段匹配操作符未知 | 使用 `glob` / `exact` / `regex` / `not_*` 系列 |
| 规则从不触发 | 插件没激活 | 在 agent spec 上加 `with_hook_filter("reminder")` |
| 规则触发太频繁 | 没设置 cooldown | 设置正数 `cooldown_turns` |

## 相关示例

- `crates/awaken-ext-reminder/src/config.rs`

## 关键文件

| 路径 | 作用 |
|------|------|
| `crates/awaken-ext-reminder/src/lib.rs` | 模块根及公共再导出 |
| `crates/awaken-ext-reminder/src/config.rs` | `ReminderRulesConfig`、JSON 加载、`ReminderConfigKey` |
| `crates/awaken-ext-reminder/src/rule.rs` | `ReminderRule` 结构定义 |
| `crates/awaken-ext-reminder/src/output_matcher.rs` | `OutputMatcher`、`ContentMatcher`、状态/内容匹配逻辑 |
| `crates/awaken-ext-reminder/src/plugin/plugin.rs` | `ReminderPlugin` 注册（`AfterToolExecute` hook） |
| `crates/awaken-ext-reminder/src/plugin/hook.rs` | `ReminderHook`——每次工具调用的模式与输出评估 |
| `crates/awaken-tool-pattern/` | 共享 glob/regex 模式匹配库 |

## 相关

- [启用工具权限 HITL](./enable-tool-permission-hitl.md) -- 使用相同的工具模式 DSL
- [添加 Plugin](./add-a-plugin.md)
- [构建 Agent](./build-an-agent.md)
