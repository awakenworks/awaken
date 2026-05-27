---
title: "Provider 与 Model 配置"
description: "Awaken 把 provider 连接和 model 选择分开处理。本地 agent 执行会通过下面这条链路解析 provider 和 model："
---

Awaken 把 provider 连接和 model 选择分开处理。本地 agent 执行会通过下面这条链路解析 provider 和 model：

```text
AgentSpec.model_id
  -> ModelRegistry[model id]
  -> ModelSpec { provider_id, upstream_model, capabilities, pricing }
  -> ProviderRegistry[provider id]
  -> Arc<dyn LlmExecutor>
  -> InferenceRequest.upstream_model = upstream_model
```

Endpoint-backed agent 会跳过这条本地 provider/model 链路。它们会被解析成非本地 `ResolvedExecution`，并交给配置的 `ExecutionBackend` 执行。

## 术语

| 术语 | 类型 | 含义 |
|---|---|---|
| Agent model id | `AgentSpec.model_id` | Agent 使用的稳定模型注册表 ID，例如 `"default"` 或 `"research"`。 |
| Model spec | `ModelSpec` | 统一的可序列化 + 运行时类型。承载寻址（`id`、`provider_id`、`upstream_model`）、固有能力（`context_window`、`max_output_tokens`、`modalities`、`knowledge_cutoff`）和定价。直接存入托管配置，`ModelRegistry::get_model` 也直接返回它。 |
| Provider config | `ProviderSpec` | 可序列化 provider 配置，用于构造 executor。 |
| Provider executor | `Arc<dyn LlmExecutor>` | 真正执行推理的 provider client。 |
| 上游模型名 | `ModelSpec.upstream_model`、`InferenceRequest.upstream_model` | 最终发送给 provider API 的模型字符串。 |

最重要的区别是：

- `AgentSpec.model_id` 是注册表 ID。
- `ModelSpec.upstream_model` 和 `InferenceRequest.upstream_model` 是上游 provider 模型名。

## 代码构建路径

当应用在代码里构造 provider 时使用这条路径。

```rust
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::ModelSpec;
use awaken::{AgentRuntimeBuilder, AgentSpec};

let agent = AgentSpec::new("assistant")
    .with_model_id("default")
    .with_system_prompt("You are helpful.");

let runtime = AgentRuntimeBuilder::new()
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model(ModelSpec::new("default", "openai", "gpt-4o-mini"))
    .with_agent_spec(agent)
    .build()?;
```

`build()` 会解析每个已注册 agent，提前发现缺失 model、provider 或 plugin 的问题。

测试和本地开发可以用 `MockProviderProfile` 显式接入 mock provider，避免依赖全局环境变量切换执行器：

```rust
use awaken::{AgentRuntimeBuilder, MockProviderProfile};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = AgentRuntimeBuilder::new()
        .with_mock_provider_profile(MockProviderProfile::new("mock", "mock-model"))
        .build()?;

    let _runtime = runtime;
    Ok(())
}
```

## 托管配置路径

当服务端通过 `ConfigStore` 管理动态配置时使用这条路径。

托管配置按 namespace 存储：

| Namespace | 可序列化类型 |
|---|---|
| `providers` | `ProviderSpec` |
| `models` | `ModelSpec` |
| `agents` | `AgentSpec` |
| `mcp-servers` | `McpServerSpec` |

配置示例：

```json
{
  "id": "openai",
  "adapter": "openai",
  "api_key": "sk-...",
  "base_url": null,
  "timeout_secs": 300,
  "adapter_options": {}
}
```

```json
{
  "id": "default",
  "provider_id": "openai",
  "upstream_model": "gpt-4o-mini",
  "context_window": 128000,
  "max_output_tokens": 16384,
  "modalities": { "input": ["text", "image"], "output": ["text"] },
  "knowledge_cutoff": "2024-10",
  "input_token_price_per_million_usd": 0.15,
  "output_token_price_per_million_usd": 0.60
}
```

```json
{
  "id": "assistant",
  "model_id": "default",
  "system_prompt": "You are helpful."
}
```

### ProviderSpec 字段

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `id` | `String` | 必填 | provider 标识，被 `ModelSpec.provider_id` 引用 |
| `adapter` | `String` | 必填 | GenAI 适配器类型（如 `"openai"`、`"anthropic"`、`"ollama"`） |
| `api_key` | `Option<RedactedString>` | `None` | 用 `RedactedString` 包裹，`Debug`/`Display` 自动遮蔽。线缆格式是普通 JSON 字符串。空字符串输入会反序列化为 `None`，便于在更新时省略字段以保留已有 key |
| `base_url` | `Option<String>` | `None` | 代理或自托管部署的 base URL 覆盖。空字符串输入反序列化为 `None` |
| `timeout_secs` | `u64` | `300` | 请求超时（秒） |
| `adapter_options` | `BTreeMap<String, Value>` | `{}` | 适配器专属、非密的扩展选项。OpenAI 兼容适配器识别 `headers`（一个 string→string 的对象，作为默认请求头加进去）。`model_discovery_schema`（`"openai"`/`"openai-compatible"` 或 `"gemini"`）让自定义 adapter 按该 schema 启用 `/models` 能力发现（见“模型能力来源”）。`model_discovery_auth`（`"bearer"`、`"x-goog-api-key"` 或 `"none"`；别名包括 `"authorization-bearer"`、`"google-api-key"`、`"gemini-api-key"`、`"no-auth"`、`"disabled"`）只覆盖 discovery 请求的鉴权 header；默认 OpenAI-compatible discovery 使用 `Authorization: Bearer`，Gemini discovery 使用 `x-goog-api-key`。配置校验会拒绝非法的 `model_discovery_schema` / `model_discovery_auth` 值。Schema 接受未知 key，但构建时会被忽略。秘密值必须用 `api_key`，不要塞到这里 |

为兼容已存储配置，`ProviderSpec` 反序列化会忽略未知顶层字段。配置写入和
validate surface 会调用 `validate_provider_spec` 并拒绝未知字段，避免新记录
持久化会被静默忽略的设置。Model spec 可使用 `validate_model_spec` 进行同样的
canonical 校验；校验 `Vec<ModelSpec>` 集合时使用 `validate_unique_model_ids`
确保 id 不重复。

带自定义 header 的示例：

```json
{
  "id": "bigmodel",
  "adapter": "openai",
  "api_key": "<redacted>",
  "base_url": "https://open.bigmodel.cn/api/paas/v4",
  "adapter_options": {
    "headers": {
      "X-Tenant-Id": "team-42"
    }
  }
}
```

服务端会把这些文档编译成运行时注册表：

```text
ProviderSpec -> ProviderExecutorFactory -> Arc<dyn LlmExecutor>
ModelSpec    -> ModelRegistry（直接存储，无 spec/runtime 拆分）
AgentSpec    -> AgentSpecRegistry
```

配置文档只使用 canonical 字段名：agent 使用 `model_id`，model spec 使用
`provider_id` 和 `upstream_model`。

候选注册表会先验证，再替换 runtime 的活动快照。验证失败时，本次配置写入会回滚。

### 模型能力来源

解析时 capability 字段按以下优先级合并：

1. `ModelSpec` 中显式保存的字段。
2. 发布 registry 时从 provider `/models` 发现的模型元数据。
3. 常见模型系列的内置静态启发式默认值。

静态启发式只作为保守 metadata。运行时输入模态拦截和 knowledge cutoff
context 自动注入，只信任显式 `ModelSpec` 配置或 provider discovery。

Provider discovery 的覆盖范围取决于 adapter。只有具备已知 `/models` schema 的
adapter 才会被探测：`openai`/`openrouter`（OpenAI 兼容）与 `gemini`/`google`
（Gemini）。未知或自定义 adapter 不会被默默当成 OpenAI 兼容；要发现自定义的
OpenAI/Gemini 兼容网关，请通过 `adapter_options.model_discovery_schema` 显式
opt-in。Discovery 鉴权与 adapter 正交：默认按声明的 discovery schema 选择
header，也可以用 `adapter_options.model_discovery_auth` 覆盖，以适配 header 约定
不同的网关。Discovery 还会复用 `adapter_options.headers` 中的非鉴权 header，
让 tenant/routing header 传到 `/models`；其中的 `Authorization` 和 `x-goog-api-key`
在 discovery 中会被忽略，避免覆盖 `model_discovery_auth` 或在禁用鉴权时泄漏。
Gemini/Google discovery 当前只补 token limit；如果需要让 `modalities`
或 `knowledge_cutoff` 驱动运行时 guard / context 注入，请显式配置这些字段。
Vertex 模型仍可能获得静态启发式 metadata，但除非未来 adapter 提供完整的
discovery URL/auth 实现，否则不启用 Vertex provider discovery。

显式 `ModelSpec.knowledge_cutoff` 在反序列化时校验：必须是 ISO `YYYY-MM` 或
`YYYY-MM-DD` 日期，因此格式非法或被注入的值会在进入已解析模型或 knowledge-cutoff
system context 之前被拒绝。

当 `ModelSpec.modalities.input` 来自显式配置或 provider discovery 且非空时，
runtime 会在 provider 调用前拒绝不支持的请求内容块。system、user、assistant
和 tool-result content 中的文本块不消耗 `text` modality；`modalities.input`
只限制模型必须读取的 media（`image`、`audio`、`video`，以及能识别为 `pdf` 的
document）。tool call 与 reasoning/thinking 是协议结构块、不是模型输入模态，因此
不受 modality 限制；但 `ToolResult.content` 中内嵌的 media *会*按
`modalities.input` 校验，因为模型仍要读取这些 media：guard 会递归进入 tool
result 并校验其中每个 media 块。当 `knowledge_cutoff` 来自显式配置或 provider discovery 时，resolver
会安装 `knowledge_cutoff_context`，每个 inference boundary 注入一条 system
context。可在 agent 上关闭：

```json
{
  "sections": {
    "knowledge_cutoff_context": { "enabled": false }
  }
}
```

Provider discovery 是 provider definition 维度的完整 snapshot。如果后续发布时
无法刷新 `/models`，会保留同一 provider signature 的上一份成功 snapshot；provider
endpoint/options 变化会使该缓存失效。

Model pool member 会使用和单模型相同的 capability resolution 与 modality guard。
只有当所有 member 暴露同一个可信 cutoff 时，pool 才会安装 pool-level
knowledge-cutoff context。

## 从旧 model 字段迁移

这个版本会有意拒绝旧的 provider/model 字段名，而不是静默归一化。升级前需要更新
已保存配置、测试 fixture 和客户端：

| 旧字段或旧形状 | 新 canonical 形式 |
|---|---|
| `AgentSpec.model` | `AgentSpec.model_id` |
| `ModelBindingSpec` 类型 | `ModelSpec`（统一类型，承载 capabilities + 定价）|
| `ModelBindingSpec.provider` | `ModelSpec.provider_id` |
| `ModelBindingSpec.model` | `ModelSpec.upstream_model` |
| 运行时 `ModelBinding`（仅 provider_id + upstream_model） | `ModelSpec`（`ModelRegistry::get_model` 完整返回）|
| `with_model_binding(id, binding)` builder | `with_model(spec)`（id 取自 `spec.id`）|
| `validate_model_binding_spec` | `validate_model_spec` |
| `ProviderRemovalPolicy::CascadeUnusedModelBindings` | `CascadeUnusedModels` |
| Rust 内部字段 `model_bindings: Vec<…>` | `models: Vec<ModelSpec>`（wire JSON key 始终是 `models`，未变）|
| `InferenceOverride.model` | `InferenceOverride.upstream_model` |
| `AgentSystemConfig.models` 使用以 model id 为 key 的对象 | `AgentSystemConfig.models` 使用显式包含 `id` 的 `ModelSpec` 列表 |

升级检查：

```bash
rg '"model"\s*:|"provider"\s*:' config/ docs/ tests/
```

每个匹配项都需要人工确认。某些外部协议 payload 可能仍然有名为 `model` 的字段；
Awaken 托管配置不应再使用这些旧字段。

## Provider 密钥（配置 API 视角）

配置 API 把 `api_key` 当作只写字段：

- list/get 响应中 `api_key` 被替换为 `has_api_key: true|false`；
- `PUT` 时省略 `api_key` 会保留已有 key；
- `PUT` `api_key: null` 或 `""` 会清空 key。

进程内的存储类型是 `RedactedString`（详见
[配置参考 — 凭据处理](/awaken/zh-cn/reference/config/#凭据处理)）。

## Runtime 快照行为

运行时不会在每个推理步骤直接读取 `ConfigStore`。托管配置变更会先编译成新的 registry 快照：

```text
ConfigStore change -> compile RegistrySet -> validate -> replace runtime snapshot
```

新 run 使用最新发布的快照。已经开始的 run 保持启动时绑定的快照。

## Runtime Registry 更新

对在代码中管理 provider executor 的应用，`RegistryHandle` 暴露 provider 更新操作：

这些操作只更新当前内存中的 runtime 快照，不会写入 `ConfigStore`；下一次
managed config 发布可能会用从 `ConfigStore` 编译出的状态替换该快照。

| 方法 | 行为 |
|---|---|
| `register_provider(id, executor)` | 新增 provider，并发布已验证的快照 |
| `replace_provider(id, executor)` | 替换已有 provider executor，不重建无关注册表 |
| `preview_remove_provider(id)` | 只返回依赖的 model 和 agent id，不修改快照 |
| `remove_provider(id, policy)` | 删除 provider 前检查依赖它的 model 和 agent |

`ProviderRemovalPolicy::BlockIfReferenced` 会在仍有 model 指向该 provider
时拒绝删除。`ProviderRemovalPolicy::CascadeUnusedModels` 会同时删除指向
该 provider 的 model，但前提是没有 agent 使用这些 model。
`ProviderRemovalPreview` 会返回 provider id、引用它的 `model_ids`、受影响的
`agent_ids`，以及每个策略当前是否允许。成功时，`ProviderRemovalImpact` 会
返回 provider id 和被删除的 model id；存在依赖冲突时，
`RegistryUpdateError::ProviderInUse` 会包含相关 model 和 agent id。

当配置来源已经生成完整的 agents、models 和 providers 替换集合时，使用
`rebuild_agent_model_provider_registries(base, update)`。它会保留 base
注册表中的 tools、plugins 和 execution backends，只替换 agents、models 和
providers，并在返回前验证候选注册表。

诊断函数可以在发布快照前使用：

| 函数 | 报告内容 |
|---|---|
| `diagnose_registry_set(registries)` | 缺失的 model、provider、plugin 和 delegate agent |
| `diagnose_registry_set_serializable(registries)` | 同样的 diagnostics，但输出带 `code`、`severity`、`resource`、可选 `depends_on` 和 `message` 的稳定 payload |
| `validate_registry_set(registries)` | 以错误结果返回相同检查 |
| `diagnose_agent_spec(registries, spec)` | 单个 agent 相对当前注册表的问题 |
| `validate_agent_spec(registries, spec)` | 以错误结果返回相同 agent 检查 |

## 推理覆盖

`InferenceOverride.upstream_model` 使用当前已解析 provider 的上游模型名。它不会重新解析 `AgentSpec.model_id`，也不会切换 provider executor；对于 model pool backed agent 会被拒绝，因为 pool 会在内部选择成员。

执行时，override 会应用到 `InferenceRequest.upstream_model`；executor 应把这个字段作为上游模型的唯一来源。其余 override 字段保留生成参数。

同 provider 内切换模型时可以使用 model override：

```rust
use awaken::contract::inference::InferenceOverride;

let overrides = InferenceOverride {
    upstream_model: Some("gpt-4o".into()),
    ..Default::default()
};
```

如果需要切换到另一个模型或 provider，请使用 `ModelPoolSpec`、不同的 `AgentSpec.model_id` 或 agent handoff。

## Retry 与 model pool

每个 agent 通过 `RetryConfigKey` 读取 `"retry"` section。缺失 section 时使用 `LlmRetryPolicy::default()`。解析阶段会在最终 policy 配置了 retry 时，用 `RetryingExecutor` 包装 provider executor。将 `max_retries` 设为 `0` 可以禁用该包装。

Provider factory 只返回 provider executor；retry 由解析流水线添加，不隐藏在 provider 构造里。

非流式执行中，retry 作用于完整推理调用。流式执行中，retry 只作用于打开 stream 的阶段。stream 已经开始后，如果后续 stream item 报错，会直接向上返回，因为重试会导致已经发出的 delta 重复。模型故障切换由 `ModelPoolSpec` 配置。

## 相关

- [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/)
- [配置](/awaken/zh-cn/reference/config/)
- [智能体解析](/awaken/zh-cn/explanation/agent-resolution/)
