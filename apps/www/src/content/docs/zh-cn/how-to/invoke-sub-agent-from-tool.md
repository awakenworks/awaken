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

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchFindings {
    pub items: Vec<String>,
}

pub struct ResearchFindingsKey;

impl StateKey for ResearchFindingsKey {
    const KEY: &'static str = "research.findings";
    type Value = ResearchFindings;
    type Update = ResearchFindings;
    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
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
        })?;
        r.register_key::<ResearchFindingsKey>(StateKeyOptions {
            persistent: true,
            ..Default::default()
        })?;
        r.register_key::<ResearchSummaryKey>(StateKeyOptions {
            persistent: true,
            ..Default::default()
        })
    }
}
```

子 agent 必须注册 `ResearchConfigKey`，这样 seed 才能应用；如果你希望 findings 出现在 `outcome.state.extensions`，它还必须以 `persistent: true` 注册 `ResearchFindingsKey`。父 agent 在提交返回的 `StateCommand` 前必须注册 `ResearchSummaryKey`。上面的单个 `ResearchPlugin` 为了方便复制粘贴而注册了全部三个 key；生产代码可以拆成 `ChildResearchPlugin` / `ParentResearchPlugin`，只要两边分别注册自己会读写的 key 即可。

2. 实现工具。关键调用是来自 `awaken_runtime::child_agent` 的 [`run_child_agent`](/awaken/zh-cn/reference/)。它返回子 run 的终态 [`BackendRunResult`](/awaken/zh-cn/reference/)；父工具自行决定如何把这个生命周期状态解释成自己的 `ToolOutput.result`。下面示例采用语义透传策略：父工具返回成功 payload，并显式带上 `child_status`，但 state export 仍保持保守。

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

use awaken_runtime::backend::{BackendParentContext, BackendRunResult, BackendRunStatus};
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

        let outcome = run_child_agent(
            ChildAgentParams::new(
                self.resolver.as_ref(),
                "researcher",
                vec![Message::user(&format!("Research: {topic}"))],
                BackendParentContext {
                    parent_run_id:       Some(ctx.run_identity.run_id.clone()),
                    parent_thread_id:    Some(ctx.run_identity.thread_id.clone()),
                    parent_tool_call_id: Some(ctx.call_id.clone()),
                },
                ctx.activity_sink.clone()
                    .unwrap_or_else(|| Arc::new(NullEventSink)),
            )
            .with_initial_state_seed(seed)
            .with_cancellation_token(ctx.cancellation_token.clone()),
        )
        .await
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let command = build_export(&outcome, topic)?;

        Ok(ToolOutput::with_command(
            ToolResult::success("research_topic", json!({
                "child_status": outcome.status.to_string(),
                "response":     outcome.response,
                "child_run_id": outcome.run_id,
                "steps":        outcome.steps,
            })),
            command,
        ))
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
/// 把子终态 state 解码成 parent 的 `StateCommand`。这个 export 策略
/// 比上面的语义结果策略更严格：只有子 run Completed 时才把 findings
/// 写回父 state。
fn build_export(outcome: &BackendRunResult, topic: &str) -> Result<StateCommand, ToolError> {
    let mut cmd = StateCommand::new();
    if !matches!(outcome.status, BackendRunStatus::Completed) {
        return Ok(cmd);
    }
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

### 为 child status 选择父工具策略

`BackendRunResult.status` 是子 run 的生命周期状态。`ToolOutput.result` 是父工具对这个结果的解释。上面的语义透传示例即使在 child 返回 `Failed`、`Cancelled`、`Timeout`、`Suspended` 或等待状态时，也会让父工具成功返回一条带 `child_status` 的 payload，让父 agent 继续判断下一步。

如果父工具只接受完成的 child，可以使用严格策略：

```rust
if !matches!(outcome.status, BackendRunStatus::Completed) {
    return Err(ToolError::ExecutionFailed(format!(
        "sub-agent did not complete: {}",
        outcome.status
    )));
}
```

`run_streaming_subagent` 就属于这种严格 helper：它把 child stream 当作当前工具输出，所以会拒绝非 `Completed` 的 child 结果。state export 是另一层独立策略；不要因为父工具返回语义成功 payload，就盲目把 child state 写回父 state。

## 把子的文本流到父工具的输出

如果父工具希望子的 token 看起来像是父工具自己在流式输出（典型如 generative UI 工具），用 `StreamingPassthroughSink` 把 activity sink 包一层再交给 `run_child_agent_checked` 或 `run_child_agent`：

```rust
use awaken_contract::contract::message::Message;
use awaken_runtime::backend::BackendParentContext;
use awaken_runtime::{
    ChildAgentParams, StreamingPassthroughSink, run_child_agent_checked,
};

let parent_sink = ctx.activity_sink.clone()
    .unwrap_or_else(|| Arc::new(NullEventSink));
let (passthrough, buffer) = StreamingPassthroughSink::new(
    ctx.call_id.clone(),
    ctx.tool_name.clone(),
    parent_sink,
);

let outcome = run_child_agent_checked(
    ChildAgentParams::new(
        self.resolver.as_ref(),
        "researcher",
        vec![Message::user("stream the research")],
        BackendParentContext::default(),
        Arc::new(passthrough),
    )
    .with_cancellation_token(ctx.cancellation_token.clone()),
)
.await
.map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

let streamed_text = buffer.lock().await.clone();
```

子的 `AgentEvent::TextDelta` 会被改装成 `AgentEvent::ToolCallStreamDelta` 发到父 sink，并以父工具的 `call_id` 为 key。`buffer` 累计完整文本。默认情况下，子的 `AgentEvent::Error` 也会被包装成 `ToolCallStreamDelta` 文本，避免前端误认为父 run fatal；只有当你的事件消费者明确理解 raw child error 语义时，才使用 `StreamingPassthroughSink::new_with_error_forwarding(..., ChildErrorForwarding::ForwardRawParentError)`。

## Backend 实现者迁移提示

`BackendCapabilities` 是 `#[non_exhaustive]`；请用 `BackendCapabilities::full()` 或 `BackendCapabilities::remote_stateless_text()` 构造后再修改字段。带 seed 的 delegate 请求现在受 capability 控制：

- 如果你的 backend 确实会在运行 child 前应用 `BackendDelegateRunRequest.state_seed`，请设置 `capabilities.delegate_state_seed = true`。
- 如果不支持，请保持 `false`；带 seed 的 delegate 调用会以 `ExecutionBackendError` 拒绝，而不是静默忽略 seed。

## 应当避免的做法

- **不要 seed 子 agent 未注册的 key。** 子用 `UnknownKeyPolicy::Error` 应用 seed——未注册 key 会让子在首步前 fail。这是设计行为：把契约不一致暴露在启动期，而不是运行期。
- **要透传父 run 的 cancellation。** 在工具里调用 child 时，调用 `.with_cancellation_token(ctx.cancellation_token.clone())`，这样取消父 run 时也会取消 child run。
- **`initial_state_seed` 只对 Local backend 生效。** state seeding 由 `BackendCapabilities::delegate_state_seed` 控制；目前只有进程内 Local backend 声明支持。A2A 以及任何尚未实现 seed wire 协议的非本地 backend 都会以 `ExecutionBackendError` 拒绝带 seed 的 delegate 请求，**不会**静默成功。如果远程子真的需要某些数据，请自己把它编码进 prompt。
- **不要在非 `Completed` 状态下盲目 export child state。** 子结果是给父 agent 解释的语义消息；父工具应单独决定是失败、返回语义成功 payload，还是选择性导出诊断 state。对 `Failed` / `Cancelled` 这类已经返回 `BackendRunResult` 的终态，`outcome.state` 是否可用取决于 backend 以及失败发生的位置；backend dispatch 或 loop setup 级别的错误会直接返回 `Err`，不会提供 `BackendRunResult.state`。
- **不要假设非持久 key 能跨 run 边界。** `BackendRunResult.state` 通过 `export_persisted` 构造，只包含 `persistent: true` 的 key。
- **不要把 `ctx.activity_sink` 直接传给流式子 agent。** 不经 `StreamingPassthroughSink` 包装，子的 `TextDelta` 会原样出现在父 sink 上，污染父消息流。要么包装，要么传 `NullEventSink`。
- **注意非本地 backend 的 transcript 语义。** 子通过 A2A backend（或其他 transcript-incremental 的远程 backend）跑时，只有 `User` 角色、`Visibility::All` 的内容会被转发给远端 agent——assistant / tool 历史不会。需要历史上下文时，要么自己编进 user prompt，要么用本地 backend。
- **不要把 A2A delegate 的 `run_id` 和远端 task id 混淆。** 对 delegate 调用来说，`BackendRunResult.run_id` 是本地生成的 correlation id，用于子工具、suspension、trace 关联。远端 A2A task id 仍然保存在 A2A progress metadata/state 中，不会被这个本地 id 替代。
- **`initial_messages` 是 fresh delegation 的初始输入，不是 history + 新增量的拆分。** `ChildAgentParams::new(..., initial_messages, ...)` 就是 child 启动时看到的输入，通常是单个 `Message::user`。当前 API 不支持复用旧 delegate transcript。内部 `run_child_agent` 会把这个 fresh input 映射到 `BackendDelegateRunRequest.messages` 和 `.new_messages`，不要据此假设公共 API 支持续跑。
- **passthrough sink 的 raw 子错误是显式 opt-in。** `StreamingPassthroughSink::new` 默认把子的 `AgentEvent::Error` 包装成父 `ToolCallStreamDelta` 输出。只有当 UI 明确知道 raw error 来自 child tool stream、不会自动 kill parent run 时，才选择 `ChildErrorForwarding::ForwardRawParentError`。

## 另见

- [多 Agent 模式](/awaken/zh-cn/explanation/multi-agent-patterns/) —— delegation / handoff / sub-agent 何时用哪个
- [新增工具](/awaken/zh-cn/how-to/add-a-tool/) —— `Tool` trait 本身
- [使用 Generative UI](/awaken/zh-cn/how-to/use-generative-ui/) —— `run_streaming_subagent` 现在是 `run_child_agent` + `StreamingPassthroughSink` 的薄包装
