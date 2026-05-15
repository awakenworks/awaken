import { expect, test, type APIResponse } from "@playwright/test";
import { a2aSendMessagePayload } from "./a2a-test-utils";
import { aiSdkTextMessages } from "./ai-sdk-test-utils";

function uniqueId(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

function sseJsonEvents(raw: string): any[] {
  return raw
    .split("\n")
    .filter((line) => line.startsWith("data:"))
    .map((line) => {
      try {
        return JSON.parse(line.slice(5).trim());
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

function firstRunId(raw: string): string | null {
  for (const event of sseJsonEvents(raw)) {
    if (typeof event.run_id === "string") {
      return event.run_id;
    }
    if (typeof event.runId === "string") {
      return event.runId;
    }
    if (typeof event.run?.run_id === "string") {
      return event.run.run_id;
    }
  }
  return null;
}

async function expectNoServerError(label: string, response: APIResponse) {
  expect(response.status(), label).toBeLessThan(500);
}

test.describe("HTTP API surface coverage", () => {
  test("covers public, runtime, protocol, and observability routes", async ({
    request,
  }) => {
    const health = await request.get("/health");
    expect(health.ok(), "/health").toBeTruthy();
    const live = await request.get("/health/live");
    expect(live.ok(), "/health/live").toBeTruthy();
    const metrics = await request.get("/metrics");
    expect(metrics.ok(), "/metrics").toBeTruthy();

    const threadRes = await request.post("/v1/threads", {
      data: { title: "API surface thread" },
    });
    expect(threadRes.ok(), "POST /v1/threads").toBeTruthy();
    const thread = await threadRes.json();
    const threadId = thread.id;

    const threadChecks = [
      ["GET /v1/threads", () => request.get("/v1/threads?limit=10&offset=0")],
      ["GET /v1/threads/summaries", () => request.get("/v1/threads/summaries")],
      ["GET /v1/threads/:id", () => request.get(`/v1/threads/${threadId}`)],
      [
        "PATCH /v1/threads/:id",
        () =>
          request.patch(`/v1/threads/${threadId}`, {
            data: { title: "API surface thread updated" },
          }),
      ],
      [
        "PATCH /v1/threads/:id/metadata",
        () =>
          request.patch(`/v1/threads/${threadId}/metadata`, {
            data: { custom: { source: "api-surface" } },
          }),
      ],
      [
        "POST /v1/threads/:id/messages",
        () =>
          request.post(`/v1/threads/${threadId}/messages`, {
            data: {
              messages: [{ role: "user", content: "message route coverage" }],
            },
          }),
      ],
      [
        "GET /v1/threads/:id/messages",
        () => request.get(`/v1/threads/${threadId}/messages`),
      ],
      [
        "POST /v1/threads/:id/mailbox",
        () =>
          request.post(`/v1/threads/${threadId}/mailbox`, {
            data: { kind: "api.surface", payload: { ok: true } },
          }),
      ],
      [
        "GET /v1/threads/:id/mailbox",
        () => request.get(`/v1/threads/${threadId}/mailbox`),
      ],
      [
        "POST /v1/threads/:id/cancel",
        () => request.post(`/v1/threads/${threadId}/cancel`),
      ],
      [
        "POST /v1/threads/:id/interrupt",
        () => request.post(`/v1/threads/${threadId}/interrupt`),
      ],
      [
        "POST /v1/threads/:id/decision",
        () =>
          request.post(`/v1/threads/${threadId}/decision`, {
            data: { toolCallId: "no-active-tool-call", action: "cancel" },
          }),
      ],
    ] as const;

    for (const [label, send] of threadChecks) {
      await expectNoServerError(label, await send());
    }

    const runRes = await request.post("/v1/runs", {
      data: {
        agentId: "limited",
        threadId,
        messages: [{ role: "user", content: "run route coverage" }],
      },
    });
    expect(runRes.ok(), "POST /v1/runs").toBeTruthy();
    const runBody = await runRes.text();
    const runId = firstRunId(runBody);
    expect(runId, "SSE run id").toBeTruthy();

    const runChecks = [
      ["GET /v1/runs", () => request.get("/v1/runs")],
      ["GET /v1/runs?status=done", () => request.get("/v1/runs?status=done")],
      ["GET /v1/runs/:id", () => request.get(`/v1/runs/${runId}`)],
      [
        "POST /v1/runs/:id/inputs",
        () =>
          request.post(`/v1/runs/${runId}/inputs`, {
            data: {
              messages: [{ role: "user", content: "late input coverage" }],
            },
          }),
      ],
      [
        "POST /v1/runs/:id/cancel",
        () => request.post(`/v1/runs/${runId}/cancel`),
      ],
      [
        "POST /v1/runs/:id/decision",
        () =>
          request.post(`/v1/runs/${runId}/decision`, {
            data: { toolCallId: "no-active-tool-call", action: "cancel" },
          }),
      ],
      [
        "GET /v1/threads/:id/runs",
        () => request.get(`/v1/threads/${threadId}/runs`),
      ],
      [
        "GET /v1/threads/:id/runs/active",
        () => request.get(`/v1/threads/${threadId}/runs/active`),
      ],
      [
        "GET /v1/threads/:id/runs/latest",
        () => request.get(`/v1/threads/${threadId}/runs/latest`),
      ],
      ["GET /v1/traces", () => request.get("/v1/traces")],
      ["GET /v1/traces/:run_id", () => request.get(`/v1/traces/${runId}`)],
      [
        "POST /v1/traces/:run_id/pin",
        () => request.post(`/v1/traces/${runId}/pin`),
      ],
    ] as const;

    for (const [label, send] of runChecks) {
      await expectNoServerError(label, await send());
    }

    const aiSdkPayload = {
      agentId: "limited",
      messages: aiSdkTextMessages([
        { role: "user", text: "AI SDK route coverage" },
      ]),
    };
    const protocolChecks = [
      [
        "POST /v1/ai-sdk/chat",
        () => request.post("/v1/ai-sdk/chat", { data: aiSdkPayload }),
      ],
      [
        "POST /v1/ai-sdk/threads/:thread_id/runs",
        () =>
          request.post(`/v1/ai-sdk/threads/${threadId}/runs`, {
            data: aiSdkPayload,
          }),
      ],
      [
        "POST /v1/ai-sdk/agents/:agent_id/runs",
        () =>
          request.post("/v1/ai-sdk/agents/limited/runs", {
            data: { messages: aiSdkPayload.messages },
          }),
      ],
      [
        "POST /v1/ai-sdk/agent-previews/runs",
        () =>
          request.post("/v1/ai-sdk/agent-previews/runs", {
            data: {
              agent: {
                id: uniqueId("preview-agent"),
                model_id: "default",
                system_prompt: "Preview route coverage",
                max_rounds: 1,
              },
              messages: aiSdkPayload.messages,
            },
          }),
      ],
      [
        "GET /v1/ai-sdk/chat/:thread_id/stream",
        () => request.get(`/v1/ai-sdk/chat/${threadId}/stream`),
      ],
      [
        "GET /v1/ai-sdk/threads/:thread_id/stream",
        () => request.get(`/v1/ai-sdk/threads/${threadId}/stream`),
      ],
      [
        "GET /v1/ai-sdk/threads/:thread_id/messages",
        () => request.get(`/v1/ai-sdk/threads/${threadId}/messages`),
      ],
      [
        "POST /v1/ai-sdk/threads/:thread_id/cancel",
        () => request.post(`/v1/ai-sdk/threads/${threadId}/cancel`),
      ],
      [
        "POST /v1/ai-sdk/threads/:thread_id/interrupt",
        () => request.post(`/v1/ai-sdk/threads/${threadId}/interrupt`),
      ],
      [
        "POST /v1/ag-ui/run",
        () =>
          request.post("/v1/ag-ui/run", {
            data: {
              agentId: "limited",
              messages: [{ role: "user", content: "AG-UI coverage" }],
            },
          }),
      ],
      [
        "POST /v1/ag-ui/threads/:thread_id/runs",
        () =>
          request.post(`/v1/ag-ui/threads/${threadId}/runs`, {
            data: {
              agentId: "limited",
              messages: [{ role: "user", content: "AG-UI threaded" }],
            },
          }),
      ],
      [
        "POST /v1/ag-ui/agents/:agent_id/runs",
        () =>
          request.post("/v1/ag-ui/agents/limited/runs", {
            data: {
              messages: [{ role: "user", content: "AG-UI agent scoped" }],
            },
          }),
      ],
      [
        "POST /v1/ag-ui/threads/:thread_id/interrupt",
        () => request.post(`/v1/ag-ui/threads/${threadId}/interrupt`),
      ],
      [
        "GET /v1/ag-ui/threads/:id/messages",
        () => request.get(`/v1/ag-ui/threads/${threadId}/messages`),
      ],
    ] as const;

    for (const [label, send] of protocolChecks) {
      await expectNoServerError(label, await send());
    }

    const card = await request.get("/.well-known/agent-card.json");
    expect(card.ok(), "GET /.well-known/agent-card.json").toBeTruthy();
    const { taskId, data } = a2aSendMessagePayload("A2A route surface");
    const a2aSend = await request.post("/v1/a2a/message:send", { data });
    expect(a2aSend.ok(), "POST /v1/a2a/message:send").toBeTruthy();
    const a2aChecks = [
      [
        "POST /v1/a2a/message:stream",
        () => request.post("/v1/a2a/message:stream", { data }),
      ],
      ["GET /v1/a2a/tasks/:id", () => request.get(`/v1/a2a/tasks/${taskId}`)],
      [
        "POST /v1/a2a/tasks/:id/pushNotificationConfigs",
        () =>
          request.post(`/v1/a2a/tasks/${taskId}/pushNotificationConfigs`, {
            data: {
              url: "https://example.com/coverage-webhook",
              token: "coverage-token",
            },
          }),
      ],
      [
        "GET /v1/a2a/tasks/:id/pushNotificationConfigs",
        () => request.get(`/v1/a2a/tasks/${taskId}/pushNotificationConfigs`),
      ],
      [
        "POST /v1/a2a/:agentId/message:send",
        () =>
          request.post("/v1/a2a/limited/message:send", {
            data: a2aSendMessagePayload("tenant A2A coverage").data,
          }),
      ],
    ] as const;
    for (const [label, send] of a2aChecks) {
      await expectNoServerError(label, await send());
    }

    const deleteThread = await request.delete(`/v1/threads/${threadId}`);
    await expectNoServerError("DELETE /v1/threads/:id", deleteThread);
  });
});
