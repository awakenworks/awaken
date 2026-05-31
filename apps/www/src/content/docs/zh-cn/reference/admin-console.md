---
title: "管理控制台"
description: "使用 Awaken 管理控制台配置 Provider、Model、Agent、工具、MCP server、Trace、Dataset、Eval 和内置 Admin Assistant。"
---

管理控制台是运行中 `awaken-server` 的浏览器控制面。你可以用它在线创建和调优
Agent，而不是重新编译 Rust 二进制：配置 provider 和 model，编辑 prompt 与工具描述，
分配 MCP 工具，调优 reminder 与 deferred-tool 策略，预览草稿，然后发布下一版
registry snapshot。

## 启动

本地 deterministic scripted server：

```sh
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
AWAKEN_ADMIN_API_BEARER_TOKEN=dev-token \
AWAKEN_STORAGE_DIR=./target/admin-sessions \
cargo run -p ai-sdk-starter-agent
```

另开一个终端：

```sh
pnpm install
pnpm --filter awaken-admin-console dev
```

打开 `http://127.0.0.1:3002`，点击 topbar 的 token pill，填入 `dev-token`。
后端默认是 `http://127.0.0.1:38080`；如果 server 在其他地址，构建或启动前设置
`VITE_BACKEND_URL`。

scripted 路径不需要模型 API key。要从启动时就使用真实 OpenAI 兼容 provider，
先设置 `OPENAI_API_KEY`，可选设置 `OPENAI_BASE_URL`、`OPENAI_ADAPTER` 和
`AGENT_MODEL`。只有需要示例 agents 和 demo tools 时，才设置
`AWAKEN_SEED_PROFILE=demo`。

## 截图

这些截图是使用 sample API data 生成的静态文档图。实际运行中的控制台会从你的后端
API 读取数据；如果某个子系统没有接入，对应界面会显示 disabled / unavailable 提示。

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/01-dashboard.png">
      <img src="/awaken/assets/admin-console/01-dashboard.png" alt="管理控制台 Dashboard，展示 live workload、agent activity、最近审计事件、provider/MCP health 和当前 scope 元数据。" loading="lazy" />
    </a>
    <figcaption>Dashboard：实时负载、健康状态、审计事件和只读 scope。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/02-agent-editor.png">
      <img src="/awaken/assets/admin-console/02-agent-editor.png" alt="Agent 编辑器，包含模型选择、系统提示、tools、plugins、delegates、history、保存控制和 preview chat。" loading="lazy" />
    </a>
    <figcaption>Agent editor：prompt、tools、plugins、delegates、history 和草稿预览。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/03-agents-list.png">
      <img src="/awaken/assets/admin-console/03-agents-list.png" alt="Agents 列表，包含筛选器、model/plugin 元数据和 runtime inference 统计。" loading="lazy" />
    </a>
    <figcaption>Agents list：筛选、model/plugin 元数据和 runtime stats。</figcaption>
  </figure>
</div>

## 首次配置

1. **连接后端。** topbar 提示时填入 admin bearer token。`/v1/capabilities` 可达后，
   状态 pill 会变成绿色。
2. **配置 Provider。** Provider 保存 endpoint、adapter、credentials、timeout 和
   provider-specific options。正式使用前先点 **Test**。
3. **配置 Model。** Model 给 Agent 一个稳定 `model_id`，并描述 upstream model、
   modalities、context limits、pricing 和 capabilities。需要多模型加权路由、
   sticky 选择或 fallback 时，通过 config API 配置 model pool。
4. **解锁 Admin Assistant。** 配置第一个 provider-backed model 后，内置 Admin
   Assistant 才可用。它的工具由 server 锁定，不会出现在普通 tool registry。它可以
   读取平台能力、创建并发布 AgentSpec、只生成 draft，以及校验 draft。
5. **MCP-only 是 full configuration mode。** 你可以配置 MCP servers，并把 MCP 工具
   分给 Agent；但 chat 和 preview 仍然需要 model executor。

Provider credentials 和 MCP credentials 是两条边界。Provider 服务模型执行；MCP
server credentials 属于对应 transport（stdio 的 `env`，HTTP 的 URL/config）。Agent
对 MCP 的访问由工具选择和可选 permission rules 控制。

## 创建和调优 Agent

打开 **Agents**，点击 **New Agent**。

1. 在 **Basics** 设置 agent id、model、max rounds、reasoning effort 和 system prompt。
2. 在 **Tools** 选择全部工具或自定义工具集合。Built-in、plugin、MCP 工具在同一页展示；
   支持覆盖描述的工具会显示最终描述。
3. 在 **Skills** 选择 Agent 可见的 skills。Skill 通过 catalog 注入，并用 `skill` 工具激活；
   当前没有单独的 `SkillSearch` 工具。
4. 在 **Delegates** 选择显式 sub-agents。Delegates 在解析时变成 delegate tools；当前没有单独的
   `AgentSearch` 工具。
5. 在 **Plugins** 启用 permission、reminder、generative UI、deferred tools 等策略。
   已保存的 plugin section 只有在对应 plugin 启用后才会生效。
6. 用 **Validate** 校验草稿，不保存。
7. 用右侧 preview chat 测试未保存草稿。
8. **Save** 发布通过校验的配置，让新 run 使用下一版 registry snapshot。

这套调优面尽量大，但仍保持安全边界：prompt、工具描述、system reminder、
ToolSearch/deferred-tool 策略、Skill 元数据、delegates、plugin section、model
选择和 provider 配置都可以在线编辑。新的可执行工具、provider factory、store 和 plugin
仍然属于 Rust 代码。

## 把保存后的 Agent 接到前端

Agent 保存后，编辑器右侧会显示 **Frontend integration** 卡片，指向 agent-scoped
protocol routes：

```text
POST /v1/ai-sdk/agents/<agent_id>/runs
POST /v1/ag-ui/agents/<agent_id>/runs
```

AI SDK v6 示例：

```ts
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";

const { messages, sendMessage } = useChat({
  transport: new DefaultChatTransport({
    api: "http://127.0.0.1:38080/v1/ai-sdk/agents/support-agent/runs",
  }),
});
```

当客户端每次请求自己选择 agent 时，用通用 `/v1/ai-sdk/chat` 并传 `agent_id`。
当某个 UI 固定绑定一个已保存 Agent 时，用 agent-scoped route。更多说明见
[AI SDK 前端集成](/awaken/zh-cn/how-to/integrate-ai-sdk-frontend/)、
[AI SDK v6 参考](/awaken/zh-cn/reference/protocols/ai-sdk-v6/) 和
[CopilotKit / AG-UI 集成](/awaken/zh-cn/how-to/integrate-copilotkit-ag-ui/)。
路由级细节见 [HTTP API 参考](/awaken/zh-cn/reference/http-api/)。

## 运维、Trace 与 Eval

- **Dashboard** 展示实时负载、provider/MCP health、最近审计事件、可选 runtime stats 和只读 `scope_id`。
- 已保存 Agent 的 **Recent runs** 会在 trace routes 启用时打开持久 trace。
- **Datasets** 可以从 trace 捕获 eval fixture。
- **Eval Runs** 对配置好的 agents/models 运行 dataset。
- **Eval Reports** 在浏览器中查看 NDJSON report 和 baseline diff。

Trace 和 eval payload 可能包含 prompt、tool arguments 和模型回复。请保护 admin bearer
token 和相关路由访问范围。

## 版本历史与 Pinning

每次配置保存都会记录 metadata；启用 audit log 后，也会出现在 Audit Log 中。Agent
History 可以查看 diff，并把历史 snapshot 恢复到 editing store。

Restore 是审查步骤：恢复后，如果要让该 payload 对新 run 生效，需要再次 Save/Publish。
当 server 挂接 versioned registry store 后，已发布 runtime registry snapshot 是不可变的；
durable run 会携带 `resolution_id`，让 resume/replay 重新选择同一个 graph。

## Scope

`scope_id` 在控制台里只读展示。浏览器不直接选择 scope；server 会通过可信的
`HttpScopeProvider` 为每个请求解析 scope。托管版或多 workspace 产品应在
auth/provider 层切换 tenant/workspace，然后在控制台显示解析后的值。

## 相关

- [快速上手](/awaken/zh-cn/get-started/) - 启动本地 server 和控制台
- [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/) - 完整调优面
- [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/) - 更长的操作 walkthrough
- [HTTP API](/awaken/zh-cn/reference/http-api/) - 请求与响应参考
