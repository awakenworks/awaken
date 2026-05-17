# 配置

## AgentSpec

`AgentSpec` 是可序列化的 agent 定义。它既可以从 JSON / YAML 加载，也可以用 builder 方法在代码里构造。

```rust,ignore
pub struct AgentSpec {
    pub id: String,
    pub model_id: String,                            // model registry id
    pub system_prompt: String,
    pub max_rounds: usize,
    pub max_continuation_retries: usize,
    pub context_policy: Option<ContextWindowPolicy>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub plugin_ids: Vec<String>,
    pub active_hook_filter: HashSet<String>,
    pub allowed_tools: Option<Vec<String>>,           // 字面量 tool id
    pub excluded_tools: Option<Vec<String>>,          // 字面量 tool id
    pub allowed_tool_patterns: Option<Vec<String>>,   // glob 模式
    pub excluded_tool_patterns: Option<Vec<String>>,  // glob 模式
    pub endpoint: Option<RemoteEndpoint>,
    pub delegates: Vec<String>,
    pub sections: HashMap<String, Value>,
    pub registry: Option<String>,
}
```

**Crate 路径：** `awaken::registry_spec::AgentSpec`（在 `awaken::AgentSpec` 重新导出）

### Builder 方法

```rust,ignore
AgentSpec::new(id) -> Self
    .with_model_id(model_id) -> Self
    .with_system_prompt(prompt) -> Self
    .with_max_rounds(n) -> Self
    .with_reasoning_effort(effort) -> Self
    .with_hook_filter(plugin_id) -> Self
    .with_config::<K>(config) -> Result<Self, StateError>
    .with_delegate(agent_id) -> Self
    .with_endpoint(endpoint) -> Self
    .with_section(key, value: Value) -> Self
```

### 类型化配置访问

```rust,ignore
fn config<K: PluginConfigKey>(&self) -> Result<K::Config, StateError>
fn set_config<K: PluginConfigKey>(&mut self, config: K::Config) -> Result<(), StateError>
```

### 运行时管理的插件配置

无论 agent spec 是在 Rust 中构造、从 JSON/YAML 加载，还是通过运行时配置 API
保存，插件配置的唯一来源都是 `AgentSpec.sections`。插件通过同一个
`PluginConfigKey` 声明类型化 section，通过 `Plugin::config_schemas()` 暴露
JSON Schema，并在解析或 phase hook 中通过 `agent_spec.config::<K>()` 读取。

这是 agent 优化能力的统一控制面。model 与 provider 选择、基础 prompt、
reminder 规则、生成式 UI prompt 指令、permission、上下文窗口、重试策略以及
deferred-tool 策略都应是可校验、可运行时变更的数据。

| 调优面 | 已实现的配置入口 |
|---|---|
| 基础 prompt | `agents` 配置命名空间中的 `AgentSpec.system_prompt` |
| model 选择 | `AgentSpec.model_id`，通过 `/v1/config/models` 解析 |
| provider 端点与 OpenAI 兼容路由 | `/v1/config/providers`（`adapter`、`base_url`、认证、超时） |
| 上下文预算与 prompt cache | `AgentSpec.context_policy` |
| reasoning effort | `AgentSpec.reasoning_effort` |
| 重试与 fallback models | `AgentSpec.sections["retry"]` |
| system reminder 与 prompt 上下文注入 | `AgentSpec.sections["reminder"]`，通过 `ReminderConfigKey` 读取 |
| Generative UI prompt 指令 | `AgentSpec.sections["generative-ui"]`，通过 `A2uiPromptConfigKey` 读取 |
| permission 策略 | `AgentSpec.sections["permission"]` |
| deferred tool loading | `AgentSpec.sections["deferred_tools"]` |

prompt 语义 hook 当前还不是内置插件。后续加入时应沿用同一路径：声明类型化
config key、暴露 schema、在 admin console 中渲染，并在 hook 中读取 resolved
config。

当 `awaken-server` 挂接 `ConfigStore` 与 config runtime manager 后，
`/v1/capabilities` 会返回 `plugins[].config_schemas`。admin console 在 agent
编辑页渲染这些 schema，并把值保存回 `AgentSpec.sections[schema.key]`。
写入通过校验并发布新的 registry snapshot 后，会对新的 run 生效。如果插件
未列在 `plugin_ids` 中，section 会继续保存，但插件不会被加载，因此对应 hook、
tool 和 request transform 都不会运行。

admin surface 还提供只读预检端点，方便集成方在执行破坏性操作前解释影响面：

| 端点 | 用途 |
|---|---|
| `GET /v1/config/providers/:id/removal-preview` | 返回引用该 provider 的 `model_ids`、受影响的 `agent_ids`，以及 strict / cascade 删除策略是否允许 |
| `GET /v1/config/diagnostics` | 以稳定可序列化结构返回 registry diagnostics，包含 `code`、`severity`、`resource`、可选 `depends_on` 和 `message` |

starter runtime 当前暴露的可配置插件 section：

| Plugin ID | Section key | Admin editor |
|---|---|---|
| `permission` | `permission` | 专用权限规则编辑器 |
| `reminder` | `reminder` | 专用 reminder 规则编辑器 |
| `generative-ui` | `generative-ui` | 专用 A2UI prompt/catalog 编辑器 |
| `ext-deferred-tools` | `deferred_tools` | 通用 JSON Schema 表单 |

## 工具目录（Tool catalog）

每个 agent 的工具目录由四个字段组成：字面量与 glob 模式互相独立，可以自由组合。

```yaml
allowed_tools:          [Bash, Read]    # 字面量 tool id
allowed_tool_patterns:  ["mcp:*"]       # glob 模式
excluded_tools:         []              # 字面量 tool id
excluded_tool_patterns: []              # glob 模式
```

运行时计算：

```text
allow_set    = allowed_tools ∪ {id | ∃p ∈ allowed_tool_patterns. matches(p, id)}
exclude_set  = excluded_tools ∪ {id | ∃p ∈ excluded_tool_patterns. matches(p, id)}
final_set    = allow_set − exclude_set
```

拒绝始终优先：只要工具命中 `excluded_*`，即使同时出现在 `allowed_*` 中也会被剔除。

### 模式语法

锚定全串匹配。`*` 匹配任意字符序列（包含 `/`、`:`、`_`）。`\` 转义下一字符 ——
`\*` 表示字面 `*`，`\\` 表示字面 `\`。不支持 `?`、字符类、`{…}` 与 `!` 取反。

### "允许全部" 简写

通配模式就是单独的 `*`：

```yaml
allowed_tool_patterns: ["*"]
```

### 默认行为（向后兼容）

如果 agent spec **既没有** `allowed_tools` 也没有 `allowed_tool_patterns`，
运行时会在反序列化阶段注入 `allowed_tool_patterns: ["*"]`，保留旧版"未配置 =
允许全部"的语义。任何显式值（包括空列表）都会抑制注入 ——
`allowed_tools: []` 且未设置 `allowed_tool_patterns` 表示"不允许任何工具"。

### 校验

| 条件                                                | 影响                              |
|-----------------------------------------------------|-----------------------------------|
| `allowed_tools` / `excluded_tools` 中包含 `*`       | 加载时记录 warning；条目被当作字面量处理（无法匹配任何东西）。 |
| `*_tool_patterns` 中的模式语法非法                  | 加载时报 **error**；spec 被拒绝。 |
| 模式没有匹配任何已注册工具                          | 解析阶段记录 warning。            |
| 目录条目形如 `name(args)`                           | 解析阶段记录 warning；应放到 `sections["permission"]`。 |
| permission 规则引用被目录过滤掉的工具                | 解析阶段记录 warning。            |

### 从旧的单字段形态迁移

旧版的 `allowed_tools: ["mcp:*"]`（在字面量字段中放入含 `*` 的条目）此前不会
匹配任何东西。新运行时在加载时记录 warning，并继续将其按字面量处理。要让它
作为 glob 生效，请把条目移到 `allowed_tool_patterns`。admin console 已自动
写入新形态。

## AgentSpecPatch

`AgentSpecPatch` 是内置 agent 定制用的字段级覆盖类型。所有字段都是可选的：
缺失字段继承基础 `AgentSpec`，出现的字段通过 `merge_agent_spec(base, patch)`
覆盖基础值。对于 `AgentSpec` 里的可选字段，JSON `null` 会清空基础值。

可覆盖字段包括 `model_id`、`system_prompt`、`max_rounds`、
`max_continuation_retries`、`context_policy`、`plugin_ids`、
`active_hook_filter`、`sections`、`allowed_tools`、`allowed_tool_patterns`、
`excluded_tools`、`excluded_tool_patterns`、`delegates`、`reasoning_effort`
和 `endpoint`。

`sections` 使用按 key 浅合并。patch 中某个 section key 的值为 JSON `null`
时，会从 effective spec 中删除这个 section。`endpoint`、`allowed_tools`、
`allowed_tool_patterns`、`excluded_tools`、`excluded_tool_patterns`、
`context_policy`、`reasoning_effort` 等可选字段是三态：缺失表示继承，`null`
表示清空，给出值表示覆盖。其他列表和标量字段在出现时整体替换基础值。

关于工具目录字段的特别说明：PATCH 中的 `null` **不会**重新触发"未配置 =
允许全部"的兼容 shim —— 该 shim 只在完整 `AgentSpec` 的初次反序列化阶段
运行。如果一个 PATCH 同时把 `allowed_tools` 与 `allowed_tool_patterns`
清空为 `null`，合并后的 spec 没有任何 allow 规则，匹配器会拒绝所有工具。
要通过 PATCH 恢复"允许全部"，请显式写入
`allowed_tool_patterns: ["*"]`。

未知 patch 字段会被拒绝。调用方需要在保存 patch 前复用 Awaken 的 canonical
解析和未知字段策略时，可使用 `validate_agent_spec_patch(value)`。

## ConfigRecord 辅助函数

`ConfigRecord<T>` 用 provenance、可见性、时间戳、revision 和可选
`user_overrides` 包装一个已存储 spec。解码器同时接受 envelope 形状和旧的裸
spec；`to_value()` 始终写出 envelope 形状。

| 辅助函数 | 用途 |
|---|---|
| `validate_agent_spec(value)` | 解码 `AgentSpec` 并拒绝未知字段 |
| `validate_agent_spec_patch(value)` | 解码 `AgentSpecPatch` 并拒绝未知字段 |
| `validate_provider_spec(value)` | 解码 `ProviderSpec`，拒绝写入面未知字段，并拒绝空 `id` / `adapter` |
| `validate_model_binding_spec(value)` | 解码 `ModelBindingSpec`，拒绝未知字段，并拒绝空 `id` / `provider_id` / `upstream_model` |
| `decode_config_record<T>(value)` | 解码 `ConfigRecord<T>`，接受旧的裸 spec，但不检查 `user_overrides` |
| `validate_config_record<T>(value)` | 解码 `ConfigRecord<T>`，并按 `T` 的 patch 类型校验 `meta.user_overrides` |
| `effective_config_record(record)` | 对单条记录应用 `meta.user_overrides` |
| `effective_visible_config_records<T>(records)` | 解码记录、跳过 hidden 记录，并返回 effective specs |

`AgentSpec`、`AgentSpecPatch`、provider 写入面和 model binding 写入面使用
`UnknownFieldPolicy::Reject`；导出的 `AGENT_SPEC_UNKNOWN_FIELD_POLICY`、
`AGENT_SPEC_PATCH_UNKNOWN_FIELD_POLICY`、`PROVIDER_SPEC_UNKNOWN_FIELD_POLICY`
和 `MODEL_BINDING_SPEC_UNKNOWN_FIELD_POLICY` 常量让集成方可以显式读取该行为。
`ProviderSpec` 反序列化本身仍为兼容性保留宽松读取；config 写入和 validate
surface 使用 `validate_provider_spec(value)` 拒绝会被静默忽略的字段。

## ContextWindowPolicy

控制上下文窗口和自动压缩行为。

```rust,ignore
pub struct ContextWindowPolicy {
    pub max_context_tokens: usize,
    pub max_output_tokens: usize,
    pub min_recent_messages: usize,
    pub enable_prompt_cache: bool,
    pub autocompact_threshold: Option<usize>,
    pub compaction_mode: ContextCompactionMode,
    pub compaction_raw_suffix_messages: usize,
}
```

### ContextCompactionMode

```rust,ignore
pub enum ContextCompactionMode {
    KeepRecentRawSuffix,
    CompactToSafeFrontier,
}
```

## InferenceOverride

用于单次推理的参数覆盖。所有字段都是 `Option`，多插件同时写时按字段 last-wins 合并。

`upstream_model` 和 `fallback_upstream_models` 是当前已解析 provider 的上游模型名。它们不会重新解析
`AgentSpec.model_id`，也不会切换 provider。详见 [Provider 与 Model 配置](./provider-model-config.md)。

```rust,ignore
pub struct InferenceOverride {
    pub upstream_model: Option<String>,      // 上游模型名
    pub fallback_upstream_models: Option<Vec<String>>, // 上游模型名列表
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f64>,
    pub reasoning_effort: Option<ReasoningEffort>,
}
```

### 方法

```rust,ignore
fn is_empty(&self) -> bool
fn merge(&mut self, other: InferenceOverride)
```

### ReasoningEffort

```rust,ignore
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    Max,
    Budget(u32),
}
```

## PluginConfigKey trait

把配置 section 名称和 Rust 配置结构绑定在一起：

```rust,ignore
pub trait PluginConfigKey: 'static + Send + Sync {
    const KEY: &'static str;
    type Config: Default + Clone + Serialize + DeserializeOwned
        + schemars::JsonSchema + Send + Sync + 'static;
}
```

## RemoteEndpoint

远程 backend agent 的配置。当前内置的是 `"a2a"` backend，backend 专有参数放在 `options` 中：

```rust,ignore
pub struct RemoteEndpoint {
    pub backend: String,
    pub base_url: String,
    pub auth: Option<RemoteAuth>,
    pub target: Option<String>,
    pub timeout_ms: u64,
    pub options: BTreeMap<String, Value>,
}

pub struct RemoteAuth {
    pub r#type: String,
    // backend 专有认证字段，例如 bearer 用 { "token": "..." }
}
```

对于 A2A，`base_url` 指向 A2A interface root，例如
`https://agent.example.com/v1/a2a`；`target` 在远端 backend 暴露多个 agent 时选择目标 agent。旧 A2A 字段（`bearer_token`、`agent_id`、`poll_interval_ms`）只有在没有 canonical 字段时才会被反序列化。新配置应使用 `auth`、`target` 和 `options`。

## ServerConfig

HTTP server 配置。需启用 `server` feature。

```rust,ignore
use awaken::RedactedString;

pub struct ServerConfig {
    pub address: String,                              // default: "0.0.0.0:3000"
    pub sse_buffer_size: usize,                       // default: 64
    pub replay_buffer_capacity: usize,                // default: 1024
    pub shutdown: ShutdownConfig,
    pub max_concurrent_requests: usize,               // default: 100
    pub a2a_extended_card_bearer_token: Option<RedactedString>,
    pub mailbox_lifecycle: MailboxLifecycleMode,      // default: Auto
}

pub struct ShutdownConfig {
    pub timeout_secs: u64,                            // default: 30
}
```

**Crate 路径：** `awaken_server::app::ServerConfig`

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `address` | `String` | `"0.0.0.0:3000"` | 服务器绑定的 socket 地址 |
| `sse_buffer_size` | `usize` | `64` | 单连接 SSE 通道最大缓冲帧数 |
| `replay_buffer_capacity` | `usize` | `1024` | 每次 run 用于断线续接的最大 replay buffer 帧数 |
| `max_concurrent_requests` | `usize` | `100` | 最大并发请求数；超出时返回 503 |
| `a2a_extended_card_bearer_token` | `Option<RedactedString>` | `None` | 设置后启用带认证的 `GET /v1/a2a/extendedAgentCard`。`Debug`/`Display` 自动遮蔽，需要明文请调用 `expose_secret()`；JSON 序列化保持普通字符串 |
| `mailbox_lifecycle` | `MailboxLifecycleMode` | `Auto` | `Auto` 由框架启停 mailbox；`Manual` 把生命周期交给嵌入应用 |
| `shutdown.timeout_secs` | `u64` | `30` | 强制退出前等待飞行中请求排空的秒数 |

## AdminApiConfig

admin/configuration API 安全配置。通过
`AppState::with_admin_api_config` 挂到 `AppState` 上；只需要 bearer
认证时可使用 `AppState::with_admin_api_bearer_token`。

```rust,ignore
use awaken::RedactedString;

pub struct AdminApiConfig {
    pub bearer_token: Option<RedactedString>,
    pub cors_allowed_origins: Vec<String>,
    pub expose_config_routes: bool,                   // default: true
}
```

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `bearer_token` | `Option<RedactedString>` | `None` | 设置后，admin surface 要求 `Authorization: Bearer ...`：`/v1/capabilities`、`/v1/config/*`、`/v1/agents*`、`/v1/system/info`、`/v1/audit-log` 和 runtime-stats 端点。`Debug`/`Display` 自动遮蔽，需要明文请调用 `expose_secret()`；JSON 序列化保持普通字符串 |
| `cors_allowed_origins` | `Vec<String>` | `["http://127.0.0.1:3002", "http://localhost:3002"]` | admin CORS layer 允许的浏览器来源 |
| `expose_config_routes` | `bool` | `true` | 是否挂载 admin/configuration HTTP surface。当配置由外部 RBAC/审计流水线管理时设为 `false`，可以彻底隐藏这部分 HTTP 表面 |

环境变量会覆盖 `AppState` 上的 admin 配置：

| 变量 | 说明 |
|---|---|
| `AWAKEN_ADMIN_API_BEARER_TOKEN` | admin/configuration API 要求的 bearer token |
| `AWAKEN_ADMIN_CORS_ALLOWED_ORIGINS` | 浏览器 admin API 的 CORS 来源，逗号分隔 |

## AuditLogConfig

审计日志保留策略从 `AdminApiConfig` 中拆出，避免破坏 0.4.0 中
`AdminApiConfig` 的 struct literal 兼容性。调用
`AppState::with_audit_log_from_config` 之前，可通过
`AppState::with_audit_log_config` 挂到 `AppState`。

```rust,ignore
use awaken_server::app::AuditLogConfig;

pub struct AuditLogConfig {
    pub enabled: bool,              // default: true
    pub retention_days: u32,        // default: 90
    pub sweep_interval_secs: u64,   // default: 3600
}
```

### 凭据处理

`RedactedString`（门面 crate 重新导出为 `awaken::RedactedString`，定义在
`awaken_contract::secret`）是序列化配置中所有凭据的唯一信任边界。线缆格式仍是普通 JSON 字符串、JSON Schema 报告 `string`，内
部 buffer 在 `Drop` 时被清零。`Debug` 输出 `RedactedString(***)`，`Display`
输出 `***`；真正发起请求时调用 `expose_secret()` 获取明文，且不要把返回的
`&str` 传到日志。原本持有 `String` token 的代码只需在构造处加一个 `.into()`
或在读取处加一个 `.expose_secret()`。

## ConfigRuntimeManager

`ConfigRuntimeManager` 在配置变化时编译候选注册快照并发布到运行中的 runtime。

| 构建器方法 | 默认值 | 说明 |
|---|---|---|
| `with_provider_factory(factory)` | `GenaiProviderExecutorFactory` | 覆盖 `ProviderSpec` 到 `LlmExecutor` 的物化方式 |
| `with_change_notifier(notifier)` | `None` | 订阅原生变更通知，避免轮询 |
| `with_mcp_registry_factory(factory)` | `DefaultMcpRegistryFactory` | 覆盖 MCP server spec 到注册表的转换 |
| `with_mcp_refresh_interval(interval)` | 关闭 | 周期性刷新 MCP server 连接 |
| `with_min_apply_interval(interval)` | `Duration::ZERO` | 由 change listener 驱动的相邻 apply 之间的最小间隔。窗口内到达的事件会合并为一次 apply；直接调用 `apply` / `apply_if_changed` 不受影响。spec hash 未变的 provider 在 apply 之间会复用缓存的 executor |

## MailboxConfig

mailbox 持久化队列配置。控制租约计时、扫描/GC 间隔以及失败任务的重试行为。

```rust,ignore
pub struct MailboxConfig {
    pub lease_ms: u64,                          // default: 30_000
    pub suspended_lease_ms: u64,                // default: 600_000
    pub lease_renewal_interval: Duration,       // default: 10s
    pub sweep_interval: Duration,               // default: 30s
    pub gc_interval: Duration,                  // default: 60s
    pub gc_ttl: Duration,                       // default: 24h
    pub default_max_attempts: u32,              // default: 5
    pub default_retry_delay_ms: u64,            // default: 250
    pub max_retry_delay_ms: u64,                // default: 30_000
}
```

**Crate 路径：** `awaken_server::mailbox::MailboxConfig`

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `lease_ms` | `u64` | `30_000` | 活跃 run 的租约时长（毫秒） |
| `suspended_lease_ms` | `u64` | `600_000` | 等待人工输入的挂起 run 的租约时长（毫秒） |
| `lease_renewal_interval` | `Duration` | `10s` | worker 续约频率 |
| `sweep_interval` | `Duration` | `30s` | 扫描过期租约、回收孤儿任务的频率 |
| `gc_interval` | `Duration` | `60s` | 对已终止（完成/失败）任务进行垃圾回收的频率 |
| `gc_ttl` | `Duration` | `24h` | 已终止任务在被清除前的保留时长 |
| `default_max_attempts` | `u32` | `5` | 任务进入死信队列前的最大投递次数 |
| `default_retry_delay_ms` | `u64` | `250` | 两次重试之间的基础延迟（毫秒） |
| `max_retry_delay_ms` | `u64` | `30_000` | 指数退避的最大延迟上限（毫秒） |

## LlmRetryPolicy

LLM 推理失败后的重试与 fallback upstream model 策略，支持指数退避。可通过 `AgentSpec` 的 `"retry"` section 按 agent 配置。

Retry 在 agent 解析阶段生效。缺失 `"retry"` section 时使用 `LlmRetryPolicy::default()`。
将 `max_retries` 设为 `0` 且保持 `fallback_upstream_models` 为空可以禁用 retry 包装。Provider
构造阶段不会额外隐藏一层 retry 策略。对于流式推理，retry 与 fallback 只作用于打开
stream 的阶段。

```rust,ignore
pub struct LlmRetryPolicy {
    pub max_retries: u32,              // default: 2
    pub fallback_upstream_models: Vec<String>,  // default: []
    pub backoff_base_ms: u64,          // default: 500
}
```

**Crate 路径：** `awaken_runtime::engine::retry::LlmRetryPolicy`

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `max_retries` | `u32` | `2` | 初次调用后的最大重试次数（0 表示不重试） |
| `fallback_upstream_models` | `Vec<String>` | `[]` | 主模型耗尽重试后依次尝试的备用模型列表 |
| `backoff_base_ms` | `u64` | `500` | 指数退避的基础延迟（毫秒）；实际延迟 = min(base × 2^attempt, 8000ms)。设为 0 可禁用退避 |

### AgentSpec 集成

通过 `"retry"` section 配置：

```rust,ignore
use awaken_runtime::engine::retry::RetryConfigKey;

let spec = AgentSpec::new("my-agent")
    .with_config::<RetryConfigKey>(LlmRetryPolicy {
        max_retries: 3,
        fallback_upstream_models: vec!["claude-sonnet-4-20250514".into()],
        backoff_base_ms: 1000,
    })?;
```

## CircuitBreakerConfig

每个模型单独维护的熔断器配置。通过短路对失败过多的模型的请求，防止级联故障。冷却期过后熔断器进入半开状态，允许有限的探测请求；成功后完全关闭。

```rust,ignore
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,    // default: 5
    pub cooldown: Duration,        // default: 30s
    pub half_open_max: u32,        // default: 1
}
```

**Crate 路径：** `awaken_runtime::engine::circuit_breaker::CircuitBreakerConfig`

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `failure_threshold` | `u32` | `5` | 触发熔断器打开并拒绝请求所需的连续失败次数 |
| `cooldown` | `Duration` | `30s` | 熔断器从打开状态过渡到半开状态前的等待时长 |
| `half_open_max` | `u32` | `1` | 半开状态下允许的最大探测请求数；失败则重新打开，成功则完全关闭 |

## Feature flags 及其效果

| Flag | 运行时行为 |
|---|---|
| `permission` | 注册权限插件，可对工具启用 HITL 审批 |
| `observability` | 注册观测插件，发出 traces / metrics |
| `mcp` | 启用 MCP 工具桥接 |
| `skills` | 启用技能子系统 |
| `reminder` | 注册 reminder 插件，在工具执行后根据模式规则注入上下文消息 |
| `server` | 启用 HTTP / SSE server 与协议适配层 |
| `generative-ui` | 启用生成式 UI 组件流 |

工作区还包含不通过门面 feature 暴露的扩展 crate，当前包括 `awaken-ext-deferred-tools`。

## 自定义插件配置

插件通过 `PluginConfigKey` 声明类型化配置 section，并通过 `config_schemas()` 提供 JSON Schema，用于 resolve 阶段校验。

### 声明 schema 用于校验

```rust,ignore
fn config_schemas(&self) -> Vec<ConfigSchema> {
    vec![ConfigSchema {
        key: RateLimitConfigKey::KEY,
        json_schema: schemars::schema_for!(RateLimitConfig),
    }]
}
```

### 在运行时读取配置

```rust,ignore
let cfg = ctx.agent_spec().config::<RateLimitConfigKey>()?;
```

### 示例

```rust,ignore
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use awaken::PluginConfigKey;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RateLimitConfig {
    pub max_calls_per_step: u32,
    pub cooldown_ms: u64,
}

pub struct RateLimitConfigKey;

impl PluginConfigKey for RateLimitConfigKey {
    const KEY: &'static str = "rate_limit";
    type Config = RateLimitConfig;
}
```

### 校验行为

- section 存在但不合法：resolve 失败
- section 存在但没有插件声明：记录 warning
- section 缺失：返回 `Config::default()`

## DeferredToolsConfig

`awaken-ext-deferred-tools` 的插件 ID 是 `ext-deferred-tools`。agent 配置
section key 是 `deferred_tools`，由 `DeferredToolsConfigKey` 绑定。该 crate
未包含在 `awaken` 门面 crate 的 `full` feature 中；使用时需要直接添加
`awaken-ext-deferred-tools` 依赖，并用 seed tool descriptors 注册
`DeferredToolsPlugin`。

```json
{
  "enabled": null,
  "default_mode": "deferred",
  "beta_overhead": 1136.0,
  "rules": [
    { "tool": "get_weather", "mode": "eager" },
    { "tool": "debug_*", "mode": "deferred" }
  ],
  "agent_priors": {
    "get_weather": 0.03
  },
  "disc_beta": {
    "omega": 0.95,
    "n0": 5.0,
    "defer_after": 5,
    "thresh_mult": 0.5,
    "gamma": 2000.0
  }
}
```

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `enabled` | `bool \| null` | `null` | `true` 始终启用，`false` 禁用，`null`/缺失时在估算 schema 节省量超过 `beta_overhead` 后自动启用 |
| `rules` | `DeferralRule[]` | `[]` | 有序精确/glob 工具规则，首次匹配生效 |
| `default_mode` | `"eager" \| "deferred"` | `"deferred"` | 未匹配规则的工具模式 |
| `beta_overhead` | `number` | `1136.0` | `ToolSearch` 与 deferred-tool 列表的估算每轮开销 |
| `agent_priors` | `object` | `{}` | 可选的每工具先验使用频率，范围 `0..1`；缺失工具使用 `0.01` |
| `disc_beta.omega` | `number` | `0.95` | 每轮折扣因子；有效记忆约为 `1/(1-omega)` 轮 |
| `disc_beta.n0` | `number` | `5.0` | 先验强度，以等价观测数表示 |
| `disc_beta.defer_after` | `integer` | `5` | 已提升工具至少空闲多少轮后才考虑重新延迟 |
| `disc_beta.thresh_mult` | `number` | `0.5` | 应用于盈亏平衡频率的阈值乘数 |
| `disc_beta.gamma` | `number` | `2000.0` | 一次 `ToolSearch` 调用的估算 token 成本 |

自动启用启发式、`ToolSearch` 行为以及完整 DiscBeta 概率模型见
[使用延迟加载工具](../how-to/use-deferred-tools.md)。

## ConfigStore

`ConfigStore` 是服务端 `/v1/config/*` 路由背后的异步配置持久化契约。适用于需要在运行时创建、列举和更新配置，而不是把配置静态写死在 `AgentSpec` 中的场景。

```rust,ignore
#[async_trait]
pub trait ConfigStore: Send + Sync {
    async fn get(&self, namespace: &str, id: &str) -> Result<Option<Value>, StorageError>;
    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, Value)>, StorageError>;
    async fn put(&self, namespace: &str, id: &str, value: &Value) -> Result<(), StorageError>;
    async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError>;
}
```

相关类型：

- `ConfigChangeNotifier` / `ConfigChangeSubscriber` —— 可选的原生变更通知接口
- `AppState::with_config_store(...)` —— 为 `awaken-server` 启用运行时配置路由
- `ConfigRuntimeManager` —— 写入配置前编译并校验候选 registry snapshot，校验通过后发布
- `ConfigService` —— `/v1/config/*`、`/v1/agents` 和 `/v1/capabilities` 使用的服务层

内置实现：

- `InMemoryStore` 实现 `ThreadRunStore`、`ProfileStore` 和 `ConfigStore`
- `FileStore` 实现 `ThreadRunStore`、`ProfileStore` 和 `ConfigStore`
- `PostgresStore` 实现 `ThreadRunStore` 和 `ConfigStore`

## 相关

- [构建 Agent](../how-to/build-an-agent.md)
- [通过配置调优 Agent 行为](../how-to/configure-agent-behavior.md)
- [HTTP API](./http-api.md)
- [Provider 与 Model 配置](./provider-model-config.md)
