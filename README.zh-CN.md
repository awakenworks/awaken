# Awaken

[English](./README.md) | [中文](./README.zh-CN.md)

[![CI](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml/badge.svg)](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml) [![crates.io](https://img.shields.io/crates/v/awaken-agent.svg?label=crates.io)](https://crates.io/crates/awaken-agent) ![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.85-orange)

生产级 Rust AI Agent 运行时 — 类型安全状态、多协议服务、插件化扩展。

在 crates.io 上发布名为 `awaken-agent`，Rust 代码中的导入仍然保持为 `awaken`。
仓库开发工具链由 Rust 1.93.0 固定，crate 的 MSRV 为 1.85。

在线文档：[GitHub Pages（英文）](https://awakenworks.github.io/awaken/) | [GitHub Pages（中文）](https://awakenworks.github.io/awaken/zh-CN/)

<p align="center">
  <img src="./docs/assets/demo.svg" alt="Awaken 演示 — 工具调用 + LLM 流式输出" width="800">
</p>

## 亮点

- **Rust-first Agent Runtime**：类型化工具、自动生成 JSON Schema、类型化 state key、作用域化 snapshot，以及原子状态提交。
- **一个 runtime 服务多类客户端**：同一后端同时提供 HTTP/SSE run API、AI SDK v6、AG-UI/CopilotKit、A2A 与 MCP JSON-RPC。
- **配置优先的优化控制面**：model/provider 选择、prompt、reminder、permission、generative UI 与 deferred tools 都通过 `/v1/config/*`、`/v1/capabilities` 和 admin console 管理。
- **生产控制路径**：mailbox 后台 run、HITL 决策、取消/中断、SSE replay、重试、fallback model、熔断器、指标和健康检查。
- **插件能力面**：权限网关、Reminder、OpenTelemetry、MCP tools、Skills、Generative UI，以及带明确概率模型的 deferred tool loading。

## 30 秒速览

1. **Tools** — 类型化函数，JSON Schema 在编译时自动生成
2. **Agents** — 每个 Agent 拥有系统提示词、模型和允许的工具集；LLM 通过自然语言驱动编排 — 无需预定义流程图
3. **State** — 既有 `run` / `thread` 作用域的类型化状态，也有用于跨线程/跨 Agent 协作的持久化 profile/shared state
4. **Plugins** — 生命周期钩子覆盖权限、可观测性、上下文管理、Skills、MCP 等

Agent 选择工具、调用工具、读写状态，如此循环 — 全部由运行时通过 9 个类型化阶段编排，其中在真正执行工具前增加了纯判定的 `ToolGate`。每次状态变更都在 gather 阶段后原子提交。

## 5 分钟上手

**前置条件：** Rust 1.85 或更新版本。仓库本地开发工具链由 `rust-toolchain.toml` 固定为 `1.93.0`；另需一个 OpenAI 兼容 LLM 提供商 API Key。

在 `Cargo.toml` 中添加：

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1" }
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
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::VecEventSink;
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

    let sink = Arc::new(VecEventSink::new());
    runtime.run(request, sink.clone()).await?;

    let events = sink.take();
    println!("events: {}", events.len());

    let finished = events
        .iter()
        .any(|e| matches!(e, AgentEvent::RunFinish { .. }));
    println!("run_finish_seen: {}", finished);

    Ok(())
}
```

运行：

```bash
export OPENAI_API_KEY=<your-key>
cargo run
```

预期输出包含 `run_finish_seen: true`。

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

可选的 admin console 通过 `/v1/capabilities` 与 `/v1/config/*` 在页面中编辑
agents、models、providers、MCP servers 以及插件配置 section。插件配置通过
同一套类型化 `PluginConfigKey` 逻辑暴露 JSON Schema，运行时 hook 也从同一
section 读取，因此保存 `permission`、`reminder`、`generative-ui` 或
`deferred_tools` 后会发布新的 registry snapshot，并对后续 `/v1/runs` 生效。
BigModel 等 OpenAI 兼容服务使用 `openai` adapter，并配置对应的 `base_url`。

设计意图是把 agent 优化能力保持为数据驱动：model 选择、provider 端点、基础
prompt、system reminder、生成式 UI 指令、permission 策略和工具加载策略都应走
同一套 schema-backed 配置链路，而不是硬编码进 agent loop。

| 调优面 | 配置路径 |
|---|---|
| 基础 prompt | agent 条目中的 `AgentSpec.system_prompt` |
| model/provider 路由 | `AgentSpec.model_id`、`/v1/config/models`、`/v1/config/providers` |
| system reminder 与 prompt 上下文注入 | `reminder` 插件 section，使用 `system` 或 `suffix_system` target |
| Generative UI prompt 指令 | `generative-ui` 插件 section（`catalog_id`、`examples` 或完整 `instructions`） |
| 工具策略与上下文成本 | `permission` 与 `deferred_tools` 插件 section |
| prompt 语义 hook | 当前还不是内置插件；后续应以类型化 `PluginConfigKey` section 和 schema-backed hook 接入 |

**React + AI SDK v6：**

```typescript
import { useChat } from "ai/react";

const { messages, input, handleSubmit } = useChat({
  api: "http://localhost:3000/v1/ai-sdk/chat",
});
```

**Next.js + CopilotKit：**

```typescript
import { CopilotKit } from "@copilotkit/react-core";

<CopilotKit runtimeUrl="http://localhost:3000/v1/ag-ui/run">
  <YourApp />
</CopilotKit>
```

## 内置插件

| Plugin | 说明 | Feature Flag |
|---|---|---|
| **Permission** | 防火墙式工具访问控制，支持 Deny/Allow/Ask 规则、glob/正则匹配和 HITL 邮箱暂停。 | `permission` |
| **Reminder** | 工具调用匹配模式时自动注入 system/conversation 级别的上下文消息。 | `reminder` |
| **Observability** | 符合 GenAI 语义规范的 OpenTelemetry 遥测，支持 OTLP、文件和内存导出。 | `observability` |
| **MCP** | 连接外部 MCP 服务器，自动发现并注册其工具为 Awaken 原生工具。 | `mcp` |
| **Skills** | 发现技能包，推理前注入技能目录供 LLM 按需激活。 | `skills` |
| **Generative UI** | 通过 A2UI 协议向前端流式推送声明式 UI 组件。 | `generative-ui` |
| **Deferred Tools** | 将大型工具 schema 隐藏在 `ToolSearch` 之后，并用折扣 Beta 概率模型把空闲的已提升工具重新延迟。 | 直接依赖：`awaken-ext-deferred-tools` |

`awaken-ext-deferred-tools` 未包含在 `awaken` 门面 crate 的 `full` feature 中。
注册 `ext-deferred-tools` 插件后，通过 agent 的 `deferred_tools` section 配置。
设置方式、自动启用启发式以及 DiscBeta 概率模型见
[使用延迟加载工具](./docs/book/src/zh-CN/how-to/use-deferred-tools.md)。

如需自定义工具拦截，应实现 `ToolGateHook` 并通过 `PluginRegistrar::register_tool_gate_hook()` 注册；`BeforeToolExecute` 仅用于真正执行前的一次性钩子。

## 为什么选择 Awaken

- **一个后端服务所有前端** — 从同一个二进制文件提供 React（AI SDK v6）、Next.js（AG-UI）、其他 Agent（A2A）和工具服务器（MCP）。无需分别部署。
- **配置就是控制面** — model/provider 路由、prompt、reminder、permission 与工具加载策略都是可校验、可页面编辑、可运行时生效的数据。
- **LLM 编排一切，无需 DAG** — 定义 Agent 的身份和工具访问权限；LLM 决定何时委托、委托给谁、如何组合结果。无需手写流程图或状态机。
- **可组合的插件体系** — 9 个类型化生命周期阶段，其中包含纯判定的 `ToolGate`。权限、上下文注入、可观测性、工具发现，全部声明式配置。`PhaseHook` / `ToolGateHook` 类型安全，插件注册 API 在构建时捕获配置错误。
- **类型安全的状态与回放** — State 是带编译时检查的 Rust 结构体。合并策略处理并发写入，无需锁。作用域限定为 thread 或 run，每次变更都是可回放的不可变快照。
- **内置生产韧性** — 熔断器、指数退避、推理超时、优雅关闭、Prometheus 指标和健康探针，开箱即用。
- **零 `unsafe` 代码** — 整个工作空间禁止 `unsafe`，内存安全由 Rust 编译器保证。

## 适用场景 / 不适用场景

**适合 Awaken：**

- 需要 **Rust 后端**构建 AI Agent，享受编译时安全
- 需要从一个后端同时提供**多种前端或 Agent 协议**
- 工具需要在并发执行中**安全共享状态**
- 需要**可审计的线程历史**、checkpoint 与可恢复控制路径
- 能接受自己注册工具、provider 与 model registry，而不是依赖开箱即用的默认能力

**不适合 Awaken：**

- 需要**开箱即用的文件/Shell/Web 工具** — 可考虑 OpenAI Agents SDK、Dify、CrewAI
- 需要**可视化工作流编辑器** — 考虑 Dify、LangGraph Studio
- 需要 **Python** 快速原型开发 — 考虑 LangGraph、AG2、PydanticAI
- 需要一个**稳定且变化缓慢**的表面 API，而不是持续演进的运行时平台
- 需要 **LLM 自主管理的记忆**（Agent 自行决定记住什么）— 考虑 Letta

## 架构

Awaken 由三层运行时组成。`awaken-contract` 定义共享契约：Agent 规格、model/provider 规格、工具、事件、传输 trait，以及类型化状态模型。`awaken-runtime` 负责把 `AgentSpec` 解析成 `ResolvedAgent`，从插件构建 `ExecutionEnv`，执行 phase loop，并管理运行中的 run 及其取消、HITL 决策等控制路径。`awaken-server` 则把同一个 runtime 暴露成 HTTP 路由、SSE 回放、mailbox 后台执行，以及 AI SDK v6、AG-UI、A2A、MCP 协议适配器。

围绕这三层的是存储和扩展。`awaken-stores` 提供线程与 run 的内存、文件、PostgreSQL 后端。`awaken-ext-*` crates 在 phase 和 tool 边界扩展运行时能力，包括权限、可观测性、MCP 工具发现、Skills、Reminder、Generative UI 和 deferred tools。

```text
awaken                   门面 crate，管理 feature flags
├─ awaken-contract       契约：spec、tool、event、transport、state model
├─ awaken-runtime        resolver、phase engine、loop runner、runtime control
├─ awaken-server         route、mailbox、SSE transport、protocol adapter
├─ awaken-stores         内存、文件、PostgreSQL 持久化
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
| [`admin-console`](./apps/admin-console/) | 管理运行时配置与插件 schema 的浏览器 UI |

```bash
export OPENAI_API_KEY=<your-key>
cargo run --package awaken-agent --example multi_turn

# 全栈演示
cd examples/ai-sdk-starter && npm install && npm run dev

# 终端 1：admin console 使用的 starter backend
AWAKEN_STORAGE_DIR=./target/admin-sessions cargo run -p ai-sdk-starter-agent

# 终端 2：admin console
npm --prefix apps/admin-console install
npm --prefix apps/admin-console run dev
```

| 目标 | 从这里开始 | 然后 |
|---|---|---|
| 构建第一个 Agent | [快速上手](https://awakenworks.github.io/awaken/zh-CN/get-started.html) | [构建 Agent 路径](https://awakenworks.github.io/awaken/zh-CN/build-agents.html) |
| 查看全栈应用 | [AI SDK starter](./examples/ai-sdk-starter/) | [CopilotKit starter](./examples/copilotkit-starter/) |
| 探索 API | [参考文档](https://awakenworks.github.io/awaken/zh-CN/reference/overview.html) | `cargo doc --workspace --no-deps --open` |
| 理解运行时 | [架构](https://awakenworks.github.io/awaken/zh-CN/explanation/architecture.html) | [Run 生命周期与 Phases](https://awakenworks.github.io/awaken/zh-CN/explanation/run-lifecycle-and-phases.html) |
| 从 tirea 迁移 | [迁移指南](https://awakenworks.github.io/awaken/zh-CN/appendix/migration-from-tirea.html) | |

## 参与贡献

欢迎贡献！请参阅 [CONTRIBUTING.md](./CONTRIBUTING.md) 了解流程。

[适合新贡献者的 Issue](https://github.com/AwakenWorks/awaken/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) 是入门的好起点。特别欢迎：

- 新增存储后端（Redis、完整 SQLite thread/run store）
- 内置工具实现（文件读写、Web 搜索）
- Token 用量追踪和预算控制
- 模型降级链

**贡献流程：** Fork → 新建分支 → 实现 + 测试 → `cargo clippy` 通过 → PR。

## 许可证

双重许可：[MIT](./LICENSE-MIT) 或 [Apache-2.0](./LICENSE-APACHE)。

> Awaken 是 [tirea](../../tree/tirea-0.5) 的全新重写版本，专为简洁性和生产可靠性而设计。tirea 0.5 代码已归档在 [`tirea-0.5`](../../tree/tirea-0.5) 分支，Awaken 与 tirea **不兼容**。
