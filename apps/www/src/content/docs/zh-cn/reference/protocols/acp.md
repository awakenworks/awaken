---
title: "ACP 协议"
description: "Agent Client Protocol (ACP) 适配器通过官方 ACP Rust SDK，把 Awaken runtime 暴露为 stdio 进程集成。"
---

Agent Client Protocol (ACP) 适配器通过官方 `agent-client-protocol` Rust SDK，
把 Awaken runtime 暴露为 stdio 进程集成。和 HTTP 协议不同，ACP 由宿主启动
Awaken-backed 进程，然后通过 stdin/stdout 交换 ACP JSON-RPC 消息。

**Feature gate**：`server`

## Runtime 入口

| API | 用途 |
|---|---|
| `awaken_server::protocols::acp::stdio::serve_stdio(runtime)` | 在进程 stdin/stdout 上提供 ACP。 |
| `awaken_server::protocols::acp::stdio::serve_stdio_io(runtime, input, output)` | 使用调用方提供的 async I/O 提供 ACP；测试和嵌入场景使用。 |
| `awaken_server::protocols::acp::encoder::AcpEncoder` | 将 `AgentEvent` 转码为 ACP session update。 |

## Session 行为

- `initialize` 返回请求的协议版本、`awaken-acp` agent info，以及 text、image、audio、embedded-context prompt capability。
- `newSession` 要求 `cwd` 是绝对路径。请求里的 `mcpServers` 会被拒绝；MCP server 应通过 Awaken 配置注册。
- 如果存在 `default` agent，适配器会选择它；否则 runtime 必须只注册一个 agent。
- 每个 ACP session 映射到一个新的 Awaken thread id。`prompt` 会把用户内容追加到该 thread，并通过 `AgentRuntime` 执行选中的 agent。
- 工具权限请求会转发给 ACP client，并转换回 Awaken HITL resume decision。

## 与 HTTP 适配器的关系

ACP 消费的仍是 AI SDK v6、AG-UI、A2A、MCP 共用的 runtime event；它不引入新的 agent 执行路径，只改变 client transport 和 wire format。
