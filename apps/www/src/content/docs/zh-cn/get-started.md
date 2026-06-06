---
title: "快速上手"
description: "先从进程内 runtime 开始；需要共享协议、托管配置和运维控制时，再加上 server 控制面。"
---

如果你第一次接触 Awaken，先用这条路径快速启动本地 server：tools 和 state 留在
Rust 代码里，行为通过配置变化；server 模式提供共享协议和浏览器管理控制台。

## 启动本地 Server

这个 server 不需要模型 API key。没有 `OPENAI_API_KEY` 时，starter backend 会使用
deterministic scripted executor，方便你先验证 HTTP routes 和管理控制台。

```sh
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
AWAKEN_ADMIN_API_BEARER_TOKEN=dev-token \
AWAKEN_STORAGE_DIR=./target/awaken-dev \
cargo run -p ai-sdk-starter-agent
```

检查 server 是否可达：

```sh
curl -sS \
  -H 'authorization: Bearer dev-token' \
  http://127.0.0.1:38080/v1/capabilities
```

另开一个终端启动管理控制台：

```sh
pnpm install
pnpm --filter awaken-admin-console dev
```

打开 `http://127.0.0.1:3002`，点击 token pill，填入 `dev-token`。之后可以创建
Provider、创建 Model、创建 Agent、预览 Agent，并在已保存 Agent 页面复制前端对接路由。

如果一开始就要用真实模型：

```sh
export OPENAI_API_KEY=<your-key>
export AGENT_MODEL=gpt-4o-mini
# OpenAI-compatible provider 可选：
export OPENAI_BASE_URL=https://api.openai.com/v1
export OPENAI_ADAPTER=openai
```

然后重新运行同一个 `cargo run -p ai-sdk-starter-agent` 命令。只有需要 sample agents
和 demo tools 时才设置 `AWAKEN_SEED_PROFILE=demo`；默认 `minimal` profile 会让控制台
聚焦你自己创建的资源。

## 你刚启动了什么

- `/v1/ai-sdk/chat` 和 `/v1/ai-sdk/agents/:agent_id/runs`，用于 AI SDK v6 前端。
- `/v1/ag-ui/*`，用于 CopilotKit / AG-UI。
- `/v1/config/*`，用于托管 providers、models、agents、MCP servers、tools 和插件 sections。
- `/v1/admin/assistant/*`，配置第一个真实 model 后启用 locked Admin Assistant。
- `AWAKEN_STORAGE_DIR` 下的本地 file-backed store。

## 推荐顺序

1. 阅读 [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/)，理解浏览器工作流：
   配置模型、创建 Agent、预览草稿，并发布下一版 runtime snapshot。
2. 配好真实模型后,阅读 [用 Admin Assistant 构建 Agent](/awaken/zh-cn/how-to/build-an-agent-with-the-assistant/),用自然语言描述即可生成 Agent。
3. 需要 React 前端时，阅读 [AI SDK 前端集成](/awaken/zh-cn/how-to/integrate-ai-sdk-frontend/)。
4. 阅读 [第一个 Agent](/awaken/zh-cn/tutorials/first-agent/)，理解最小进程内 runtime。
5. 阅读 [第一个 Tool](/awaken/zh-cn/tutorials/first-tool/)，理解 tool schema、执行流程和状态写入。
6. 进入 [构建 Agent](/awaken/zh-cn/how-to/build-an-agent/)，把示例整理成可复用的工程基线。

## 何时离开这条路径

- 需要实现新的 runtime 能力时，进入 [开发 Agent 路径](/awaken/zh-cn/build-agents/)。
- 需要调优或运营已保存的 Agent 时，进入 [调优与运营](/awaken/zh-cn/operate/)。
- 需要 HTTP 或前端集成时，进入 [服务与集成](/awaken/zh-cn/serve-and-integrate/)。
- 需要持久化时，进入 [状态与存储](/awaken/zh-cn/state-and-storage/)。
- 准备上线时，进入 [部署到生产](/awaken/zh-cn/how-to/deploy-to-production/)。
