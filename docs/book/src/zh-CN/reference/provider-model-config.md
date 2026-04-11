# Provider 与 Model 配置

Awaken 把 provider 连接和 model 选择分开处理。运行时总是通过下面这条链路解析 agent：

```text
AgentSpec.model_id
  -> ModelRegistry[model id]
  -> ModelBinding { provider_id, upstream_model }
  -> ProviderRegistry[provider id]
  -> Arc<dyn LlmExecutor>
  -> InferenceRequest.upstream_model = upstream_model
```

## 术语

| 术语 | 类型 | 含义 |
|---|---|---|
| Agent model id | `AgentSpec.model_id` | Agent 使用的稳定模型注册表 ID，例如 `"default"` 或 `"research"`。 |
| Runtime model binding | `ModelBinding` | 运行时映射：model id -> provider id + 上游模型名。 |
| Config model binding | `ModelBindingSpec` | 托管配置中的可序列化模型配置，发布时会编译成 `ModelBinding`。 |
| Provider config | `ProviderSpec` | 可序列化 provider 配置，用于构造 executor。 |
| Provider executor | `Arc<dyn LlmExecutor>` | 真正执行推理的 provider client。 |
| 上游模型名 | `ModelBinding.upstream_model`、`ModelBindingSpec.upstream_model`、`InferenceRequest.upstream_model` | 最终发送给 provider API 的模型字符串。 |

最重要的区别是：

- `AgentSpec.model_id` 是注册表 ID。
- `ModelBindingSpec.upstream_model`、`ModelBinding.upstream_model`、`InferenceRequest.upstream_model` 是上游 provider 模型名。

## 代码构建路径

当应用在代码里构造 provider 时使用这条路径。

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::{AgentRuntimeBuilder, AgentSpec};

let agent = AgentSpec::new("assistant")
    .with_model_id("default")
    .with_system_prompt("You are helpful.");

let runtime = AgentRuntimeBuilder::new()
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model_binding("default", ModelBinding {
        provider_id: "openai".into(),
        upstream_model: "gpt-4o-mini".into(),
    })
    .with_agent_spec(agent)
    .build()?;
```

`build()` 会解析每个已注册 agent，提前发现缺失 model、provider 或 plugin 的问题。

## 托管配置路径

当服务端通过 `ConfigStore` 管理动态配置时使用这条路径。

托管配置按 namespace 存储：

| Namespace | 可序列化类型 |
|---|---|
| `providers` | `ProviderSpec` |
| `models` | `ModelBindingSpec` |
| `agents` | `AgentSpec` |
| `mcp-servers` | `McpServerSpec` |

配置示例：

```json
{
  "id": "openai",
  "adapter": "openai",
  "api_key": "sk-...",
  "base_url": null,
  "timeout_secs": 300
}
```

```json
{
  "id": "default",
  "provider_id": "openai",
  "upstream_model": "gpt-4o-mini"
}
```

```json
{
  "id": "assistant",
  "model_id": "default",
  "system_prompt": "You are helpful."
}
```

服务端会把这些文档编译成运行时注册表：

```text
ProviderSpec -> ProviderExecutorFactory -> Arc<dyn LlmExecutor>
ModelBindingSpec    -> ModelBinding
AgentSpec    -> AgentSpecRegistry
```

配置文档只使用 canonical 字段名：agent 使用 `model_id`，model binding 使用
`provider_id` 和 `upstream_model`，retry 或 inference override 使用
`fallback_upstream_models`。

候选注册表会先验证，再替换 runtime 的活动快照。验证失败时，本次配置写入会回滚。

## 从旧 model 字段迁移

这个版本会有意拒绝旧的 provider/model 字段名，而不是静默归一化。升级前需要更新
已保存配置、测试 fixture 和客户端：

| 旧字段或旧形状 | 新 canonical 形式 |
|---|---|
| `AgentSpec.model` | `AgentSpec.model_id` |
| `ModelBindingSpec.provider` | `ModelBindingSpec.provider_id` |
| `ModelBindingSpec.model` | `ModelBindingSpec.upstream_model` |
| `InferenceOverride.model` | `InferenceOverride.upstream_model` |
| `fallback_models` | `fallback_upstream_models` |
| `AgentSystemConfig.models` 使用以 model id 为 key 的对象 | `AgentSystemConfig.models` 使用显式包含 `id` 的 `ModelBindingSpec` 列表 |

升级检查：

```bash
rg '"model"\s*:|"provider"\s*:|fallback_models' config/ docs/ tests/
```

每个匹配项都需要人工确认。某些外部协议 payload 可能仍然有名为 `model` 的字段；
Awaken 托管配置不应再使用这些旧字段。

## Provider 密钥

Provider API key 通过配置 API 写入后不会明文返回：

- 响应会移除 `api_key`；
- 已保存 key 时响应包含 `has_api_key: true`；
- 更新 provider 时省略 `api_key` 会保留已有 key；
- 把 `api_key` 设为 `null` 或空字符串会清除 key。

## Runtime 快照行为

运行时不会在每个推理步骤直接读取 `ConfigStore`。托管配置变更会先编译成新的 registry 快照：

```text
ConfigStore change -> compile RegistrySet -> validate -> replace runtime snapshot
```

新 run 使用最新发布的快照。已经开始的 run 保持启动时绑定的快照。

## 推理覆盖

`InferenceOverride.upstream_model` 和 `InferenceOverride.fallback_upstream_models` 使用的是当前已解析 provider 的上游模型名。它们不会重新解析 `AgentSpec.model_id`，也不会切换 provider executor。

执行时，primary override 会应用到 `InferenceRequest.upstream_model`；executor 应把这个字段作为 primary 上游模型的唯一来源。其余 override 字段保留生成参数和 fallback upstream models。

同 provider 内切换模型时可以使用 model override：

```rust,ignore
use awaken::contract::inference::InferenceOverride;

let overrides = InferenceOverride {
    upstream_model: Some("gpt-4o".into()),
    fallback_upstream_models: Some(vec!["gpt-4o-mini".into()]),
    ..Default::default()
};
```

如果需要切换到另一个 provider，请使用不同的 `AgentSpec.model_id` 或 agent handoff。

## Retry 与 fallback

每个 agent 通过 `RetryConfigKey` 读取 `"retry"` section。缺失 section 时使用 `LlmRetryPolicy::default()`。解析阶段会在最终 policy 配置了 retry 或 fallback upstream model 时，用 `RetryingExecutor` 包装 provider executor。将 `max_retries` 设为 `0` 且保持 `fallback_upstream_models` 为空可以禁用该包装。

Provider factory 只返回 provider executor；retry 由解析流水线添加，不隐藏在 provider 构造里。

非流式执行中，retry 与 fallback 作用于完整推理调用。流式执行中，retry 与 fallback 只作用于打开 stream 的阶段。stream 已经开始后，如果后续 stream item 报错，会直接向上返回，因为重试会导致已经发出的 delta 重复。
