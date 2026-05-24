---
title: "在工具里调用 Sub-Agent"
description: "当工具需要把工作委托给另一个 agent，并且要精确控制父 state 怎么流入子、子 state 怎么写回父时，使用本页。"
---

当工具需要把工作委托给另一个 agent，**并且**需要精确控制哪些父 state 流入子 run、哪些子 state 流回父 store 时，使用本页。

Awaken 用一个辅助函数加上你已经熟悉的 `Tool::execute` 模式来覆盖这个场景。框架不引入 hook、phase 或 strategy 类型——state 传递就是写在 `execute` 里的普通 Rust 代码。

## 前置条件

- 已可运行的 agent runtime（见 [构建 Agent](/awaken/zh-cn/how-to/build-an-agent/)）
- 一份 `Tool` 实现（见 [新增工具](/awaken/zh-cn/how-to/add-a-tool/)）
- 子 agent 已注册到 runtime 的 resolver 中，使辅助函数能解析到它

```toml
[dependencies]
awaken = { version = "0.5" }
awaken-contract = "0.5"
awaken-runtime = "0.5"
async-trait = "0.1"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

辅助函数与其相关类型在 `awaken_runtime::child_agent` 下；`awaken` 门面并没有重新导出，因此请直接从 `awaken_runtime` 导入。

## 步骤

1. 声明父子双方共享的 `StateKey`：

```rust
use awaken_contract::state::{StateKey, StateKeyOptions};
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_contract::StateError;

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchConfig {
    pub topic: String,
    pub max_sources: u32,
}

pub struct ResearchConfigKey;

impl StateKey for ResearchConfigKey {
    const KEY: &'static str = "research.config";
    type Value = ResearchConfig;
    type Update = ResearchConfig;
    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
}

pub struct ResearchPlugin;

impl Plugin for ResearchPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "research-plugin" }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<ResearchConfigKey>(StateKeyOptions {
            persistent: true,
            ..Default::default()
        })
    }
}
```

子 agent 的 plugin 集里**必须**包含 `ResearchPlugin`，否则 seed 步骤会以 `StateError::UnknownKey` 失败。若你打算从父侧写入 `ResearchConfigKey`，父 agent 也要注册该 key。

2. 实现工具。关键调用是来自 `awaken_runtime::child_agent` 的 [`run_child_agent`](/awaken/zh-cn/reference/)：

```rust
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::contract::event_sink::NullEventSink;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::state::PersistedState;

use awaken_runtime::backend::{
    BackendControl, BackendDelegatePolicy, BackendParentContext, BackendRunResult,
    BackendRunStatus,
};
use awaken_runtime::child_agent::{ChildAgentParams, run_child_agent};
use awaken_runtime::registry::ExecutionResolver;
use awaken_runtime::{MutationBatch, StateCommand, StateStore};

pub struct ResearchTool {
    pub resolver: Arc<dyn ExecutionResolver>,
}

#[async_trait]
impl Tool for ResearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("research_topic", "research_topic",
            "对一个主题做深度研究并附引用")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "topic":       { "type": "string" },
                    "max_sources": { "type": "integer", "minimum": 1 }
                },
                "required": ["topic"]
            }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext)
        -> Result<ToolOutput, ToolError>
    {
        let topic = args["topic"].as_str()
            .ok_or_else(|| ToolError::InvalidArguments("topic required".into()))?;
        let max_sources = args["max_sources"].as_u64().unwrap_or(5) as u32;

        let seed = build_seed(topic, max_sources)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let outcome = run_child_agent(ChildAgentParams {
            resolver:           self.resolver.as_ref(),
            agent_id:           "researcher",
            messages:           vec![Message::user(&format!("Research: {topic}"))],
            parent: BackendParentContext {
                parent_run_id:       Some(ctx.run_identity.run_id.clone()),
                parent_thread_id:    Some(ctx.run_identity.thread_id.clone()),
                parent_tool_call_id: Some(ctx.call_id.clone()),
            },
            initial_state_seed: Some(seed),
            sink:               ctx.activity_sink.clone()
                                   .unwrap_or_else(|| Arc::new(NullEventSink)),
            control:            BackendControl::default(),
            policy:             BackendDelegatePolicy::default(),
        })
        .await
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        // 子 run 未成功完成时直接报错，避免给 parent 返回 ToolResult::success
        // 又不写任何 state——见下文 "应当避免的做法" 中关于挂起/失败处理的提示。
        if !matches!(outcome.status, BackendRunStatus::Completed) {
            return Err(ToolError::ExecutionFailed(format!(
                "child agent did not complete: {}",
                outcome.status,
            )));
        }

        let command = build_export(&outcome, topic)?;

        Ok(ToolOutput {
            result: ToolResult::success("research_topic", json!({
                "response":     outcome.response,
                "child_run_id": outcome.run_id,
                "steps":        outcome.steps,
            })),
            command,
            ..Default::default()
        })
    }

    fn validate_args(&self, _args: &Value) -> Result<(), ToolError> { Ok(()) }
}
```

3. 构造 seed（父 → 子）。最稳妥的方式是用一个临时 store 做类型化编码：

```rust
fn build_seed(topic: &str, max_sources: u32)
    -> Result<PersistedState, awaken_contract::StateError>
{
    let scratch = StateStore::new();
    scratch.install_plugin(ResearchPlugin)?;
    let mut batch = MutationBatch::new();
    batch.update::<ResearchConfigKey>(ResearchConfig {
        topic: topic.into(),
        max_sources,
    });
    scratch.commit(batch)?;
    scratch.export_persisted()
}
```

只有 `persistent: true` 的 `StateKey` 才会被 `export_persisted` 输出。若 seed 需要非持久 key，直接往 `PersistedState.extensions` 写原始 JSON 即可。

4. 构造 export（子 → 父）：从子的终态 state 解码后写入 `StateCommand`。

子的 `StateStore` 终态 snapshot 在 `BackendRunResult.state`（一个 `PersistedState`）里返回。解码你关心的 key，再翻译成针对父 state key 的 `StateCommand`——工具返回后 loop runner 会自动 commit。

```rust
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchFindings {
    pub items: Vec<String>,
}

pub struct ResearchFindingsKey;

impl StateKey for ResearchFindingsKey {
    const KEY: &'static str = "research.findings";
    type Value = ResearchFindings;
    type Update = ResearchFindings;
    fn apply(value: &mut Self::Value, update: Self::Update) { *value = update; }
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchSummary {
    pub topic: String,
    pub items: Vec<String>,
}

pub struct ResearchSummaryKey;

impl StateKey for ResearchSummaryKey {
    const KEY: &'static str = "research.summary";
    type Value = ResearchSummary;
    type Update = ResearchSummary;
    fn apply(value: &mut Self::Value, update: Self::Update) { *value = update; }
}

/// 把子终态 state 解码成 parent 的 `StateCommand`。外层 `execute` 已经拒
/// 绝了非 Completed 的 outcome，所以这里只需要处理 "成功但没产出 key"
/// 这种边界情况。
fn build_export(outcome: &BackendRunResult, topic: &str) -> Result<StateCommand, ToolError> {
    let mut cmd = StateCommand::new();
    let Some(state) = outcome.state.as_ref() else {
        return Ok(cmd);
    };
    let Some(json) = state.extensions.get(ResearchFindingsKey::KEY) else {
        return Ok(cmd);
    };
    let findings: ResearchFindings = serde_json::from_value(json.clone())
        .map_err(|e| ToolError::ExecutionFailed(format!("decode findings: {e}")))?;

    let mut batch = MutationBatch::new();
    batch.update::<ResearchSummaryKey>(ResearchSummary {
        topic: topic.into(),
        items: findings.items,
    });
    cmd.patch
        .extend(batch)
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
    Ok(cmd)
}
```

把它接入到 `execute`：

```rust
let command = build_export(&outcome, topic)?;
```

`ToolOutput.command` 会被 loop runner 在工具返回后 commit 进父 store——见 [工具与插件边界](/awaken/zh-cn/explanation/tool-and-plugin-boundary/)。这里没有新增任何 commit 路径，走的就是普通工具的同一套机制。

只有以 `persistent: true` 注册的 key 会出现在 `outcome.state.extensions`。如果你需要的值是非持久 key，要么改 child 端的注册，要么回退到 `outcome.response` / `outcome.output`（结构化文本输出与持久化标记无关）。

## 把子的文本流到父工具的输出

如果父工具希望子的 token 看起来像是父工具自己在流式输出（典型如 generative UI 工具），用 `StreamingPassthroughSink` 把 activity sink 包一层再交给 `run_child_agent`：

```rust
use awaken_runtime::StreamingPassthroughSink;

let parent_sink = ctx.activity_sink.clone()
    .unwrap_or_else(|| Arc::new(NullEventSink));
let (passthrough, buffer) = StreamingPassthroughSink::new(
    ctx.call_id.clone(),
    ctx.tool_name.clone(),
    parent_sink,
);

let outcome = run_child_agent(ChildAgentParams {
    sink: Arc::new(passthrough),
    // ...其它字段同上...
}).await?;

let streamed_text = buffer.lock().await.clone();
```

子的 `AgentEvent::TextDelta` 会被改装成 `AgentEvent::ToolCallStreamDelta` 发到父 sink，并以父工具的 `call_id` 为 key。`buffer` 累计完整文本。

## 应当避免的做法

- **不要 seed 子 agent 未注册的 key。** 子用 `UnknownKeyPolicy::Error` 应用 seed——未注册 key 会让子在首步前 fail。这是设计行为：把契约不一致暴露在启动期，而不是运行期。
- **`initial_state_seed` 只对 Local backend 生效。** state seeding 由 `BackendCapabilities::delegate_state_seed` 控制；目前只有进程内 Local backend 声明支持。A2A 以及任何尚未实现 seed wire 协议的非本地 backend 都会以 `ExecutionBackendError` 拒绝带 seed 的 delegate 请求，**不会**静默成功。如果远程子真的需要某些数据，请自己把它编码进 prompt。
- **非 `Completed` 状态不要 export。** `outcome.state` 在失败/取消时仍会填充以便诊断，但把不完整的子 state 写回父 state 会引入不一致。导出前先判断 `outcome.status`。
- **不要假设非持久 key 能跨 run 边界。** `BackendRunResult.state` 通过 `export_persisted` 构造，只包含 `persistent: true` 的 key。
- **不要把 `ctx.activity_sink` 直接传给流式子 agent。** 不经 `StreamingPassthroughSink` 包装，子的 `TextDelta` 会原样出现在父 sink 上，污染父消息流。要么包装，要么传 `NullEventSink`。
- **注意非本地 backend 的 transcript 语义。** 子通过 A2A backend（或其他 transcript-incremental 的远程 backend）跑时，只有 `User` 角色、`Visibility::All` 的内容会被转发给远端 agent——assistant / tool 历史不会。需要历史上下文时，要么自己编进 user prompt，要么用本地 backend。
- **passthrough sink 上的子错误会以"父级错误"的形态出现。** `StreamingPassthroughSink` 把子的 `AgentEvent::Error` 原样转发到父 sink。前端若把 run 级 error 当成致命错误，会同样对待子 agent 的失败；需要工具级语义时请自己在 sink 层包装/过滤。

## 另见

- [多 Agent 模式](/awaken/zh-cn/explanation/multi-agent-patterns/) —— delegation / handoff / sub-agent 何时用哪个
- [新增工具](/awaken/zh-cn/how-to/add-a-tool/) —— `Tool` trait 本身
- [使用 Generative UI](/awaken/zh-cn/how-to/use-generative-ui/) —— `run_streaming_subagent` 现在是 `run_child_agent` + `StreamingPassthroughSink` 的薄包装
