[English](../README.md) | **中文**

# Tirea

**用 Rust 构建 AI 智能体，连接任意前端，轻松部署到生产环境。**

用 Rust 定义智能体、工具和状态，然后通过单一二进制文件将它们通过 AG-UI、AI SDK v6、A2A 和 MCP 协议提供给 React、Next.js、CopilotKit 或其他智能体使用。

[![Crates.io](https://img.shields.io/crates/v/tirea.svg)](https://crates.io/crates/tirea)
[![docs.rs](https://img.shields.io/docsrs/tirea)](https://docs.rs/tirea)
[![License](https://img.shields.io/crates/l/tirea)](LICENSE-MIT)

<p align="center">
  <img src="./assets/demo.svg" alt="Tirea demo — 工具调用 + LLM 流式响应" width="800">
</p>

## 你能构建什么

构建组件——工具、插件、智能体——然后将它们组装成一个 `AgentOs`。AgentOs 是一个容器，智能体由你注册的组件组合而成。

```rust
// 1. 构建工具 — 将参数定义为结构体，Schema 自动生成
#[derive(Deserialize, JsonSchema)]
struct SearchFlightsArgs {
    from: String,
    to: String,
    date: String,
}

struct SearchFlightsTool;

#[async_trait]
impl TypedTool for SearchFlightsTool {
    type Args = SearchFlightsArgs;
    fn tool_id(&self) -> &str { "search_flights" }
    fn name(&self) -> &str { "Search Flights" }
    fn description(&self) -> &str { "Find flights between two cities." }

    async fn execute(&self, args: SearchFlightsArgs, _ctx: &ToolCallContext<'_>)
        -> Result<ToolResult, ToolError>
    {
        // ... call your flight API ...
        Ok(ToolResult::success("search_flights", json!({
            "flights": [{"airline": "UA", "price": 342, "from": args.from, "to": args.to}]
        })))
    }
}

// 2. 定义智能体 — 每个智能体自行声明可使用的工具/技能/子智能体
let planner = AgentDefinition::with_id("planner", "deepseek-chat")
    .with_system_prompt("You are a travel planner. Use search tools to find options.")
    .with_max_rounds(8)
    .with_allowed_tools(vec!["search_flights".into(), "search_hotels".into()])
    .with_allowed_agents(vec!["researcher".into()]);

let researcher = AgentDefinition::with_id("researcher", "deepseek-chat")
    .with_system_prompt("You research destinations and provide summaries.")
    .with_max_rounds(4)
    .with_excluded_tools(vec!["delete_account".into()]);

// 3. 组装成 AgentOs — 所有组件的容器
let os = AgentOsBuilder::new()
    .with_tools(tool_map_from_arc(vec![
        Arc::new(SearchFlightsTool),
        Arc::new(SearchHotelsTool),
    ]))
    .with_agent_spec(AgentDefinitionSpec::local(planner))
    .with_agent_spec(AgentDefinitionSpec::local(researcher))
    .with_agent_state_store(Arc::new(FileStore::new("./sessions")))
    .build()?;
```

工具在 AgentOs 上全局注册，每个智能体通过 `allowed_*` / `excluded_*` 列表定义自己的访问策略——决定可以使用哪些工具、技能和子智能体。运行时在解析时将全局工具池过滤为每个智能体有权访问的子集。

通过 `useChat()` 连接 React 前端、通过 AG-UI 连接 CopilotKit 应用，或通过 A2A 连接另一个智能体——无需修改任何代码。

## 与众不同之处

| 特性 | 为什么重要 |
|---|---|
| **一个服务器，四种协议** | 同一个二进制文件同时提供 UI 协议（AG-UI、AI SDK v6）和智能体协议（A2A、MCP），无需单独部署。 |
| **并发安全的状态** | 多个智能体可以同时写入同一状态。CRDT 字段（`GSet`、`ORSet`、`GCounter`）自动合并——无锁、无冲突。 |
| **按生命周期划分的状态作用域** | 将状态标记为 Thread 作用域（永久持久化）、Run 作用域（每次运行时重置）或 ToolCall 作用域（工具执行结束后销毁），避免过期数据在多次运行间泄漏。 |
| **编译期插件安全性** | 插件挂载到 8 个生命周期阶段。将权限检查接入错误的阶段？编译器直接报错。 |
| **回放任意对话** | 每次状态变更都是不可变补丁，可以重放以精确还原任意时间点的状态。 |
| **Rust 性能** | 无 GC 停顿，内存占用低，原生异步并发。 |

## 功能对比

|  | Tirea | LangGraph | CrewAI | OpenAI Agents | Mastra | PydanticAI | Letta |
|---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| **语言** | Rust | Python | Python | Python/TS | TypeScript | Python | Python |
| **多协议服务器** | 4 (2 UI + 2 Agent) | ❌ | ❌ | ❌ | ❌ | AG-UI | REST |
| **类型化状态** | ✅ derive 宏 | ◐ | ❌ | ❌ | ◐ | ◐ | ❌ |
| **并发状态安全** | ✅ CRDT | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **状态生命周期作用域** | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **状态回放** | ✅ | ◐ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **插件生命周期** | 8 个类型化阶段 | ❌ | ❌ | Guardrails | ❌ | ❌ | ❌ |
| **子智能体** | ✅ | ✅ | ✅ | Handoffs | ◐ | ◐ | ✅ |
| **MCP 支持** | ✅ | Adapter | ✅ | ✅ | ✅ | ✅ | ❌ |
| **Human-in-the-loop** | ✅ | ✅ | ❌ | ✅ | ❌ | ✅ | ❌ |
| **内置通用工具** | ❌ | ❌ | ✅ | ❌ | ❌ | ❌ | ✅ |

✅ = 原生支持  ◐ = 部分支持  ❌ = 不支持

## 快速开始

### 前置条件

- 来自 [`rust-toolchain.toml`](../rust-toolchain.toml) 的 Rust 工具链
- 前端演示需要：Node.js 20+ 和 npm
- 任意一个模型提供商的 API Key（OpenAI、DeepSeek、Anthropic 等）

### 60 秒内运行全栈演示

**React + AI SDK v6：**

```bash
cd examples/ai-sdk-starter && npm install
DEEPSEEK_API_KEY=<your-key> npm run dev
# Open http://localhost:3001
```

**Next.js + CopilotKit：**

```bash
cd examples/copilotkit-starter && npm install
cp .env.example .env.local
DEEPSEEK_API_KEY=<your-key> npm run setup:agent && npm run dev
# Open http://localhost:3000
```

### 仅启动服务器

```bash
export OPENAI_API_KEY=<your-key>
cargo run --package tirea-agentos-server -- --http-addr 127.0.0.1:8080
```

## 工作原理

```mermaid
graph LR
    subgraph "Your frontends"
        A["React app\n(AI SDK v6)"]
        B["Next.js app\n(CopilotKit / AG-UI)"]
        C["Another agent\n(A2A)"]
    end

    subgraph "tirea server (one binary)"
        GW["Protocol gateway\nUI: AG-UI · AI SDK\nAgent: A2A · MCP"]
        RT["Agent runtime\nLLM streaming · tool dispatch\nplugin lifecycle · context mgmt"]
        EXT["Extensions\npermission · skills · MCP\nreminder · observability"]
    end

    subgraph "Storage (pick one)"
        S1[(File)]
        S2[(PostgreSQL)]
    end

    A & B --> GW
    C --> GW
    GW --> RT
    RT --> EXT
    RT --> S1 & S2
```

## 能做什么

### 连接任意前端

一个后端从同一个二进制文件提供多种协议，切换时无需修改代码：

**UI 协议** — 将前端连接到你的智能体：

- **AG-UI**（CopilotKit）— 共享状态、前端操作、生成式 UI、human-in-the-loop
- **AI SDK v6**（Vercel）— `useChat()` 流式传输、画布、对话历史

**智能体协议** — 将智能体互联：

- **A2A** — Google 的 agent-to-agent 协议，将你的智能体暴露为对等服务
- **MCP** — 连接 MCP 服务器，外部工具以原生工具形式呈现

**端点** — 启动服务器后，从任意前端连接：

```bash
cargo run --package tirea-agentos-server -- --http-addr 127.0.0.1:8080
```

| 协议 | 端点 | 前端 |
|---|---|---|
| AI SDK v6 | `POST /v1/ai-sdk/agents/:agent_id/runs` | React `useChat()` |
| AG-UI | `POST /v1/ag-ui/agents/:agent_id/runs` | CopilotKit `<CopilotKit>` |

**React + AI SDK v6** — 极简前端：

```typescript
import { useChat } from "ai/react";

const { messages, input, handleSubmit } = useChat({
  api: "http://localhost:8080/v1/ai-sdk/agents/assistant/runs",
});
```

**Next.js + CopilotKit** — 极简前端：

```typescript
import { CopilotKit } from "@copilotkit/react-core";

<CopilotKit runtimeUrl="http://localhost:8080/v1/ag-ui/agents/assistant/runs">
  <YourApp />
</CopilotKit>
```

### 添加工具

将参数定义为类型化结构体——JSON Schema 由 `JsonSchema` 自动生成，参数也会自动反序列化：

```rust
#[derive(Deserialize, JsonSchema)]
struct MyToolArgs {
    query: String,
    limit: Option<u32>,
}

struct MyTool;

#[async_trait]
impl TypedTool for MyTool {
    type Args = MyToolArgs;
    fn tool_id(&self) -> &str { "my_tool" }
    fn name(&self) -> &str { "My Tool" }
    fn description(&self) -> &str { "Does something useful." }

    async fn execute(&self, args: MyToolArgs, ctx: &ToolCallContext<'_>)
        -> Result<ToolResult, ToolError>
    {
        // Read current state
        let state = ctx.snapshot_of::<MyState>().unwrap_or_default();

        // Do work
        let result = my_api_call(&args.query, args.limit).await?;

        // Return result (optionally with state updates)
        Ok(ToolResult::success("my_tool", json!(result)))
    }
}
```

### 内置工具

Tirea 内置了用于子智能体、后台任务、技能、UI 渲染和 MCP 集成的工具。启用对应的 feature 后，它们会自动注册：

| 工具组 | Tools | 功能描述 |
|---|---|---|
| **子智能体**（核心） | `agent_run`, `agent_stop`, `agent_output` | 并行启动、取消子智能体并读取其结果 |
| **后台任务**（核心） | `task_status`, `task_cancel`, `task_output` | 监控和管理长时间运行的后台操作 |
| **技能**（`skills` feature） | `skill`, `load_skill_resource`, `skill_script` | 发现、激活并执行技能包 |
| **A2UI**（`a2ui` 扩展） | `render_a2ui` | 向前端发送声明式 UI 组件 |
| **MCP**（`mcp` feature） | *动态* | 来自已连接 MCP 服务器的工具以原生工具形式呈现 |

### 为什么需要插件？仅靠工具还不够

工具只是 LLM 可以调用的函数，但单纯的工具在实践中行不通。

**LLM 不知道工具的存在。** `agent_run` 工具可以启动子智能体，但 LLM 不会去调用它，除非系统提示中列出了哪些智能体可用。这种上下文注入不是工具的职责，`AgentToolsPlugin` 通过在每次推理前注入智能体目录来解决这个问题。

同样的模式随处可见：`SkillDiscoveryPlugin` 注入技能目录，让 LLM 知道可以激活哪些技能；`BackgroundTasksPlugin` 注入任务状态，让 LLM 了解哪些任务正在运行；`A2uiPlugin` 注入 UI Schema，让 LLM 知道如何渲染组件。

**横切关注点无法在单个工具内解决：**

| 问题 | 为何工具无法解决 | 插件方案 |
|---|---|---|
| 权限控制 | 每个工具都要重新实现鉴权 | `PermissionPlugin` — 对所有工具统一提供一个 `before_tool_execute` 钩子 |
| Token 预算 | 单个工具无法看到完整的消息历史 | `ContextPlugin` — 跨所有消息进行截断、摘要和缓存 |
| 停止条件 | 没有工具知道何时应该停止智能体循环 | `StopPolicyPlugin` — 在每次推理后评估最大轮次、超时和预算 |
| 可观测性 | 延迟/Token 跨度跨越工具边界 | `LLMMetryPlugin` — 对完整的 LLM + 工具流水线生成 OpenTelemetry span |
| 持久化提醒 | 提醒需要跨轮次存活，不依附于某个工具 | `ReminderPlugin` — 在每次推理前注入提醒 |
| 孤儿恢复 | 子智能体可能在父进程退出后继续存在 | `AgentRecoveryPlugin` — 在重启时检测并恢复孤立的运行实例 |

这就是每个内置工具都附带一个伴生插件的原因。工具提供能力，插件将其接入 LLM 的感知范围和运行时生命周期。

### 在危险操作前要求审批

内置的 `PermissionPlugin` 通过 `PermissionPolicy` 状态（每个工具的 Allow/Deny/Ask 策略）检查工具权限。你也可以编写自定义插件，完全控制暂停流程来拦截任意工具：

```rust
// In your plugin's before_tool_execute:
async fn before_tool_execute(&self, ctx: &ReadOnlyContext<'_>)
    -> ActionSet<BeforeToolExecuteAction>
{
    let tool_id = ctx.tool_name().unwrap_or_default();
    let call_id = ctx.tool_call_id().unwrap_or_default();

    if tool_id == "delete_account" {
        let pending_id = format!("fc_{call_id}");
        let tool_args = ctx.tool_args().cloned().unwrap_or_default();
        let suspension = Suspension::new(&pending_id, "confirm_delete")
            .with_message("Requires admin approval");
        let pending = PendingToolCall::new(pending_id, "Confirm", tool_args);
        ActionSet::single(BeforeToolExecuteAction::Suspend(
            SuspendTicket::new(suspension, pending, ToolCallResumeMode::ReplayToolCall)
        ))
    } else {
        ActionSet::empty()
    }
}
```

前端会收到带有待处理调用的暂停事件。当用户批准后，运行时会重放原始工具调用——这次将跳过权限检查。

### 多智能体协作

多智能体编排是核心能力。在构建时注册智能体——运行时将智能体目录注入系统提示，编排者通过内置工具进行委派：

```rust
let orchestrator = AgentDefinition::with_id("orchestrator", "deepseek-chat")
    .with_system_prompt("Route tasks to the right agent.")
    .with_allowed_agents(vec!["researcher".into(), "writer".into()]);

let researcher = AgentDefinition::with_id("researcher", "deepseek-chat")
    .with_system_prompt("Research topics and return summaries.")
    .with_excluded_tools(vec!["agent_run".into()]); // no further delegation

let os = AgentOsBuilder::new()
    .with_agent_spec(AgentDefinitionSpec::local(orchestrator))
    .with_agent_spec(AgentDefinitionSpec::local(researcher))
    // Remote agents via A2A protocol
    .with_agent_spec(AgentDefinitionSpec::a2a_with_id(
        "writer",
        A2aAgentBinding::new("https://writer-service.example.com/v1/a2a", "writer-v2"),
    ))
    .build()?;
```

**委派工具** — 每个子智能体在自己独立的线程中运行：

- `agent_run` — 通过 `agent_id` 启动（前台或后台），或通过 `run_id` 恢复
- `agent_stop` — 取消正在运行的子智能体（级联至所有后代）
- `agent_output` — 从子智能体的线程中读取其运行结果

**支持的协作模式：**

| 模式 | 工作方式 |
|---|---|
| **协调者** | 编排者分析意图，路由到合适的专家 |
| **流水线** | 智能体顺序执行——每个智能体对前一个的输出进行转换 |
| **并行扇出** | 编排者并发启动多个智能体，汇总结果 |
| **层级式** | 父级分解任务 → 子级进一步分解 → 递归委派 |
| **生成-批评** | 生成者起草，批评者验证，生成者在循环中修订 |

**前台与后台：** `agent_run(background=false)` 阻塞直到子智能体完成（进度实时回传）。`agent_run(background=true)` 立即返回一个 `run_id`——稍后通过 `agent_output` 检查状态。

**本地 + 远程智能体：** 本地智能体在进程内运行。远程智能体通过 A2A 协议经 HTTP 通信——对编排者透明，使用相同的 `agent_run` 接口。

智能体必须在构建器中预先定义。可见性通过 `allowed_agents` / `excluded_agents` 进行策略控制。孤立的子智能体会在重启时自动恢复。详见[多智能体设计模式指南](https://tirea-ai.github.io/tirea/explanation/multi-agent-design-patterns.html)。

### 跨对话管理状态

状态是类型化的，并根据其预期生命周期划定作用域：

```rust
#[derive(State)]
#[tirea(scope = "thread")]   // persists across all runs in this conversation
struct UserPreferences { /* ... */ }

#[derive(State)]
#[tirea(scope = "run")]      // reset at the start of each agent run
struct SearchProgress { /* ... */ }

#[derive(State)]
#[tirea(scope = "tool_call")] // exists only during a single tool execution
struct ToolWorkspace { /* ... */ }
```

标记为 `#[tirea(lattice)]` 的字段使用 CRDT 类型，当多个智能体并发写入时自动合并——无需加锁。

### 持久化对话

无需修改智能体代码即可切换存储后端：

| 后端 | 适用场景 |
|---|---|
| `FileStore` | 本地开发、单机部署 |
| `PostgresStore` | 生产环境，支持 SQL 查询和备份 |
| `MemoryStore` | 测试 |

### 通过插件扩展功能

插件挂载到 8 个生命周期阶段。可使用内置插件或自行编写：

| 插件 | 功能 | 启用方式 |
|---|---|---|
| **Context** | Token 预算、消息摘要、Prompt 缓存 | `ContextPlugin::for_model("claude-3-5-sonnet")` |
| **Stop Policy** | 按最大轮次、超时、Token 预算、循环检测终止 | `StopPolicyPlugin::new(conditions, specs)` |
| **Permission** | 按工具 Allow/Deny/Ask，human-in-the-loop 暂停 | `PermissionPlugin` + `ToolPolicyPlugin` |
| **Skills** | 从文件系统发现并激活技能包 | `skills` feature flag |
| **MCP** | 连接 MCP 服务器，工具以原生形式呈现 | `mcp` feature flag |
| **Reminder** | 跨轮次持久化的系统提醒 | `ReminderPlugin::new()` |
| **Observability** | LLM 调用和工具执行的 OpenTelemetry span | `LLMMetryPlugin::new(sink)` |
| **A2UI** | 向前端发送声明式 UI 组件 | `A2uiPlugin::with_catalog_id(url)` |
| **Agent Recovery** | 检测并恢复孤立的子智能体运行实例 | 随子智能体自动接入 |
| **Background Tasks** | 追踪并注入后台任务状态 | 随任务工具自动接入 |

### 使用任意 LLM 提供商

基于 [genai](https://crates.io/crates/genai) 构建——支持 OpenAI、Anthropic、DeepSeek、Google、Mistral、Groq、Ollama 等。只需修改一个字符串即可切换提供商：

```rust
model: "gpt-4o".into(),        // OpenAI
model: "deepseek-chat".into(), // DeepSeek
model: "claude-sonnet-4-20250514".into(), // Anthropic
```

## 适合使用 Tirea 的场景

- 你希望用 **Rust 后端**构建具备编译期安全性的 AI 智能体
- 你需要从一个服务器提供**多种前端协议**
- 你的智能体需要**无需协调地并发共享状态**
- 你需要**可审计的状态历史**和回放能力
- 你面向**生产环境**构建——低内存占用、无 GC、支持数千个并发智能体

## 不适合使用 Tirea 的场景

- 你需要开箱即用的**文件/Shell/网页工具**——可以考虑 Dify、CrewAI
- 你想要**可视化工作流构建器**——可以考虑 Dify、LangGraph Studio
- 你偏好 **Python** 和快速原型开发——可以考虑 LangGraph、PydanticAI
- 你需要 **LLM 管理的记忆**（由智能体决定记住什么）——可以考虑 Letta

## 学习路径

| 目标 | 从这里开始 | 然后 |
|---|---|---|
| 构建你的第一个智能体 | [First Agent 教程](https://tirea-ai.github.io/tirea/tutorials/first-agent.html) | [构建智能体指南](https://tirea-ai.github.io/tirea/how-to/build-an-agent.html) |
| 查看完整全栈应用 | [AI SDK starter](../examples/ai-sdk-starter/README.md) | [CopilotKit starter](../examples/copilotkit-starter/README.md) |
| 探索 API | [API 参考](https://tirea-ai.github.io/tirea/reference/api.html) | `cargo doc --workspace --no-deps --open` |
| 参与贡献 | [贡献指南](../CONTRIBUTING.md) | [能力矩阵](https://tirea-ai.github.io/tirea/reference/capability-matrix.html) |

## 示例

| 示例 | 展示内容 |
|---|---|
| [ai-sdk-starter](../examples/ai-sdk-starter/) | React + AI SDK v6 — 聊天、画布、共享状态 |
| [copilotkit-starter](../examples/copilotkit-starter/) | Next.js + CopilotKit — 持久化对话、前端操作 |
| [travel-ui](../examples/travel-ui/) | 地图画布 + 需审批的行程规划 |
| [research-ui](../examples/research-ui/) | 资源收集 + 需审批的报告撰写 |

## 文档

完整文档：<https://tirea-ai.github.io/tirea/> · [API 参考](https://docs.rs/tirea) · [文档源码](./book/src/)

## 贡献

详见 [CONTRIBUTING.md](../CONTRIBUTING.md)，欢迎贡献——特别期待以下方面：

- 内置工具实现（文件读写、搜索、Shell 执行）
- 工具级并发安全标志
- 模型降级/回退链
- Token 成本追踪
- 更多存储后端

## 许可证

双协议授权：[MIT](../LICENSE-MIT) 或 [Apache-2.0](../LICENSE-APACHE)。

[SECURITY.md](../SECURITY.md) · [CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md)
