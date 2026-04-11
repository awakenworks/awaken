# 启用可观测性

当你需要用 OpenTelemetry 兼容的遥测方式追踪 LLM 推理和工具执行时，使用本页。

## 前置条件

- 已有可运行的 awaken runtime
- `awaken` 启用了 `observability`
- 如果要导出 OTel：`awaken-ext-observability` 需要启用 `otel`，并准备好 collector

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["observability"] }
tokio = { version = "1", features = ["full"] }
```

## 步骤

1. 先用内存 sink（开发环境）：

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_observability::{ObservabilityPlugin, InMemorySink};
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, Plugin};

let sink = InMemorySink::new();
let obs_plugin = ObservabilityPlugin::new(sink.clone());
let mut agent_spec = AgentSpec::new("observed-agent")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("You are a helpful assistant.")
    .with_hook_filter("observability");
agent_spec.plugin_ids.push("observability".into());

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
    .with_plugin("observability", Arc::new(obs_plugin) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

`plugin_ids` 负责加载 observability 插件；`with_hook_filter("observability")`
在同一个 agent 加载多个插件时保留它的 phase hook。

run 结束后，可以直接读取 `sink.metrics()`。

2. 换成 OTel sink（生产环境）：

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_observability::{ObservabilityPlugin, OtelMetricsSink};
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, Plugin};
use opentelemetry_sdk::trace::SdkTracerProvider;

let provider = SdkTracerProvider::builder().build();
let tracer = provider.tracer("awaken");
let obs_plugin = ObservabilityPlugin::new(OtelMetricsSink::new(tracer));
```

3. 如果需要，也可以实现自定义 sink：

```rust,ignore
use awaken::ext_observability::{MetricsSink, GenAISpan, ToolSpan, AgentMetrics};

struct MySink;

impl MetricsSink for MySink {
    fn on_inference(&self, span: &GenAISpan) {}
    fn on_tool(&self, span: &ToolSpan) {}
    fn on_run_end(&self, metrics: &AgentMetrics) {}
}
```

4. 插件会在这些 phase 采集数据：

| Phase | 采集内容 |
|-------|----------|
| `RunStart` | session 起始时间 |
| `BeforeInference` | model、provider、开始时间 |
| `AfterInference` | token usage、finish reason、耗时 |
| `BeforeToolExecute` | tool 开始时间 |
| `AfterToolExecute` | tool 耗时、失败状态 |
| `RunEnd` | session 总时长 |

## 验证

1. 用 `InMemorySink` 跑一个 agent
2. 执行结束后调用 `sink.metrics()`
3. 确认 `inferences` 非空且 token 统计有值
4. 如果用 OTel，去 collector / Jaeger 确认 span 已上报

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| metrics 全是 0 | 插件没注册 | 通过 builder 注册 `ObservabilityPlugin` |
| 找不到 `OtelMetricsSink` | 缺少 `otel` feature | 给 `awaken-ext-observability` 开 `otel` |
| collector 里没有 span | exporter 没配置或 tracer provider 被提前释放 | 检查 exporter 和 provider 生命周期 |
| token 统计缺失 | provider 没返回 usage | 确保 `LlmExecutor` 产生 `TokenUsage` |

## 相关示例

- `crates/awaken-ext-observability/tests/`

## 关键文件

- `crates/awaken-ext-observability/src/lib.rs`
- `crates/awaken-ext-observability/src/plugin/plugin.rs`
- `crates/awaken-ext-observability/src/plugin/hooks.rs`
- `crates/awaken-ext-observability/src/metrics.rs`
- `crates/awaken-ext-observability/src/sink.rs`
- `crates/awaken-ext-observability/src/otel.rs`

## 相关

- [添加 Plugin](./add-a-plugin.md)
- [事件](../reference/events.md)
- [架构](../explanation/architecture.md)
