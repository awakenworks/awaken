---
title: "MCP HTTP Protocol"
description: "The MCP HTTP adapter exposes an Awaken runtime as a Streamable HTTP MCP server."
---

The MCP HTTP adapter exposes an Awaken runtime as a Streamable HTTP MCP
server. It is a server-mode surface: MCP client requests are translated into
the same runtime run model used by the other protocol adapters.

**Feature gate**: `server`

## Endpoints

| Route | Method | Description |
|-------|--------|-------------|
| `/v1/mcp` | POST | JSON-RPC 2.0 request, notification, or response. `initialize` creates a session. `tools/call` streams the tool result over SSE. |
| `/v1/mcp` | DELETE | Stop and remove an MCP HTTP session. Requires `MCP-Session-Id`. |
| `/v1/mcp` | GET | Reserved by the route set; currently returns `405 Method Not Allowed`. |

## Session Rules

- `initialize` must not include `MCP-Session-Id`. On success, the response
  includes `MCP-Session-Id`.
- Later POST and DELETE requests use that `MCP-Session-Id`.
- `MCP-Protocol-Version` is optional, but when present it must match the
  negotiated session protocol version.
- Requests must be single JSON-RPC objects. Batch payloads are not accepted.
- Notifications and responses are accepted with `202 Accepted`.

## Runtime Mapping

The adapter builds an MCP server from the active `AgentRuntime`. When a server
mailbox is present, MCP tool calls enter the durable mailbox path; otherwise
they execute through the runtime directly. Tool calls return runtime events as
MCP tool responses, and terminal run failures are converted into MCP errors.

MCP servers that Awaken consumes as tool providers are configured separately in
`/v1/config/mcp-servers`; this page covers the HTTP endpoint where Awaken acts
as the MCP server.
