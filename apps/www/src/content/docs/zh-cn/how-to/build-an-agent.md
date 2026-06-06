---
title: "构建 Agent"
description: "当你需要把 agent spec、tools、provider 和持久化组装成一个可运行的 AgentRuntime 时，使用本页。"
---

当你需要把 agent spec、tools、provider 和持久化组装成一个可运行的 `AgentRuntime` 时，使用本页。

## 目的

本页定义 runtime 边界：agent 能执行什么、能调用哪个模型、state 存在哪里，以及哪些行为留给后续配置调优。把这些选择放进 builder，可以在启动时快速失败，而不是让用户在 run 中途才发现 tool 或 provider 缺失。

## 前置条件

- 已在 `Cargo.toml` 中加入 `awaken`
- 已有一个 `LlmExecutor` 实现
- 了解 `AgentSpec` 和 `AgentRuntimeBuilder`

## 步骤

1. 定义 agent spec：

```rust
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::ModelSpec;
use awaken::{AgentSpec, AgentRuntimeBuilder};

let spec = AgentSpec::new("assistant")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a helpful assistant.")
    .with_max_rounds(10);
```

2. 注册 tools：

```rust
use std::sync::Arc;

let builder = AgentRuntimeBuilder::new()
    .with_agent_spec(spec)
    .with_tool("search", Arc::new(SearchTool))
    .with_tool("calculator", Arc::new(CalculatorTool));
```

每个 `with_*` 调用都把东西注册进 runtime 的五张注册表之一(agents、tools、models、providers、plugins);agent 在调用时按 id 对照这些表解析,server 模式下同样这几张表由发布的配置填充。见 [智能体解析](/awaken/zh-cn/explanation/agent-resolution/)。

3. 注册 provider 和 model：

```rust
let builder = builder
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model(ModelSpec::new("claude-sonnet", "anthropic", "claude-sonnet-4-20250514"));
```

如果要接自定义模型客户端，实现 `LlmExecutor` 后用 `with_provider` 注册。若走
server managed config，则提供 `ProviderExecutorFactory`，把 `ProviderSpec`
物化成 live executor。不要把 retry 或 provider failover 藏在 provider 实现里：
retry 在解析阶段包裹 executor；跨 provider failover 应使用 `ModelPoolSpec`。

4. 挂接持久化：

```rust
use awaken::contract::commit_coordinator::CommitCoordinator;
use awaken::stores::{InMemoryStore, MemoryCommitCoordinator};

let store = Arc::new(InMemoryStore::new());
let coordinator = MemoryCommitCoordinator::wrap(store) as Arc<dyn CommitCoordinator>;
let builder = builder.with_commit_coordinator(coordinator);
```

5. 构建并校验：

```rust
let runtime = builder.build()?;
```

`build()` 会在启动时就解析并校验所有注册项，提前发现缺失的 model、provider 或 plugin。

6. 通过配置调优 agent 行为：

`AgentSpec` 就是 agent 的运行时配置对象。下面这些字段和 section 与
`/v1/config/agents`、admin console 页面编辑的是同一份数据：

```rust
use serde_json::json;

let mut spec = AgentSpec::new("assistant")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a careful coding assistant.")
    .with_hook_filter("reminder")
    .with_section("reminder", json!({
        "rules": [{
            "tool": "*",
            "output": "any",
            "message": {
                "target": "suffix_system",
                "content": "Prefer verifying code changes before final answers.",
                "cooldown_turns": 3
            }
        }]
    }));
spec.plugin_ids.push("reminder".into());
```

基础 prompt 使用 `system_prompt`；需要页面可配置、可校验、可运行时生效的行为，
放到 `reminder`、`generative-ui`、`permission`、`deferred_tools` 等插件
section 中。后续 prompt 语义 hook 也应沿用同样的类型化 section 模式。

7. 执行一次 run：

```rust
use awaken::RunActivation;

let request = RunActivation::new("thread-1", vec![user_message])
    .with_agent_id("assistant");

// 当调用方需要流式事件时，使用 runtime.run(..., sink)。
let result = runtime.run_to_completion(request).await?;
```

8. 只在确实需要时接入后台工作。

当 tool 启动的工作可能晚于当前 model step 完成，或子 agent 需要保留 inbox 接收后续消息时，使用 background extension。它比直接 `tokio::spawn` 更好，因为每个任务都有稳定 ID、cancellation token、持久状态、父级 lineage 和可让 loop 恢复的 inbox 事件。

```rust
use std::sync::Arc;
use awaken::extensions::background::{
    BackgroundTaskManager, BackgroundTaskPlugin, SendMessageTool,
};

let background = Arc::new(BackgroundTaskManager::new());
let background_plugin = Arc::new(BackgroundTaskPlugin::new(background.clone()));

let builder = builder
    .with_plugin("background_tasks", background_plugin)
    // 当 host 提供 DurableMessageSink 时，再注册跨 thread / 跨进程通信工具。
    .with_tool("send_message", Arc::new(SendMessageTool::new(background, durable_sink)));
```

在 tool 内，普通后台工作使用 `BackgroundTaskManager::spawn(...)`；后台子 agent 使用 `spawn_agent_with_context(...)`，这样它会有自己的 inbox。state 传递仍需显式：任务状态记录在 `BackgroundTaskStateKey`；父 ↔ 子的业务 state 使用 [在工具里调用 Sub-Agent](/awaken/zh-cn/how-to/invoke-sub-agent-from-tool/) 中的类型化 `StateKey` seed/export 规则。

## 验证

如果启用了 server，可访问 `/health`；否则直接检查 `AgentRunResult` 是否成功完成。

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| `BuildError::ValidationFailed` | spec 引用了未注册的 model/provider | 在 `build()` 前补齐注册 |
| `BuildError::State` | 多个插件重复注册同一状态键 | 保证状态键只注册一次 |
| 运行期 `RuntimeError` | provider 推理失败 | 检查凭据和模型 ID |

## 相关示例

`examples/src/research/`

## 代码参考

- `crates/awaken-doctest/examples/http_app_builder.rs` —— canonical `AgentRuntime` → `Mailbox` → `ServerState` wiring。
- `crates/awaken/tests/readme_quickstart.rs` —— README 路径使用的小型自定义 `LlmExecutor`。
- `crates/awaken-server/tests/config_api.rs` 与 `crates/awaken-server/tests/config_backends.rs` —— `ProviderExecutorFactory`、managed provider config 和 model-pool 覆盖。
- `crates/awaken-runtime/tests/background_task_lifecycle.rs` —— background task、background agent、inbox、cancellation 和状态传递。
- `crates/awaken-runtime/tests/child_agent_seed.rs` —— parent → child state seed 与 child → parent state export 规则。

## 关键文件

- `crates/awaken-runtime/src/builder.rs`
- `crates/awaken-runtime-contract/src/registry_spec.rs`
- `crates/awaken-runtime/src/runtime/agent_runtime/mod.rs`

## 相关

- [添加 Tool](/awaken/zh-cn/how-to/add-a-tool/)
- [添加 Plugin](/awaken/zh-cn/how-to/add-a-plugin/)
- [使用文件存储](/awaken/zh-cn/how-to/use-file-store/)
- [通过 SSE 暴露 HTTP](/awaken/zh-cn/how-to/expose-http-sse/)
