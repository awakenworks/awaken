# AG-UI Protocol

The AG-UI adapter translates Awaken's `AgentEvent` stream into the [AG-UI (CopilotKit) event format](https://docs.ag-ui.com). This enables CopilotKit frontends to drive Awaken agents with no custom adapter code.

## Endpoint

```
POST /v1/ag-ui/run
```

### Request Body

```json
{
  "threadId": "optional-thread-id",
  "agentId": "optional-agent-id",
  "messages": [{ "role": "user", "content": "Hello" }],
  "context": {}
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `messages` | `AgUiMessage[]` | yes | Chat messages with `role` and `content` strings. |
| `threadId` | `string` | no | Existing thread. Omit to create a new thread. |
| `agentId` | `string` | no | Target agent. Uses the default agent when omitted. |
| `context` | `object` | no | CopilotKit context forwarding (reserved). |

### Response

SSE stream (`text/event-stream`). Each frame is a JSON-encoded AG-UI `Event`.

### Auxiliary Routes

| Route | Method | Description |
|-------|--------|-------------|
| `/v1/ag-ui/threads/:id/messages` | GET | Retrieve thread message history. |

## Event Mapping

The `AgUiEncoder` is a stateful transcoder that manages text message and step lifecycles.

| AgentEvent | AG-UI Event(s) |
|------------|----------------|
| `RunStart` | `RUN_STARTED` |
| `TextDelta` | `TEXT_MESSAGE_START` (if not open) + `TEXT_MESSAGE_CONTENT` |
| `ReasoningDelta` | `REASONING_MESSAGE_START` (if not open) + `REASONING_MESSAGE_CONTENT` |
| `ToolCallStart` | Close text/reasoning, `STEP_STARTED`, `TOOL_CALL_START` |
| `ToolCallDelta` | `TOOL_CALL_ARGS` |
| `ToolCallDone` | `TOOL_CALL_END`, `STEP_FINISHED` |
| `StateSnapshot` | `STATE_SNAPSHOT` |
| `StateDelta` | `STATE_DELTA` |
| `RunFinish` (success) | Close text/reasoning, `RUN_FINISHED` |
| `RunFinish` (error) | Close text/reasoning, `RUN_ERROR` |

## AG-UI Event Types

Events use an uppercase `type` discriminant:

- `RUN_STARTED` / `RUN_FINISHED` / `RUN_ERROR` -- run lifecycle
- `TEXT_MESSAGE_START` / `TEXT_MESSAGE_CONTENT` / `TEXT_MESSAGE_END` -- assistant text
- `REASONING_MESSAGE_START` / `REASONING_MESSAGE_CONTENT` / `REASONING_MESSAGE_END` -- reasoning trace
- `STEP_STARTED` / `STEP_FINISHED` -- step boundaries (wrapping tool calls)
- `TOOL_CALL_START` / `TOOL_CALL_ARGS` / `TOOL_CALL_END` -- tool call lifecycle
- `STATE_SNAPSHOT` / `STATE_DELTA` -- shared state synchronization
- `MESSAGES_SNAPSHOT` -- full thread message history

All events carry a `BaseEvent` with optional `timestamp` and `rawEvent` fields.

## Roles

AG-UI messages use lowercase role strings: `system`, `user`, `assistant`, `tool`.

## Text Message Lifecycle

1. First `TextDelta` emits `TEXT_MESSAGE_START` followed by `TEXT_MESSAGE_CONTENT`.
2. Subsequent deltas emit only `TEXT_MESSAGE_CONTENT`.
3. A `ToolCallStart` or `RunFinish` closes the open message with `TEXT_MESSAGE_END`.

Reasoning messages follow the same pattern with `REASONING_MESSAGE_*` events.

## Related

- [Events](../events.md) -- full `AgentEvent` enum
- [Integrate CopilotKit (AG-UI)](../../how-to/integrate-copilotkit-ag-ui.md) -- frontend integration guide
