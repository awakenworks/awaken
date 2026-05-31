# Awaken

[English](./README.md) | [中文](./README.zh-CN.md)

[![CI](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml/badge.svg)](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml) [![crates.io awaken](https://img.shields.io/crates/v/awaken.svg?label=awaken)](https://crates.io/crates/awaken) [![crates.io awaken-agent](https://img.shields.io/crates/v/awaken-agent.svg?label=awaken-agent)](https://crates.io/crates/awaken-agent) [![Changelog](https://img.shields.io/badge/changelog-current-informational)](./CHANGELOG.md) ![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.93-orange)

用 Rust 写一次 Agent 能力，把行为调优交给在线配置，并让同一个 runtime 服务本地开发、生产 API、多协议前端和管理控制台。Awaken 是面向生产的 Rust AI Agent 后端：tools、state、plugins 留在代码里，agents、models、prompts 通过 server 控制面验证、发布和热调优。应用自己处理 I/O 时用 runtime 模式；需要协议适配、持久编排、Trace/Eval 和控制台时用 server 模式。

在线文档：[Awaken docs（英文）](https://awakenworks.github.io/awaken) · [中文文档](https://awakenworks.github.io/awaken/zh-cn) · [Changelog](./CHANGELOG.md)。MSRV：Rust 1.93。发布的 crate 是 `awaken`；`awaken-agent` 是早期同名发布期的兼容包，导入名都是 `awaken`。

<p align="center">
  <img src="./docs/assets/demo.svg" alt="Awaken 演示 — 工具调用 + LLM 流式输出" width="800">
</p>

## 选择开发模式

Awaken 把 **agent 执行 loop** 和 **服务控制面**分开。runtime 负责 agent 推理、工具选择、类型化 phase、状态提交和直接 run API；server 负责服务侧编排：HTTP/SSE、协议适配、mailbox 派发、托管配置、审计/恢复和管理控制台工作流。

| 模式 | 从这里开始 | 你负责 | Awaken 提供 |
|---|---|---|---|
| **runtime 开发** | `awaken` / `awaken-runtime` | HTTP/UI/job scheduling、auth、配置存储、具体 tools/providers/stores | 直接 run API、流式事件、9 phase loop、类型化 tools/state、取消与 HITL primitives |
| **server 开发** | `awaken-server` + `awaken-stores` | 部署、租户/auth 策略、已注册 tools/providers、store 选择 | HTTP resources、SSE replay、AI SDK/AG-UI/A2A/MCP/ACP adapters、mailbox 编排、`/v1/config/*`、registry snapshots、管理控制台 |

构建 Rust 应用或测试 harness，并且要自己掌控 I/O 时，从 runtime 模式开始；当多个客户端、运维人员或后台 worker 需要共享同一个 agent surface，并且要求持久 run 与在线配置时，使用 server 模式。

这里的 runtime 模式指标准 Rust 程序里的进程内 library 使用，不是 `no_std` 或无 Tokio 的嵌入式设备目标：`awaken-runtime` 当前依赖 Tokio 来处理 timer、timeout、异步协调和 HTTP/provider 执行。

当前 IO/runtime 边界：

| 组件 | Tokio / IO 画像 |
|---|---|
| `awaken-runtime` | 需要 Tokio。phase loop 是进程内执行，但 crate 内包含 `genai` / `reqwest` provider 路径，以及基于 Tokio 的 timeout、retry、后台任务机制。 |
| `awaken-runtime-contract` / `awaken-server-contract` | 主要是 contract/type surface，适合作 API 边界；但目标仍然是 `std` Rust crate，不是 `no_std` 嵌入式目标。 |
| Permission、Reminder、Deferred Tools、Generative UI | 主要是进程内 policy/state/event 逻辑，但依赖 runtime contract/runtime 栈，因此继承 Tokio/std 假设。 |
| MCP 与 Skills | 具备 IO 能力：MCP 使用 network/stdio/process transport；Skills 可以从磁盘读取 skill package、启动配置命令，并可选注册 MCP tools。 |
| Observability | 内存记录是本地逻辑；OTLP/file/metrics exporter 会引入外部 IO。 |
| Stores 与 Server | 明确的 IO 层：memory/file/PostgreSQL/SQLite/NATS stores、HTTP routes、SSE、mailbox workers 和 protocol replay。 |

## Awaken 的独特价值

- **一个 agent 后端，多种客户端。** AI SDK v6、AG-UI / CopilotKit、A2A、MCP、ACP 都是同一条 runtime event stream 和 run model 上的适配层，不需要为每个协议重写 agent。
- **托管配置就是控制面。** Provider、`ModelSpec`、model pool、agent、tool、插件 section、MCP server 都可以在服务运行中校验并发布成新的 registry snapshot。
- **模型与 provider 运维是内建能力。** `ModelSpec` 同时承载寻址、capability bounds、modalities、knowledge cutoff 与定价；model pool 负责故障切换；provider discovery 只在安全边界内补全能力字段，custom adapter 默认不会被当成可信 discovery 来源。
- **流式输出按生产 I/O 处理。** mid-stream 中断与 idle stall 触发类型化恢复方案，尊重 `Retry-After`，并可通过 `StreamCheckpointStore` 跨进程恢复。([详情](https://awakenworks.github.io/awaken/zh-cn/how-to/recover-streaming-llms))
- **状态与工具执行可类型化、可回放。** 类型化 `StateKey` + 合并策略，`TypedTool` 自动生成 JSON Schema，纯 `ToolGate` 拦截，phase 级原子提交，让并发工具调用有审计边界而不是隐藏副作用。
- **运维边界显式化。** 父子 thread、HITL mailbox 暂停、取消、audit log restore、凭据遮蔽、admin config validation 都是 runtime/server 契约的一部分。

## 心智模型

Awaken 把**写一次的代码**和**持续调优的配置**分开。

**代码层（Rust）：**

1. **Tools** — 直接实现 `Tool`，或用 `TypedTool` 通过 `schemars` 生成 JSON Schema。这是 agent 里唯一需要重新编译的部分。
2. **State** — `run`/`thread` 作用域的类型化状态，加上跨 thread/agent 协作用的持久 profile 与 shared state。
3. **Plugins** — 覆盖 permission、可观测性、上下文管理、Skills、MCP、Generative UI 的生命周期钩子。

**配置层（声明式，运行时热替换）：**

4. **Providers + Models** — 凭据、adapter，以及 agent 引用的 `ModelSpec`（含寻址、capabilities、定价）。
5. **Agents** — 系统提示词、`model_id`、允许/排除的工具集。LLM 用自然语言编排，没有 DAG。
6. **Skills** — 可发现的能力包，限定 agent 在特定任务下激活哪些工具和指令（`SkillSpec.allowed_tools`）。

工具一次写好就基本稳定；模型、agent、skill 通过 `/v1/config/*` 或[管理控制台](https://awakenworks.github.io/awaken/zh-cn/reference/admin-console/)**在运行时**调优 —— Validate → Save → 预览对话 → 调整。这套反馈环本身**就是**优化流程。

高价值的调优面包括系统提示词、工具描述覆盖、system reminder、ToolSearch / deferred-tool 策略、Skill 目录与激活元数据、插件 section，以及显式 sub-agent delegates。这些都是行为配置，不是任意代码执行：ToolSearch 由 `awaken-ext-deferred-tools` 实现；Skill 通过 catalog 注入并由 `skill` 工具激活；sub-agent 通过 `AgentSpec.delegates` 显式声明并暴露为 delegate tools。当前没有单独发布 SkillSearch 或 AgentSearch 工具。

当 server 挂接 audit store 与 versioned-registry store 后，配置写入可以通过 record revision 与 audit restore 追溯；已发布 runtime registry snapshot 是不可变的；durable run 会携带 `resolution_id`，让 resume/replay 重新选择同一个已发布 graph。手动把任意配置版本 pin 为生产版本属于 server/versioned-registry 能力边界，不是通用 runtime API。

runtime 每轮跑 9 个类型化 phase，其中包含一个纯判定的 `ToolGate`；状态变更在每轮结束时批量原子提交。

## 上手：runtime 模式

**前置条件：** Rust 1.93+ 和一个 OpenAI 兼容的 API Key。

```toml
[dependencies]
awaken = { git = "https://github.com/AwakenWorks/awaken" }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde_json = "1"
```

这些示例跟随当前 main 分支 API。从已发布 `0.5` 版本线升级时，请阅读
[0.5 到 0.6 迁移指南](https://awakenworks.github.io/awaken/zh-cn/how-to/migrate-to-0-6/)。

```bash
export OPENAI_API_KEY=<your-key>
```

`src/main.rs`（`cargo run` 启动）：

```rust
use awaken::engine::GenaiExecutor;
use awaken::prelude::*;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echo input back to the caller").with_parameters(json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }))
    }

    async fn execute(&self, args: JsonValue, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let text = args["text"].as_str().unwrap_or_default();
        Ok(ToolResult::success("echo", json!({ "echoed": text })).into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(
            AgentSpec::new("assistant")
                .with_model_id("gpt-4o-mini")
                .with_system_prompt("你是助手；用户请求时调用 echo 工具。")
                .with_max_rounds(5),
        )
        .with_tool("echo", Arc::new(EchoTool))
        .with_provider("openai", Arc::new(GenaiExecutor::new()))
        .with_model(ModelSpec::new("gpt-4o-mini", "openai", "gpt-4o-mini"))
        .build()?;

    let request = RunActivation::new("thread-1", vec![Message::user("用 echo 工具说一句 hello")])
        .with_agent_id("assistant");

    let result = runtime.run_to_completion(request).await?;
    println!("{}", result.response);
    Ok(())
}
```

需要流式事件（SSE / WebSocket / 协议适配器 / 测试）时，把 `run_to_completion`
换成 `runtime.run(request, sink)`。更完整的多轮 + 持久化 thread 示例见
[`crates/awaken/examples/multi_turn.rs`](./crates/awaken/examples/multi_turn.rs)。

无网络覆盖测试：

```bash
cargo test -p awaken --test readme_quickstart        # 离线（脚本化 provider）
OPENAI_API_KEY=<key> cargo test -p awaken --test readme_live_provider -- --ignored  # 真实 provider
```

## server 模式：通过任意协议提供服务

把 runtime 包成 HTTP/stdio 之后，同一个 agent 同时服务 React、Next.js、A2A 对端、MCP 客户端与 ACP 宿主，无需改动 agent 代码。server 模式在 runtime 外增加服务层能力：

- thread、run、config、capabilities、health 的 HTTP resources。
- SSE stream/replay，以及 AI SDK v6、AG-UI、A2A、MCP、ACP 协议适配。
- 持久 mailbox 派发，支持可恢复、可取消、可中断和 HITL 阻塞的 run。
- 托管配置 API 和管理控制台工作流，用于校验、预览、发布、恢复、审计 agent/model/provider/plugin 配置。
- 可选 server 模块：canonical events、trace persistence、eval datasets/runs、system discovery、runtime stats 与 run summaries。

三个组件夹在 runtime 与 wire 之间：

- `ThreadRunStore` — 持久化 thread 消息与 run 记录（memory / file / PostgreSQL 在 `awaken-stores` 里）。
- `Mailbox` — 持久 run 队列，把 HTTP 请求与 agent 执行解耦（memory / SQLite / NATS 可插拔）。
- `ServerState` — 每个路由 handler 读取的依赖捆绑。

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

let state = ServerState::new(
    runtime.clone(),
    mailbox,
    store,
    runtime.resolver_arc(),
    ServerConfig::default(),
);
serve(state).await?;
```

#### 协议适配器

| 协议 | 路由 / transport | 常见客户端 |
|---|---|---|
| AI SDK v6 | `POST /v1/ai-sdk/chat` | React `useChat()` |
| AG-UI | `POST /v1/ag-ui/run` | CopilotKit `<CopilotKit>` |
| A2A | `POST /v1/a2a/message:send` | 其他 Agent |
| MCP | `POST /v1/mcp` | JSON-RPC 2.0 客户端 |
| ACP | stdio via `serve_stdio` | Agent Client Protocol 宿主 |

可选的 admin console 读取 `/v1/capabilities`、写入 `/v1/config/*`，在浏览器里
管理 agents、models、providers、MCP servers 和插件配置 section。它还提供
server-managed Admin Assistant（`/v1/admin/assistant/runs`）：该助手可以读取平台能力、
创建并发布 AgentSpec、只生成草稿、校验配置，但绑定的是锁定的 admin-only 工具，不会进入普通
tool registry。配置第一个 provider-backed model 后它会自动启用。插件通过同一套 `PluginConfigKey` 暴露 schema，因此保存 `permission`、
`reminder`、`generative-ui`、`deferred_tools` 等 section 后会发布新的 registry snapshot，
对下一次 `/v1/runs` 立即生效。BigModel 等 OpenAI 兼容服务使用 `openai`
adapter + 对应 `base_url`；非密的扩展项放到 `ProviderSpec.adapter_options`。

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

#### 管理控制台

把 `ConfigStore` 接入 `ServerState` 后，[`apps/admin-console`](./apps/admin-console/) 就变成同一套配置 API 上的浏览器控制面（通过 `VITE_BACKEND_URL` 读服务端地址）。运维可以校验草稿，调优 prompt、工具描述、reminder、deferred-tool 策略、skills 和 delegates，发布 registry snapshot，测试 provider，查看 runtime 健康，在保存前预览 agent 修改，并从 audit log 恢复历史配置。首页优先展示真实运维信号：等待 HITL 决策、运行/排队负载、provider/MCP 健康、滚动窗口推理/错误/token 统计，以及最近审计事件。

下面的截图是使用 sample API data 生成的静态文档图。实际运行中的管理控制台会从配置的后端 API 读取这些值。

<table>
  <tr>
    <td width="33%"><a href="./docs/assets/admin-console/01-dashboard.png"><img src="./docs/assets/admin-console/01-dashboard.png" alt="管理控制台 Dashboard：实时负载、Agent 活动、最近审计事件、健康状态和系统元数据" /></a></td>
    <td width="33%"><a href="./docs/assets/admin-console/02-agent-editor.png"><img src="./docs/assets/admin-console/02-agent-editor.png" alt="Agent 编辑器：模型、系统提示词、草稿预览和保存控制" /></a></td>
    <td width="33%"><a href="./docs/assets/admin-console/03-agents-list.png"><img src="./docs/assets/admin-console/03-agents-list.png" alt="Agents 列表：筛选、插件元数据和推理统计" /></a></td>
  </tr>
  <tr>
    <td align="center"><sub><b>Dashboard</b><br/>负载 · 健康 · 最近审计</sub></td>
    <td align="center"><sub><b>Agent Editor</b><br/>校验 · 预览 · 保存</sub></td>
    <td align="center"><sub><b>Agents</b><br/>筛选 · 插件 · Runtime stats</sub></td>
  </tr>
</table>

完整界面说明：[管理控制台参考](https://awakenworks.github.io/awaken/zh-cn/reference/admin-console) · 操作手册：[使用管理控制台](https://awakenworks.github.io/awaken/zh-cn/how-to/use-admin-console)。

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
| **Deferred Tools** | 把大体量工具 schema 藏在 `ToolSearch` 后，用折扣 Beta 用量模型把空闲工具重新延迟。 | 直接依赖：`awaken-ext-deferred-tools` |

自定义工具拦截用 `ToolGateHook`（纯 gate 决策）或 `BeforeToolExecute`（执行时钩子），跟内置插件共用 trait 签名。

## 适合的场景

- 想用 **Rust 后端**写 AI Agent，要编译期保证。
- 需要从一个 backend 同时服务 **AI SDK、CopilotKit、A2A、MCP 或 ACP**。
- 工具需要在并发中**安全共享状态**，run 需要**可审计历史 + checkpoint + 可恢复控制路径**。
- 可以接受自己注册工具与 provider，而不是依赖开箱即用的默认能力。

## 不适合的场景

- 想要**开箱即用的文件 / Shell / Web 工具** — 看 OpenAI Agents SDK、Dify、CrewAI。
- 想要**可视化工作流编辑器** — 看 Dify、LangGraph Studio。
- 想要 **Python** 快速原型开发 — 看 LangGraph、AG2、PydanticAI。
- 想要 **LLM 自主管理记忆**（让 Agent 自行决定记住什么）— 看 Letta。

## 架构

门面 crate 下三层核心、外加存储与扩展两个分支：

```text
awaken                   门面 crate，管理 feature flag
├─ awaken-runtime-contract runtime 契约：spec、tool、event、state、commit coordinator
├─ awaken-server-contract  server/store 契约：query、scoped store、mailbox/outbox、staged commit
├─ awaken-runtime        resolver、phase 引擎、loop runner、runtime 控制
├─ awaken-server         HTTP 路由、SSE 回放、mailbox 派发、协议适配器
├─ awaken-stores         thread + run + config + mailbox + profile 存储（memory / file / PostgreSQL / SQLite / NATS）
├─ awaken-tool-pattern   扩展使用的 glob/regex 匹配
└─ awaken-ext-*          可选插件（permission、reminder、observability、mcp、skills、generative-ui、deferred-tools）
```

`awaken-server` 是服务编排和控制面层：HTTP、SSE replay、mailbox 后台 run、协议适配器、托管配置 API 和管理控制台工作流都在这里。它调用 `awaken-runtime` 这个进程内执行核心；runtime 把 `AgentSpec` 解析成本地 `ResolvedAgent` 或 backend-backed 执行计划，跑 9 段 phase loop，并管理 cancel + HITL 决策。

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
# 可选：为演示 seed 示例 agent / tool
AWAKEN_SEED_PROFILE=demo AWAKEN_STORAGE_DIR=./target/admin-sessions cargo run -p ai-sdk-starter-agent

# 终端 2：admin console
pnpm install
pnpm --filter awaken-admin-console dev
```

| 目标 | 从这里开始 | 然后 |
|---|---|---|
| 构建第一个 Agent | [快速上手](https://awakenworks.github.io/awaken/zh-cn/get-started) | [构建 Agent 路径](https://awakenworks.github.io/awaken/zh-cn/build-agents) |
| 查看全栈应用 | [AI SDK starter](./examples/ai-sdk-starter/) | [CopilotKit starter](./examples/copilotkit-starter/) |
| 管理运行时配置 | [Admin Console](./apps/admin-console/) | [通过配置调优 Agent 行为](https://awakenworks.github.io/awaken/zh-cn/how-to/configure-agent-behavior) |
| 探索 API | [参考文档](https://awakenworks.github.io/awaken/zh-cn/reference/overview) | `cargo doc --workspace --no-deps --open` |
| 理解运行时 | [架构](https://awakenworks.github.io/awaken/zh-cn/explanation/architecture) | [Run 生命周期与 Phases](https://awakenworks.github.io/awaken/zh-cn/explanation/run-lifecycle-and-phases) |

## 参与贡献

流程见 [CONTRIBUTING.md](./CONTRIBUTING.md)。[good first issues](https://github.com/AwakenWorks/awaken/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) 是入门标签。特别欢迎：额外的存储后端（Redis、S3 等）、内置文件 / Web / Shell 工具、Token 用量与预算、模型降级链。讨论：[GitHub Discussions](https://github.com/AwakenWorks/awaken/discussions)。

## 鸣谢

crates.io 上 `awaken` 这个名字是 [@brayniac](https://github.com/brayniac) 转让过来的：他原先维护着同名的另一个 crate。`awaken` 的 `0.1`–`0.3` 属于那个早期项目；本仓库的发版历史延续自之前的 `awaken-agent 0.2.x`，从 `0.4.0` 起步以跳过此前的版本号。感谢。


## 许可证

双重许可：[MIT](./LICENSE-MIT) 或 [Apache-2.0](./LICENSE-APACHE)。
