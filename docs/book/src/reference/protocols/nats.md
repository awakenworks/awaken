# NATS Protocol

NATS gateway exposes protocol-encoded run streaming over request/reply subjects.

## Subjects

Default subjects:

- `agentos.ag-ui.runs`
- `agentos.ai-sdk.runs`

## Request Payloads

AG-UI payload:

```json
{
  "agentId": "assistant",
  "request": {
    "threadId": "t1",
    "runId": "r1",
    "messages": [{ "role": "user", "content": "hello" }],
    "tools": []
  },
  "replySubject": "_INBOX.x"
}
```

AI SDK payload (NATS path):

```json
{
  "agentId": "assistant",
  "sessionId": "t1",
  "input": "hello",
  "runId": "r1",
  "replySubject": "_INBOX.x"
}
```

Note: AI SDK NATS currently uses `sessionId/input`, while AI SDK HTTP v6 UI route uses `id/messages`.

## Reply Subject Resolution

Gateway chooses reply target in this order:

1. NATS message reply inbox (`msg.reply`)
2. payload `replySubject`

If both are missing, request is rejected with `missing reply subject`.

## Reply Behavior

- Request starts run and publishes protocol events to reply subject.
- Invalid request or agent resolution failure publishes one protocol error event.
- AG-UI replies use AG-UI event encoding.
- AI SDK replies use AI SDK UI stream event encoding.

## Operational Notes

- NATS is transport only; run lifecycle remains canonical `AgentEvent`-driven.
- Run tracking/projection still uses configured run service/store.

## Related

- [Expose NATS](../../how-to/expose-nats.md)
- [AG-UI Protocol](./ag-ui.md)
- [AI SDK v6 Protocol](./ai-sdk-v6.md)
