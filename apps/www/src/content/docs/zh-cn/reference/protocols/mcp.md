---
title: "MCP HTTP 协议"
description: "MCP HTTP 适配器把 Awaken runtime 暴露成 Streamable HTTP MCP server。"
---

MCP HTTP 适配器把 Awaken runtime 暴露成 Streamable HTTP MCP server。这是
server 模式的协议表面：MCP client 请求会被转换成与其它协议适配器相同的
runtime run model。

**Feature gate**：`server`

## Endpoints

| Route | Method | 说明 |
|-------|--------|------|
| `/v1/mcp` | POST | JSON-RPC 2.0 request、notification 或 response。`initialize` 创建 session。`tools/call` 通过 SSE 流式返回工具结果。 |
| `/v1/mcp` | DELETE | 停止并移除 MCP HTTP session。需要 `MCP-Session-Id`。 |
| `/v1/mcp` | GET | 路由已保留；当前返回 `405 Method Not Allowed`。 |

## Session 规则

- `initialize` 不能带 `MCP-Session-Id`。成功后，响应会返回
  `MCP-Session-Id`。
- 后续 POST 和 DELETE 请求使用这个 `MCP-Session-Id`。
- `MCP-Protocol-Version` 可选；如果提供，必须匹配 session 协商出的协议版本。
- 请求体必须是单个 JSON-RPC object，不接受 batch payload。
- Notification 和 response 会以 `202 Accepted` 接收。

## Runtime 映射

适配器从当前 `AgentRuntime` 构造 MCP server。存在 server mailbox 时，MCP
tool call 进入持久 mailbox 路径；否则直接通过 runtime 执行。工具调用会把
runtime 事件转换成 MCP tool response，终止性 run failure 会转换成 MCP error。

Awaken 作为 tool provider 消费外部 MCP server 时，配置入口是
`/v1/config/mcp-servers`；本页描述的是 Awaken 自己作为 MCP server 暴露的 HTTP
endpoint。
