# Awaken

[English](./README.md) | [中文](./README.zh-CN.md)

[![CI](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml/badge.svg)](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml) [![crates.io awaken](https://img.shields.io/crates/v/awaken.svg?label=awaken)](https://crates.io/crates/awaken) [![crates.io awaken-agent](https://img.shields.io/crates/v/awaken-agent.svg?label=awaken-agent)](https://crates.io/crates/awaken-agent) [![Changelog](https://img.shields.io/badge/changelog-0.5.0-informational)](./CHANGELOG.md) ![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.93-orange)

一个用 Rust 写的 Agent runtime：同一份 backend 同时给 AI SDK、CopilotKit、A2A、MCP 用，能在 LLM 流式过程中自愈，并把配置当作真正的控制面。

`awaken` 是当前的规范 crate（由 [@brayniac](https://github.com/brayniac) 转
让而来，详见下方鸣谢）。`awaken-agent` 是项目早期发布期的兼容包，导入名都是
`awaken`。MSRV：Rust 1.93。

在线文档：[GitHub Pages（英文）](https://awakenworks.github.io/awaken/) ·
[GitHub Pages（中文）](https://awakenworks.github.io/awaken/zh-CN/) ·
[Changelog](./CHANGELOG.md)

<p align="center">
  <img src="./docs/assets/demo.svg" alt="Awaken 演示 — 工具调用 + LLM 流式输出" width="800">
</p>

## 0.4 给你什么

- **一个 backend 同时多协议。** 同一个 runtime 同时提供 AI SDK v6、AG-UI / CopilotKit、A2A、MCP；它们都跑在同一个 `/v1/runs` 之上。
- **流式 LLM 调用能从瞬时故障中恢复。** mid-stream 中断与 idle stall 会被识别，并按四种明确方案恢复：继续文本、回放已完成的 tool call、注入 cancelled tool 提示重启、整轮重启；`Retry-After` 被尊重；`StreamCheckpointStore` 契约让恢复可以跨进程。 ([详情](./docs/book/src/zh-CN/how-to/recover-streaming-llms.md))
- **Thread 有父子层级。** sub-agent run 会创建子 thread，删除策略需显式指定（`reject` / `detach` / `cascade`）；`/v1/threads` 上的过滤与游标让带层级的 UI 直接好做。
- **凭据自动遮蔽。** `ProviderSpec.api_key`、admin / A2A bearer token 都用 `RedactedString` 包裹：`Debug`/`Display` 输出 `***`，`Drop` 时清零，JSON 序列化保持不变。
- **配置即控制面。** model、provider、prompt、reminder、permission、tool-loading 策略都在 `/v1/config/*` 与 `/v1/capabilities` 后面；apply 可以去抖，未变化的 provider executor 会被复用。
- **类型安全的状态与工具。** 类型化 `StateKey` + 合并策略，`TypedTool` 自动生成 JSON Schema，每个 phase 后批量原子提交。整个 workspace `unsafe_code = "forbid"`。

## 心智模型

1. **Tools** — 直接实现 `Tool`，或用 `TypedTool` 通过 `schemars` 生成 JSON Schema。
2. **Agents** — 系统提示词 + model binding + 允许的工具集；LLM 用自然语言编排，没有 DAG。
3. **State** — `run`/`thread` 作用域的类型化状态，加上跨 thread/agent 协作用的持久 profile 与 shared state。
4. **Plugins** — 覆盖 permission、可观测性、上下文管理、Skills、MCP、Generative UI 的生命周期钩子。

runtime 每轮跑 9 个类型化 phase，其中包含一个纯判定的 `ToolGate`；状态变更在每轮结束时批量原子提交。

## 上手

**前置条件：** Rust 1.93+ 和一个 OpenAI 兼容的 API Key。

```toml
[dependencies]
awaken = { version = "0.5.0" }
tokio = { version = "1.51.0", features = ["full"] }
async-trait = "0.1.89"
serde_json = "1.0.149"
```

`src/main.rs`：

```rust
use std::sync::Arc;
use serde_json::{json, Value};
use async_trait::async_trait;
use awaken::contract::tool::{Tool, ToolDescriptor, ToolResult, ToolOutput, ToolError, ToolCallContext};
use awaken::contract::message::Message;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::AgentSpec;
use awaken::registry::ModelBinding;
use awaken::{AgentRuntimeBuilder, RunRequest};

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echo input back to the caller")
            .with_parameters(json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        let text = args["text"].as_str().unwrap_or_default();
        Ok(ToolResult::success("echo", json!({ "echoed": text })).into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent_spec = AgentSpec::new("assistant")
        .with_model_id("gpt-4o-mini")
        .with_system_prompt("You are a helpful assistant. Use the echo tool when asked.")
        .with_max_rounds(5);

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(agent_spec)
        .with_tool("echo", Arc::new(EchoTool))
        .with_provider("openai", Arc::new(GenaiExecutor::new()))
        .with_model_binding("gpt-4o-mini", ModelBinding {
            provider_id: "openai".into(),
            upstream_model: "gpt-4o-mini".into(),
        })
        .build()?;

    let request = RunRequest::new(
        "thread-1",
        vec![Message::user("Say hello using the echo tool")],
    )
    .with_agent_id("assistant");

    // 快速开始只需要最终结果；需要 SSE、WebSocket、协议适配器或测试事件流时，
    // 使用 run(..., sink)。
    let result = runtime.run_to_completion(request).await?;
    println!("response: {}", result.response);
    println!("termination: {:?}", result.termination);

    Ok(())
}
```

运行：

```bash
export OPENAI_API_KEY=<your-key>
cargo run
```

预期输出包含 `response: ...` 和 `termination: NaturalEnd`。

快速开始路径已有无网络测试覆盖：

```bash
cargo test -p awaken --test readme_quickstart
```

真实 provider 验证是显式 opt-in，不让默认 CI 依赖外部模型服务：

```bash
OPENAI_API_KEY=<your-key> cargo test -p awaken --test readme_live_provider -- --ignored
```

## 通过任意协议提供服务

构建 runtime 后，一行代码即可启动多协议服务器：

```rust
use awaken::prelude::*;
use awaken::stores::{InMemoryMailboxStore, InMemoryStore};
use std::sync::Arc;

let store = Arc::new(InMemoryStore::new());
let runtime = Arc::new(runtime);
let mailbox = Arc::new(Mailbox::new(
    runtime.clone(),
    Arc::new(InMemoryMailboxStore::new()),
    store.clone(),
    "default-consumer".into(),
    MailboxConfig::default(),
));

let state = AppState::new(
    runtime.clone(),
    mailbox,
    store,
    runtime.resolver_arc(),
    ServerConfig::default(),
);
serve(state).await?;
```

#### 前端协议

| 协议 | 端点 | 前端 |
|---|---|---|
| AI SDK v6 | `POST /v1/ai-sdk/chat` | React `useChat()` |
| AG-UI | `POST /v1/ag-ui/run` | CopilotKit `<CopilotKit>` |
| A2A | `POST /v1/a2a/message:send` | 其他 Agent |
| MCP | `POST /v1/mcp` | JSON-RPC 2.0 |

可选的 admin console 读取 `/v1/capabilities`、写入 `/v1/config/*`，在浏览器里
管理 agents、models、providers、MCP servers 和插件配置 section。插件通过同一
套 `PluginConfigKey` 暴露 schema，因此保存 `permission`、`reminder`、
`generative-ui`、`deferred_tools` 等 section 后会发布新的 registry snapshot，
对下一次 `/v1/runs` 立即生效。BigModel 等 OpenAI 兼容服务使用 `openai`
adapter + 对应 `base_url`；非密的扩展项放到 `ProviderSpec.adapter_options`。

| 调优面 | 配置位置 |
|---|---|
| 基础 prompt | `AgentSpec.system_prompt` |
| model 与 provider 路由 | `AgentSpec.model_id` + `/v1/config/models` + `/v1/config/providers` |
| system reminder 与 prompt 注入 | `reminder` 插件 section |
| Generative UI prompt 指令 | `generative-ui` 插件 section |
| 工具策略与上下文成本 | `permission` 与 `deferred_tools` 插件 section |

**React + AI SDK v6：**

```typescript
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";

const { messages, sendMessage } = useChat({
  transport: new DefaultChatTransport({
    api: "http://localhost:3000/v1/ai-sdk/chat",
  }),
});
```

**Next.js + CopilotKit：**

```typescript
import { CopilotKit } from "@copilotkit/react-core";

<CopilotKit runtimeUrl="http://localhost:3000/v1/ag-ui/run">
  <YourApp />
</CopilotKit>
```

#### 托管配置

把 `ConfigStore` 接入 `AppState` 后，可通过 `/v1/config/*` 管理 agents、models、providers 和 MCP servers。参考 [通过配置调优 Agent 行为](https://awakenworks.github.io/awaken/zh-CN/how-to/configure-agent-behavior.html) 调优 provider、model binding、工具和插件 section。[`apps/admin-console`](./apps/admin-console/) 使用同一套 API，并通过 `VITE_BACKEND_URL` 读取服务端地址。

## 内置插件

门面 crate 的 `full` feature 默认启用以下插件。`default-features = false` 可
按需关闭。`awaken-ext-deferred-tools` 不被门面 crate 重新导出，需要直接依赖。

| 插件 | 作用 | Feature flag |
|---|---|---|
| **Permission** | Allow/Deny/Ask 规则匹配工具名和参数（支持 glob 与正则）。优先级 Deny > Allow > Ask；Ask 通过 mailbox 暂停 run，等待 HITL 决策。 | `permission` |
| **Reminder** | 工具调用匹配某模式时，在 system 或会话级注入上下文消息。 | `reminder` |
| **Observability** | 与 GenAI Semantic Conventions 对齐的 OpenTelemetry trace 与 metric；支持 OTLP、文件和内存导出。 | `observability` |
| **MCP** | 连接外部 MCP server，把它们的工具注册成 Awaken 原生工具。 | `mcp` |
| **Skills** | 发现 skill 包，推理前注入 catalog 让 LLM 按需激活。 | `skills` |
| **Generative UI** | 通过 A2UI、JSON Render、OpenUI Lang 把声明式 UI 组件流式发到前端。 | `generative-ui` |
| **Deferred Tools** | 把大体量工具 schema 藏在 `ToolSearch` 后，连续多轮没用就用折扣 Beta 用量模型把已提升的工具重新延迟。 | 直接依赖：`awaken-ext-deferred-tools` |

自定义工具拦截通过 `ToolGateHook` + `PluginRegistrar::register_tool_gate_hook()`
完成；`BeforeToolExecute` 仅用于工具真正即将执行那一刻的钩子。

## 适合的场景

- 想用 **Rust 后端**写 AI Agent，要编译期保证。
- 需要从一个 backend 同时服务 **AI SDK、CopilotKit、A2A 或 MCP**。
- 工具需要在并发中**安全共享状态**，run 需要**可审计历史 + checkpoint + 可恢复控制路径**。
- 可以接受自己注册工具与 provider，而不是依赖开箱即用的默认能力。

## 不适合的场景

- 想要**开箱即用的文件 / Shell / Web 工具** — 看 OpenAI Agents SDK、Dify、CrewAI。
- 想要**可视化工作流编辑器** — 看 Dify、LangGraph Studio。
- 想要 **Python** 快速原型开发 — 看 LangGraph、AG2、PydanticAI。
- 想要 **LLM 自主管理记忆**（让 Agent 自行决定记住什么）— 看 Letta。

## 架构

Awaken 由三层运行时组成。`awaken-contract` 定义共享契约：Agent 规格、model/provider 规格、工具、事件、传输 trait，以及类型化状态模型。`awaken-runtime` 负责把 `AgentSpec` 解析成 `ResolvedExecution`：本地 agent 会成为带插件 `ExecutionEnv` 的 `ResolvedAgent`，endpoint-backed agent 则通过 `ExecutionBackend` 执行。它还负责执行 phase loop，并管理运行中的 run 及其取消、HITL 决策等控制路径。`awaken-server` 则把同一个 runtime 暴露成 HTTP 路由、SSE 回放、mailbox 后台执行，以及 AI SDK v6、AG-UI、A2A、MCP 协议适配器。

围绕这三层的是存储和扩展。`awaken-stores` 为 thread/run 提供内存、文件、PostgreSQL 持久化，为 config 提供内存、文件、PostgreSQL 后端，为 mailbox 提供内存和 SQLite 后端，并为 profile state 提供内存和文件后端。`awaken-ext-*` crates 在 phase 和 tool 边界扩展运行时能力，包括权限、可观测性、MCP 工具发现、Skills、Reminder、Generative UI 和 deferred tools。

```text
awaken                   门面 crate，管理 feature flags
├─ awaken-contract       契约：spec、tool、event、transport、state model
├─ awaken-runtime        resolver、phase engine、loop runner、runtime control
├─ awaken-server         route、mailbox、SSE transport、protocol adapter
├─ awaken-stores         内存、文件、PostgreSQL 与 SQLite-backed 存储
├─ awaken-tool-pattern   扩展使用的 glob/regex 匹配
└─ awaken-ext-*          可选运行时扩展
```

## 示例与学习路径

| 示例 | 展示内容 |
|---|---|
| [`live_test`](./crates/awaken/examples/live_test.rs) | 基础 LLM 集成 |
| [`multi_turn`](./crates/awaken/examples/multi_turn.rs) | 多轮对话与持久化线程 |
| [`tool_call_live`](./crates/awaken/examples/tool_call_live.rs) | 工具调用（计算器） |
| [`ai-sdk-starter`](./examples/ai-sdk-starter/) | React + AI SDK v6 全栈 |
| [`copilotkit-starter`](./examples/copilotkit-starter/) | Next.js + CopilotKit 全栈 |
| [`openui-chat`](./examples/openui-chat/) | OpenUI Lang chat 前端 |
| [`admin-console`](./apps/admin-console/) | Config API 管理界面 |

```bash
export OPENAI_API_KEY=<your-key>
cargo run --package awaken --example multi_turn

# 全栈演示
pnpm install && pnpm --filter awaken-ai-sdk-starter dev

# 终端 1：admin console 使用的 starter backend
AWAKEN_STORAGE_DIR=./target/admin-sessions cargo run -p ai-sdk-starter-agent

# 终端 2：admin console
pnpm install
pnpm --filter awaken-admin-console dev
```

| 目标 | 从这里开始 | 然后 |
|---|---|---|
| 构建第一个 Agent | [快速上手](https://awakenworks.github.io/awaken/zh-CN/get-started.html) | [构建 Agent 路径](https://awakenworks.github.io/awaken/zh-CN/build-agents.html) |
| 查看全栈应用 | [AI SDK starter](./examples/ai-sdk-starter/) | [CopilotKit starter](./examples/copilotkit-starter/) |
| 管理运行时配置 | [Admin Console](./apps/admin-console/) | [通过配置调优 Agent 行为](https://awakenworks.github.io/awaken/zh-CN/how-to/configure-agent-behavior.html) |
| 探索 API | [参考文档](https://awakenworks.github.io/awaken/zh-CN/reference/overview.html) | `cargo doc --workspace --no-deps --open` |
| 理解运行时 | [架构](https://awakenworks.github.io/awaken/zh-CN/explanation/architecture.html) | [Run 生命周期与 Phases](https://awakenworks.github.io/awaken/zh-CN/explanation/run-lifecycle-and-phases.html) |
| 从 tirea 迁移 | [迁移指南](https://awakenworks.github.io/awaken/zh-CN/appendix/migration-from-tirea.html) | |

## 参与贡献

欢迎贡献！请参阅 [CONTRIBUTING.md](./CONTRIBUTING.md) 了解流程。

[适合新贡献者的 Issue](https://github.com/AwakenWorks/awaken/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) 是入门的好起点。特别欢迎：

- 新增内置 memory/file/PostgreSQL/SQLite 之外的 mailbox、config 与 storage 后端
- 内置工具实现（文件读写、Web 搜索）
- Token 用量追踪和预算控制
- 模型降级链

**贡献流程：** Fork → 新建分支 → 实现 + 测试 → `cargo clippy` 通过 → PR。

## 鸣谢

crates.io 上 `awaken` 这个名字是
[@brayniac](https://github.com/brayniac) 转让过来的——他原先维护着同名的另
一个 crate，并主动愿意把名字让出来，让本项目能用规范名发布。crates.io 上的
`awaken` `0.1`–`0.3` 属于那个早期项目；本仓库的发版历史延续自之前的
`awaken-agent 0.2.x`，并直接从 `0.4.0` 起步以跳过此前的版本号。再次感谢。

Awaken 也是 [tirea](../../tree/tirea-0.5) 的全新重写版本，与 tirea **不兼容**。
tirea 0.5 代码归档在 [`tirea-0.5`](../../tree/tirea-0.5) 分支。

## 许可证

双重许可：[MIT](./LICENSE-MIT) 或 [Apache-2.0](./LICENSE-APACHE)。
