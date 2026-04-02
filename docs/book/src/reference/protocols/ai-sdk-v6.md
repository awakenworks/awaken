# AI SDK v6 Protocol

The AI SDK v6 adapter translates Awaken's internal `AgentEvent` stream into the [Vercel AI SDK v6 UI Message Stream](https://sdk.vercel.ai/docs/ai-sdk-ui/stream-protocol) format. This allows any AI SDK-compatible frontend (`useChat`, `useAssistant`) to consume agent output without custom parsing.

## Endpoint

```text
POST /v1/ai-sdk/chat
```

### Request Body

```json
{
  "messages": [{ "role": "user", "content": "Hello" }],
  "threadId": "optional-thread-id",
  "agentId": "optional-agent-id"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `messages` | `AiSdkMessage[]` | yes | Chat messages. Content may be a string or an array of content parts. |
| `threadId` | `string` | no | Existing thread to continue. Omit to create a new thread. |
| `agentId` | `string` | no | Target agent. Uses the default agent when omitted. |

### Response

SSE stream (`text/event-stream`). Each line is a JSON-encoded `UIStreamEvent`.

### Auxiliary Routes

| Route | Method | Description |
|-------|--------|-------------|
| `/v1/ai-sdk/streams/:run_id` | GET | Reconnect to an active run's SSE stream. |
| `/v1/ai-sdk/runs/:run_id/stream` | GET | Alias for stream reconnect. |
| `/v1/ai-sdk/threads/:id/messages` | GET | Retrieve thread message history. |

## Event Mapping

The `AiSdkEncoder` is a stateful transcoder that converts `AgentEvent` variants into `UIStreamEvent` variants. It tracks open text blocks and reasoning blocks across tool-call boundaries.

| AgentEvent | UIStreamEvent(s) |
|------------|------------------|
| `RunStart` | `MessageStart` + `Data("run-info", ...)` |
| `TextDelta` | `TextStart` (if block not open) + `TextDelta` |
| `ReasoningDelta` | `ReasoningStart` (if block not open) + `ReasoningDelta` |
| `ReasoningEncryptedValue` | `ReasoningStart` (if not open) + `ReasoningDelta` |
| `ToolCallStart` | Close open text/reasoning blocks, then `ToolCallStart` |
| `ToolCallDelta` | `ToolCallDelta` |
| `ToolCallDone` | `ToolCallEnd` |
| `StepStart` | (no direct mapping) |
| `StepEnd` | (no direct mapping) |
| `InferenceComplete` | `Data("inference-complete", ...)` |
| `MessagesSnapshot` | `Data("messages-snapshot", ...)` |
| `StateSnapshot` | `Data("state-snapshot", ...)` |
| `StateDelta` | `Data("state-delta", ...)` |
| `ActivitySnapshot` | `Data("activity-snapshot", ...)` |
| `ActivityDelta` | `Data("activity-delta", ...)` |
| `RunFinish` | Close open blocks, `Data("finish", ...)`, `Finish` |

## UIStreamEvent Types

The wire format uses the `type` field as a discriminant, serialized in kebab-case:

- `start` -- message lifecycle start, carries optional `messageId` and `messageMetadata`
- `text-start`, `text-delta`, `text-end` -- text block lifecycle with content ID
- `reasoning-start`, `reasoning-delta`, `reasoning-end` -- reasoning block lifecycle
- `tool-call-start`, `tool-call-delta`, `tool-call-end` -- tool call lifecycle
- `data` -- arbitrary named data events (state snapshots, activity, inference metadata)
- `finish` -- terminal event with finish reason and usage summary

## Text Block Lifecycle

The encoder automatically manages text block open/close boundaries:

1. First `TextDelta` opens a text block (`TextStart`).
2. Subsequent deltas append to the open block.
3. When a `ToolCallStart` arrives, the encoder closes any open text or reasoning block before emitting tool events.
4. After tool execution completes, new text deltas open a fresh block with an incremented ID.

This ensures the frontend receives well-formed block boundaries even though the runtime emits flat delta events.

## Related

- [Events](../events.md) -- full `AgentEvent` enum
- [HTTP API](../http-api.md) -- server configuration
- [Integrate AI SDK Frontend](../../how-to/integrate-ai-sdk-frontend.md) -- frontend integration guide
