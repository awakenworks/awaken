# 配置

## AgentSpec

`AgentSpec` 是可序列化的 agent 定义。它既可以从 JSON / YAML 加载，也可以用 builder 方法在代码里构造。

```rust,ignore
pub struct AgentSpec {
    pub id: String,
    pub model: String,
    pub system_prompt: String,
    pub max_rounds: usize,
    pub max_continuation_retries: usize,
    pub context_policy: Option<ContextWindowPolicy>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub plugin_ids: Vec<String>,
    pub active_hook_filter: HashSet<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub excluded_tools: Option<Vec<String>>,
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
    .with_model(model) -> Self
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

```rust,ignore
pub struct InferenceOverride {
    pub model: Option<String>,
    pub fallback_models: Option<Vec<String>>,
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

远程 A2A agent 的配置：

```rust,ignore
pub struct RemoteEndpoint {
    pub base_url: String,
    pub bearer_token: Option<String>,
    pub agent_id: Option<String>,
    pub poll_interval_ms: u64,
    pub timeout_ms: u64,
}
```

## ServerConfig

HTTP server 配置。需启用 `server` feature。

```rust,ignore
pub struct ServerConfig {
    pub address: String,                   // default: "0.0.0.0:3000"
    pub sse_buffer_size: usize,            // default: 64
    pub replay_buffer_capacity: usize,     // default: 1024
    pub shutdown: ShutdownConfig,
    pub max_concurrent_requests: usize,    // default: 100
    pub a2a_extended_card_bearer_token: Option<String>,
}

pub struct ShutdownConfig {
    pub timeout_secs: u64,                 // default: 30
}
```

**Crate 路径：** `awaken_server::app::ServerConfig`

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `address` | `String` | `"0.0.0.0:3000"` | 服务器绑定的 socket 地址 |
| `sse_buffer_size` | `usize` | `64` | 单连接 SSE 通道最大缓冲帧数 |
| `replay_buffer_capacity` | `usize` | `1024` | 每次 run 用于断线续接的最大 replay buffer 帧数 |
| `max_concurrent_requests` | `usize` | `100` | 最大并发请求数；超出时返回 503 |
| `a2a_extended_card_bearer_token` | `Option<String>` | `None` | 设置后启用带认证的 `GET /v1/a2a/extendedAgentCard` |
| `shutdown.timeout_secs` | `u64` | `30` | 强制退出前等待飞行中请求排空的秒数 |

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

LLM 推理失败后的重试与 fallback model 策略，支持指数退避。可通过 `AgentSpec` 的 `"retry"` section 按 agent 配置。

```rust,ignore
pub struct LlmRetryPolicy {
    pub max_retries: u32,              // default: 2
    pub fallback_models: Vec<String>,  // default: []
    pub backoff_base_ms: u64,          // default: 500
}
```

**Crate 路径：** `awaken_runtime::engine::retry::LlmRetryPolicy`

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `max_retries` | `u32` | `2` | 初次调用后的最大重试次数（0 表示不重试） |
| `fallback_models` | `Vec<String>` | `[]` | 主模型耗尽重试后依次尝试的备用模型列表 |
| `backoff_base_ms` | `u64` | `500` | 指数退避的基础延迟（毫秒）；实际延迟 = min(base × 2^attempt, 8000ms)。设为 0 可禁用退避 |

### AgentSpec 集成

通过 `"retry"` section 配置：

```rust,ignore
use awaken_runtime::engine::retry::RetryConfigKey;

let spec = AgentSpec::new("my-agent")
    .with_config::<RetryConfigKey>(LlmRetryPolicy {
        max_retries: 3,
        fallback_models: vec!["claude-sonnet-4-20250514".into()],
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

## 相关

- [构建 Agent](../how-to/build-an-agent.md)
