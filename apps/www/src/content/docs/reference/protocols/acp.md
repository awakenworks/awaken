---
title: "ACP Protocol"
description: "The Agent Client Protocol (ACP) adapter exposes an Awaken runtime over stdio using the official ACP Rust SDK."
---

The Agent Client Protocol (ACP) adapter exposes an Awaken runtime over stdio
using the official `agent-client-protocol` Rust SDK. Unlike the HTTP protocols,
ACP is a process/stdio integration: a host launches the Awaken-backed process,
then exchanges ACP JSON-RPC messages over stdin/stdout.

**Feature gate**: `server`

## Runtime entry points

| API | Purpose |
|---|---|
| `awaken_server::protocols::acp::stdio::serve_stdio(runtime)` | Serve ACP on process stdin/stdout. |
| `awaken_server::protocols::acp::stdio::serve_stdio_io(runtime, input, output)` | Serve ACP over caller-provided async I/O; used by tests and embedders. |
| `awaken_server::protocols::acp::encoder::AcpEncoder` | Transcode `AgentEvent` values into ACP session updates. |

## Session behavior

- `initialize` returns the requested protocol version, `awaken-acp` agent info,
  and prompt capabilities for text plus image/audio/embedded-context blocks.
- `newSession` requires an absolute `cwd`. `mcpServers` in the request are
  rejected; register MCP servers through Awaken config instead.
- The adapter selects the `default` agent when present. If no `default` agent
  exists, the runtime must have exactly one registered agent.
- Each ACP session maps to a fresh Awaken thread id. `prompt` appends the user
  content to that thread and runs the selected agent through `AgentRuntime`.
- Tool permission requests are bridged to the ACP client and converted back into
  Awaken HITL resume decisions.

## Relationship to HTTP adapters

ACP consumes the same runtime events as AI SDK v6, AG-UI, A2A, and MCP. It does
not add a separate agent execution path; it only changes the client transport
and wire format.
