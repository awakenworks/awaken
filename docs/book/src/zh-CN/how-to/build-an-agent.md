# 构建 Agent

当你需要把 agent spec、tools、provider 和持久化组装成一个可运行的 `AgentRuntime` 时，使用本页。

## 前置条件

- 已在 `Cargo.toml` 中加入 `awaken`
- 已有一个 `LlmExecutor` 实现
- 了解 `AgentSpec` 和 `AgentRuntimeBuilder`

## 步骤

1. 定义 agent spec：

```rust,ignore
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::{AgentSpec, AgentRuntimeBuilder};

let spec = AgentSpec::new("assistant")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a helpful assistant.")
    .with_max_rounds(10);
```

2. 注册 tools：

```rust,ignore
use std::sync::Arc;

let builder = AgentRuntimeBuilder::new()
    .with_agent_spec(spec)
    .with_tool("search", Arc::new(SearchTool))
    .with_tool("calculator", Arc::new(CalculatorTool));
```

3. 注册 provider 和 model：

```rust,ignore
let builder = builder
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model_binding("claude-sonnet", ModelBinding {
        provider_id: "anthropic".into(),
        upstream_model: "claude-sonnet-4-20250514".into(),
    });
```

4. 挂接持久化：

```rust,ignore
use awaken::stores::InMemoryStore;

let store = Arc::new(InMemoryStore::new());
let builder = builder.with_thread_run_store(store);
```

5. 构建并校验：

```rust,ignore
let runtime = builder.build()?;
```

`build()` 会在启动时就解析并校验所有注册项，提前发现缺失的 model、provider 或 plugin。

6. 执行一次 run：

```rust,ignore
use std::sync::Arc;
use awaken::RunRequest;
use awaken::contract::event_sink::VecEventSink;

let request = RunRequest::new("thread-1", vec![user_message])
    .with_agent_id("assistant");

let sink = Arc::new(VecEventSink::new());
let handle = runtime.run(request, sink.clone()).await?;
```

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

## 关键文件

- `crates/awaken-runtime/src/builder.rs`
- `crates/awaken-contract/src/registry_spec.rs`
- `crates/awaken-runtime/src/runtime/agent_runtime/mod.rs`

## 相关

- [添加 Tool](./add-a-tool.md)
- [添加 Plugin](./add-a-plugin.md)
- [使用文件存储](./use-file-store.md)
- [通过 SSE 暴露 HTTP](./expose-http-sse.md)
