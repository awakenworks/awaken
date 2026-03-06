# HTTP API

This page lists the complete HTTP surface exposed by `tirea-agentos-server`.

## Conventions

- Error response shape:

```json
{ "error": "<message>" }
```

- Stream responses use `text/event-stream`.
- Query `limit` is clamped to `1..=200`.
- Canonical Run API and A2A task APIs require run-store initialization (`init_run_service(...)`).

## Endpoint Map

Health:

- `GET /health`

Threads:

- `GET /v1/threads`
- `GET /v1/threads/:id`
- `GET /v1/threads/:id/messages`

Canonical runs:

- `GET /v1/runs`
- `GET /v1/runs/:id`
- `POST /v1/runs`
- `POST /v1/runs/:id/inputs`
- `POST /v1/runs/:id/cancel`

AG-UI:

- `POST /v1/ag-ui/agents/:agent_id/runs`
- `GET /v1/ag-ui/threads/:id/messages`

AI SDK v6:

- `POST /v1/ai-sdk/agents/:agent_id/runs`
- `GET /v1/ai-sdk/agents/:agent_id/runs/:chat_id/stream`
- `GET /v1/ai-sdk/threads/:id/messages`

A2A:

- `GET /.well-known/agent-card.json`
- `GET /v1/a2a/agents`
- `GET /v1/a2a/agents/:agent_id/agent-card`
- `POST /v1/a2a/agents/:agent_id/message:send`
- `GET /v1/a2a/agents/:agent_id/tasks/:task_id`
- `POST /v1/a2a/agents/:agent_id/tasks/:task_id:cancel`

## Core Examples

Health:

```bash
curl -i http://127.0.0.1:8080/health
```

List thread projections:

```bash
curl 'http://127.0.0.1:8080/v1/threads?offset=0&limit=50&parent_thread_id=thread-root'
```

Load raw thread messages:

```bash
curl 'http://127.0.0.1:8080/v1/threads/thread-1/messages?after=10&limit=20&order=asc&visibility=all&run_id=run-1'
```

## Stream Run Endpoints

AI SDK v6 stream (`id/messages` payload):

```bash
curl -N \
  -H 'content-type: application/json' \
  -d '{"id":"thread-1","runId":"run-1","messages":[{"id":"u1","role":"user","content":"hello"}]}' \
  http://127.0.0.1:8080/v1/ai-sdk/agents/assistant/runs
```

AI SDK decision forwarding to an active run:

```bash
curl -X POST \
  -H 'content-type: application/json' \
  -d '{"id":"thread-1","runId":"run-1","messages":[{"role":"assistant","parts":[{"type":"tool-approval-response","approvalId":"fc_perm_1","approved":true}]}]}' \
  http://127.0.0.1:8080/v1/ai-sdk/agents/assistant/runs
```

AI SDK regenerate-message:

```bash
curl -X POST \
  -H 'content-type: application/json' \
  -d '{"id":"thread-1","trigger":"regenerate-message","messageId":"m_assistant_1"}' \
  http://127.0.0.1:8080/v1/ai-sdk/agents/assistant/runs
```

AI SDK resume stream for active chat id:

```bash
curl -N http://127.0.0.1:8080/v1/ai-sdk/agents/assistant/runs/thread-1/stream
```

AG-UI stream:

```bash
curl -N \
  -H 'content-type: application/json' \
  -d '{"threadId":"thread-2","runId":"run-2","messages":[{"role":"user","content":"hello"}],"tools":[]}' \
  http://127.0.0.1:8080/v1/ag-ui/agents/assistant/runs
```

## Canonical Run API

Start run:

```bash
curl -N \
  -H 'content-type: application/json' \
  -d '{"agentId":"assistant","threadId":"thread-1","messages":[{"role":"user","content":"hello"}]}' \
  http://127.0.0.1:8080/v1/runs
```

List runs:

```bash
curl 'http://127.0.0.1:8080/v1/runs?thread_id=thread-1&status=completed&origin=ag_ui&limit=20'
```

Forward decisions or continue:

```bash
curl -X POST \
  -H 'content-type: application/json' \
  -d '{"decisions":[{"target_id":"fc_perm_1","decision_id":"d1","action":"resume","result":{"approved":true},"updated_at":1760000000000}]}' \
  http://127.0.0.1:8080/v1/runs/run-1/inputs
```

Cancel:

```bash
curl -X POST http://127.0.0.1:8080/v1/runs/run-1/cancel
```

## A2A

Gateway discovery card:

```bash
curl -i http://127.0.0.1:8080/.well-known/agent-card.json
```

Submit task:

```bash
curl -X POST \
  -H 'content-type: application/json' \
  -d '{"input":"hello from a2a"}' \
  http://127.0.0.1:8080/v1/a2a/agents/assistant/message:send
```

Get task projection:

```bash
curl http://127.0.0.1:8080/v1/a2a/agents/assistant/tasks/<task_id>
```

Cancel task:

```bash
curl -X POST http://127.0.0.1:8080/v1/a2a/agents/assistant/tasks/<task_id>:cancel
```

## Validation Failures

AI SDK v6:

- empty `id` -> `400` (`id cannot be empty`)
- no user input and no decisions and not regenerate -> `400`
- `trigger=regenerate-message` without valid `messageId` -> `400`
- legacy payload (`sessionId/input`) -> `400` (`legacy AI SDK payload shape is no longer supported; use id/messages`)

AG-UI:

- empty `threadId` -> `400`
- empty `runId` -> `400`

Run API:

- empty `agentId` on run creation -> `400`
- `/inputs` with both `messages` and `decisions` empty -> `400`
- `/inputs` with messages but missing `agentId` -> `400`

A2A:

- action not `message:send` -> `400`
- decision-only submit without `taskId` -> `400`
- cancel path without `:cancel` suffix -> `400`

Shared:

- unknown `agent_id` -> `404`
- unknown run/task id -> `404`
