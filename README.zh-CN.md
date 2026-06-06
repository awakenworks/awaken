# Awaken

[English](./README.md) | [中文](./README.zh-CN.md)

[![CI](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml/badge.svg)](https://github.com/AwakenWorks/awaken/actions/workflows/test.yml) [![crates.io awaken](https://img.shields.io/crates/v/awaken.svg?label=awaken)](https://crates.io/crates/awaken) [![crates.io awaken-agent](https://img.shields.io/crates/v/awaken-agent.svg?label=awaken-agent)](https://crates.io/crates/awaken-agent) [![Changelog](https://img.shields.io/badge/changelog-current-informational)](./CHANGELOG.md) ![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.93-orange)

用 Rust 构建 Agent 能力，在线调优 prompts、models、permissions、skills 和 eval loop，并让同一个 runtime 服务 AI SDK、AG-UI、A2A、MCP、ACP 客户端，而不是为每个场景写一套脆弱脚本。

在线文档：[Awaken docs（英文）](https://awakenworks.github.io/awaken) · [中文文档](https://awakenworks.github.io/awaken/zh-cn) · [Changelog](./CHANGELOG.md)。MSRV：Rust 1.93。发布的 crate 是 `awaken`；`awaken-agent` 是早期同名发布期的兼容包。

## 30 秒看懂

启动本地 server 和 Admin Console：

```sh
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
AWAKEN_ADMIN_API_BEARER_TOKEN=dev-token \
AWAKEN_STORAGE_DIR=./target/awaken-dev \
cargo run -p ai-sdk-starter-agent

pnpm --filter awaken-admin-console dev
```

打开 `http://127.0.0.1:3002`，填入 `dev-token`，配置 provider-backed model，然后创建或调优 Agent。没有 API key 时，starter backend 会使用 deterministic scripted executor，方便先验证 server routes 和控制台。

调优优先的核心循环是：

```text
校验草稿 -> 预览对话 -> 保存 snapshot -> 执行任务 -> 查看 trace -> 采集 dataset/eval -> 调整
```

## 为什么用 Awaken

- **代码保持稳定。** Tools、类型化 state、providers、stores、plugins 留在 Rust 代码里。
- **行为在线调优。** Prompts、model 绑定、工具描述、权限规则、reminders、skills、delegates、插件 sections 通过托管配置变更。
- **一个后端服务多种客户端。** AI SDK v6、AG-UI / CopilotKit、A2A、MCP、ACP 都是同一条 runtime event stream 和 run model 上的适配层。
- **Run 是可运营对象。** Durable dispatch、HITL mailbox 暂停、取消、trace、replay、datasets、eval runs、audit restore 都是 runtime/server 契约。
- **状态与工具类型化。** `StateKey`、`TypedTool` 自动生成 JSON Schema、纯 tool gate、原子提交，让并发工具执行可审计。

## 调优优先工作流

Awaken 把 Agent 行为变成受管理资源，而不是散落在代码里的临时改动。Server config 写入会经过校验、发布为 registry snapshot；接入 stores 后还能审计和恢复。

| 在线调优 | Awaken 管理 |
|---|---|
| Prompts、model 绑定、reasoning effort、停止策略 | 校验、预览、保存、发布给下一次 run 的 registry snapshot |
| 工具描述、允许/排除规则、权限 gates、reminders | 类型化 schema、策略校验、HITL 暂停/恢复 |
| Providers、models、model pools、MCP servers、skills | 能力 metadata、provider 检查、故障切换池、catalogs |
| Traces、datasets、eval runs、audit history | 可回放记录、baseline diff、可恢复配置 revision |

工具写一次后保持稳定。Models、agents、prompts、skills、delegates 和 policy sections 通过 `/v1/config/*` 或 [Admin Console](https://awakenworks.github.io/awaken/zh-cn/how-to/use-admin-console/) 调优：Validate → Save → 预览对话 → 调整。

## 选择模式

Awaken 把 **Agent 执行 loop** 和 **服务控制面**分开。

| 模式 | 从这里开始 | 你负责 | Awaken 提供 |
|---|---|---|---|
| **Runtime library** | `awaken` / `awaken-runtime` | HTTP/UI/job scheduling、auth、配置存储、具体 tools/providers/stores | 直接 run API、流式事件、类型化 tools/state、取消、tool gate、HITL primitives |
| **Server control plane** | `awaken-server` + `awaken-stores` | 部署、租户/auth 策略、已注册 tools/providers、store 选择 | HTTP/SSE、AI SDK/AG-UI/A2A/MCP/ACP adapters、mailbox 编排、`/v1/config/*`、registry snapshots、Admin Console |

Runtime 模式是标准 async Rust 程序里的进程内 library 使用，不是 `no_std` 或无 Tokio 的嵌入式目标。Server 模式在同一个 runtime 外层加上协议、durable dispatch、托管配置、审计/恢复、trace/eval 存储和浏览器工作流。

## Quickstart A：server + Admin Console

想先体验调优工作流时，从这里开始。

```sh
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
AWAKEN_ADMIN_API_BEARER_TOKEN=dev-token \
AWAKEN_STORAGE_DIR=./target/awaken-dev \
cargo run -p ai-sdk-starter-agent

pnpm install
pnpm --filter awaken-admin-console dev
```

打开 `http://127.0.0.1:3002`，点击 token pill，填入 `dev-token`。配置 provider/model，创建 Agent，预览并保存，然后从已保存 Agent 页面复制 AI SDK 或 AG-UI route。

相关文档：

- [快速上手](https://awakenworks.github.io/awaken/zh-cn/get-started/)
- [使用管理控制台](https://awakenworks.github.io/awaken/zh-cn/how-to/use-admin-console/)
- [通过配置调优 Agent 行为](https://awakenworks.github.io/awaken/zh-cn/how-to/configure-agent-behavior/)
- [采集数据集并运行评测](https://awakenworks.github.io/awaken/zh-cn/how-to/capture-a-dataset-and-run-an-eval/)

## Quickstart B：runtime library

当你的 Rust 应用自己拥有 I/O 边界并直接调用 runtime 时，从这里开始。

**前置条件：** Rust 1.93+ 和一个 OpenAI 兼容 API key。

```toml
[dependencies]
awaken = { git = "https://github.com/AwakenWorks/awaken" }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde_json = "1"
```

这些示例跟随当前 main 分支 API。从已发布 `0.5` 版本线升级时，请阅读 [0.5 到 0.6 迁移指南](https://awakenworks.github.io/awaken/zh-cn/how-to/migrate-to-0-6/)。

```bash
export OPENAI_API_KEY=<your-key>
```

`src/main.rs`：

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

需要把事件流式发送到 SSE、WebSocket、协议适配器或测试时，把 `run_to_completion` 换成 `runtime.run(request, sink)`。更完整示例见 [`crates/awaken/examples/multi_turn.rs`](./crates/awaken/examples/multi_turn.rs)。

无网络覆盖测试：

```bash
cargo test -p awaken --test readme_quickstart        # 离线 scripted provider
OPENAI_API_KEY=<key> cargo test -p awaken --test readme_live_provider -- --ignored  # live provider
```

## 协议

| 协议 | 路由 / transport | 常见客户端 |
|---|---|---|
| AI SDK v6 | `POST /v1/ai-sdk/chat` | React `useChat()` |
| AG-UI | `POST /v1/ag-ui/run` | CopilotKit `<CopilotKit>` |
| A2A | `POST /v1/a2a/message:send` | 其他 Agent |
| MCP | `POST /v1/mcp` | JSON-RPC 2.0 客户端 |
| ACP | stdio via `serve_stdio` | Agent Client Protocol 宿主 |

前端指南：[AI SDK](https://awakenworks.github.io/awaken/zh-cn/how-to/integrate-ai-sdk-frontend/) · [CopilotKit / AG-UI](https://awakenworks.github.io/awaken/zh-cn/how-to/integrate-copilotkit-ag-ui/) · [HTTP SSE](https://awakenworks.github.io/awaken/zh-cn/how-to/expose-http-sse/)。

## 扩展

门面 crate 的 `full` feature 会拉入下列插件。`default-features = false` 可按需关闭。`awaken-ext-deferred-tools` 是配套 crate，需要直接依赖。

| 扩展 | 作用 | Feature / crate |
|---|---|---|
| **Permission** | 基于工具名和参数的 Allow/Deny/Ask 规则；Ask 通过 mailbox 暂停等待 HITL。 | `permission` |
| **Reminder** | 工具调用匹配配置模式时注入上下文消息。 | `reminder` |
| **Observability** | 与 GenAI Semantic Conventions 对齐的 OpenTelemetry traces 和 metrics。 | `observability` |
| **MCP** | 连接外部 MCP server，并把其工具注册为 Awaken 原生工具。 | `mcp` |
| **Skills** | 发现 skill 包，并在推理前注入 catalog。 | `skills` |
| **Generative UI** | 通过 A2UI、JSON Render、OpenUI Lang 流式输出声明式 UI。 | `generative-ui` |
| **Deferred Tools** | 将大体量工具 schema 藏在 `ToolSearch` 后，并重新延迟空闲工具。 | `awaken-ext-deferred-tools` |

自定义扩展可使用 `ToolGateHook` 或 `BeforeToolExecute`，与内置插件共用 trait 签名。

## 架构

<p align="center">
  <img src="./docs/assets/demo.svg" alt="Awaken 演示 — 托管 Agent run、工具调用、审批与 trace" width="800">
</p>

```text
awaken                   门面 crate，管理 feature flags
├─ awaken-runtime-contract runtime 契约：spec、tool、event、state、commit coordinator
├─ awaken-server-contract  server/store 契约：query、scoped store、mailbox/outbox、staged commit
├─ awaken-runtime        resolver、phase engine、loop runner、runtime control
├─ awaken-server         HTTP routes、SSE replay、mailbox dispatch、protocol adapters
├─ awaken-stores         thread + run + config + mailbox + profile stores
├─ awaken-tool-pattern   扩展使用的 glob/regex 匹配
└─ awaken-ext-*          可选扩展和配套插件
```

详细说明见 [架构](https://awakenworks.github.io/awaken/zh-cn/explanation/architecture/) 和 [Run 生命周期与 Phases](https://awakenworks.github.io/awaken/zh-cn/explanation/run-lifecycle-and-phases/)。

## 适合的场景

- 想用 **Rust 后端**写 AI Agent，并保留编译期保证。
- 需要从一个 backend 同时服务 **AI SDK、CopilotKit、A2A、MCP 或 ACP**。
- 工具需要在并发中**安全共享状态**，run 需要可审计历史、checkpoint 和恢复。
- 需要让 operators 在不改代码的情况下调优 prompts、models、permissions、skills、traces、datasets 和 evals。

## 不适合的场景

- 想要**开箱即用的文件 / Shell / Web 工具** — 看 OpenAI Agents SDK、Dify、CrewAI。
- 想要**可视化工作流编辑器** — 看 Dify、LangGraph Studio。
- 想要 **Python** 快速原型开发 — 看 LangGraph、AG2、PydanticAI。
- 想要 **LLM 自主管理记忆**（让 Agent 自行决定记住什么）— 看 Letta。

## 示例与学习路径

| 目标 | 从这里开始 | 然后 |
|---|---|---|
| 构建第一个 Agent | [快速上手](https://awakenworks.github.io/awaken/zh-cn/get-started/) | [构建 Agent](https://awakenworks.github.io/awaken/zh-cn/build-agents/) |
| 调优已保存 Agent | [使用管理控制台](https://awakenworks.github.io/awaken/zh-cn/how-to/use-admin-console/) | [通过配置调优 Agent 行为](https://awakenworks.github.io/awaken/zh-cn/how-to/configure-agent-behavior/) |
| 查看全栈应用 | [AI SDK starter](./examples/ai-sdk-starter/) | [CopilotKit starter](./examples/copilotkit-starter/) |
| 探索 API | [参考文档](https://awakenworks.github.io/awaken/zh-cn/reference/overview/) | `cargo doc --workspace --no-deps --open` |
| 理解 runtime | [架构](https://awakenworks.github.io/awaken/zh-cn/explanation/architecture/) | [Run 生命周期与 Phases](https://awakenworks.github.io/awaken/zh-cn/explanation/run-lifecycle-and-phases/) |

示例：

| 示例 | 展示内容 |
|---|---|
| [`live_test`](./crates/awaken/examples/live_test.rs) | 基础 LLM 集成 |
| [`multi_turn`](./crates/awaken/examples/multi_turn.rs) | 多轮对话与持久化 thread |
| [`tool_call_live`](./crates/awaken/examples/tool_call_live.rs) | 工具调用（计算器） |
| [`ai-sdk-starter`](./examples/ai-sdk-starter/) | React + AI SDK v6 全栈 |
| [`copilotkit-starter`](./examples/copilotkit-starter/) | Next.js + CopilotKit 全栈 |
| [`openui-chat`](./examples/openui-chat/) | OpenUI Lang chat 前端 |
| [`admin-console`](./apps/admin-console/) | Config API 管理界面 |

## 参与贡献

流程见 [CONTRIBUTING.md](./CONTRIBUTING.md) 和 [DEVELOPMENT.md](./DEVELOPMENT.md)。[good first issues](https://github.com/AwakenWorks/awaken/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) 是入门标签。讨论：[GitHub Discussions](https://github.com/AwakenWorks/awaken/discussions)。

## 鸣谢

crates.io 上 `awaken` 这个名字是 [@brayniac](https://github.com/brayniac) 转让过来的：他原先维护着同名的另一个 crate。`awaken` 的 `0.1`–`0.3` 属于那个早期项目；本仓库的发版历史延续自之前的 `awaken-agent 0.2.x`，从 `0.4.0` 起步以跳过此前的版本号。感谢。

## 许可证

双重许可：[MIT](./LICENSE-MIT) 或 [Apache-2.0](./LICENSE-APACHE)。
