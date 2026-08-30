import { describe, expect, it } from "vitest";
import type { UIMessage } from "ai";
import {
  draftPreviewAccessRequest,
  draftPreviewRequest,
  draftPreviewSignature,
  previewToolView,
  selectDraftPreviewAttempt,
  uiMessageText,
} from "./SandboxPane";
import {
  agUiMessageText,
  inspectProtocolSse,
  protocolEventType,
  previewToolLabel,
  retainProtocolEvents,
} from "./ProtocolDebugger";

describe("AI SDK Live Preview helpers", () => {
  it("projects only AI SDK text parts into transcript text", () => {
    const message = {
      id: "m1",
      role: "assistant",
      parts: [
        { type: "text", text: "hello" },
        { type: "step-start" },
        { type: "text", text: " world" },
      ],
    } as UIMessage;
    expect(uiMessageText(message)).toBe("hello world");
  });

  it("gives preview tool disclosures an explicit localized accessible name", () => {
    expect(previewToolLabel("write", false)).toBe("Tool write");
    expect(previewToolLabel("write", true)).toBe("工具 write");
  });

  it("projects AI SDK tool states into the shared conversation language", () => {
    expect(previewToolView({
      type: "tool-write",
      toolCallId: "tool-1",
      state: "output-available",
      input: { file_path: "report.md" },
      output: { ok: true },
    } as never, false)).toEqual({
      name: "write",
      tone: "done",
      statusLabel: "done",
      input: '{\n  "file_path": "report.md"\n}',
      output: '{\n  "ok": true\n}',
    });
    expect(previewToolView({
      type: "tool-write",
      toolCallId: "tool-2",
      state: "output-error",
      input: { file_path: "report.md" },
      errorText: "permission denied",
    } as never, true)).toMatchObject({
      name: "write",
      tone: "error",
      statusLabel: "失败",
      output: "permission denied",
    });
  });
});

describe("cross-protocol Live Preview", () => {
  it("binds both frontend protocols to one short-lived Session token", () => {
    expect(draftPreviewAccessRequest("thread-1", "session-1")).toEqual({
      protocols: ["ai-sdk", "ag-ui"],
      operations: ["thread.run", "thread.messages.read"],
      thread_bindings: [{ external_thread_id: "thread-1", managed_session_id: "session-1" }],
      expires_in_seconds: 900,
    });
  });

  it("labels protocol lifecycle frames without inventing a type", () => {
    expect(protocolEventType({ type: "RUN_STARTED", runId: "run-1" })).toBe("RUN_STARTED");
    expect(protocolEventType({ delta: "hello" })).toBe("data");
  });

  it("retains lifecycle anchors when streaming deltas exceed the inspector limit", () => {
    const events = [
      { id: 1, protocol: "ag-ui", direction: "request", type: "RUN_AGENT", payload: {} },
      { id: 2, protocol: "ag-ui", direction: "response", type: "RUN_STARTED", payload: {} },
      ...Array.from({ length: 140 }, (_, index) => ({
        id: index + 3,
        protocol: "ag-ui" as const,
        direction: "response" as const,
        type: "TEXT_MESSAGE_CONTENT",
        payload: { delta: String(index) },
      })),
      { id: 143, protocol: "ag-ui", direction: "response", type: "RUN_FINISHED", payload: {} },
    ] as const;

    const retained = retainProtocolEvents([...events], 60);
    expect(retained).toHaveLength(60);
    expect(retained.map((event) => event.type)).toEqual(expect.arrayContaining([
      "RUN_AGENT",
      "RUN_STARTED",
      "RUN_FINISHED",
    ]));
    expect(retained.at(-2)?.payload).toEqual({ delta: "139" });
    expect(retained.at(-1)?.type).toBe("RUN_FINISHED");
  });

  it("reassembles SSE records across transport chunk boundaries", async () => {
    const encoder = new TextEncoder();
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(encoder.encode('data: {"type":"RUN_STA'));
        controller.enqueue(encoder.encode('RTED"}\n\ndata: {"type":"RUN_FINISHED"}\n\n'));
        controller.close();
      },
    });
    const frames: unknown[] = [];
    await inspectProtocolSse(stream, (frame) => frames.push(frame));
    expect(frames).toEqual([{ type: "RUN_STARTED" }, { type: "RUN_FINISHED" }]);
  });

  it("projects only visible AG-UI text content", () => {
    expect(agUiMessageText({ id: "u1", role: "user", content: [
      { type: "text", text: "hello" },
      { type: "image", source: { type: "url", value: "https://example.test/x.png" } },
    ] })).toBe("hello");
  });
});

describe("draft preview cause/effect graph", () => {
  const draft = {
    id: "",
    model: { mode: "auto" as const },
    system: "Use the project memory.",
    metadata: {},
    tools: [],
    mcp_servers: [],
    skills: [],
    max_steps: 8,
    plugins: [],
    plugin_config: {},
    context_policy: { kind: "keep_all" as const },
  };
  const resources = [{
    binding_id: "memory",
    target: { kind: "memory_store" as const, id: "memstore-1" },
    mount_path: "/mnt/memory",
    access: "read_write" as const,
    instructions: "Prefer decisions from this project.",
  }];

  it("builds a complete ephemeral snapshot without requiring a saved Agent id", () => {
    const request = draftPreviewRequest("preview-1", draft, resources);
    expect(request.config.id).toBe("");
    expect(request.resources).toEqual({ agent_id: "preview-1", inputs: resources, revision: 1 });
  });

  it("marks config and resource edits as a different preview snapshot", () => {
    const baseline = draftPreviewSignature(draft, resources);
    expect(draftPreviewSignature({ ...draft, system: "Changed" }, resources)).not.toBe(baseline);
    expect(draftPreviewSignature(draft, [{ ...resources[0], instructions: "Changed" }])).not.toBe(baseline);
  });

  it("retains an unknown create attempt and rotates a changed preview", () => {
    // Cause/effect graph: C1 previous create result is unknown; C2 the draft
    // signature is exact/changed. Effects: E1 exact retry retains preview,
    // external Thread, and idempotency payload coordinates; E2 changed intent
    // rotates all coordinates. Decision table: P1=C1+exact=>E1;
    // P2=C1+changed=>E2. The IdempotencyScope test owns key closure on success.
    let next = 0;
    const randomId = () => `id-${++next}`;
    const first = selectDraftPreviewAttempt(undefined, "draft-a", randomId);
    const retry = selectDraftPreviewAttempt(first, "draft-a", randomId);
    const changed = selectDraftPreviewAttempt(first, "draft-b", randomId);
    expect(retry).toBe(first);
    expect(changed).toEqual({
      signature: "draft-b",
      externalThreadId: "id-3",
      nextPreviewId: "preview-id-4",
    });
  });
});
