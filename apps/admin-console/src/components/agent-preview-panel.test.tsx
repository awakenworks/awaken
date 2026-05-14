// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { UIMessage } from "@ai-sdk/react";
import {
  AgentPreviewPanel,
  MessageParts,
  hasRenderableContent,
  normalizePreviewAgent,
} from "./agent-preview-panel";
import type { AgentSpec, RemoteEndpoint } from "../lib/api/types";

const previewHarness = vi.hoisted(() => ({
  messages: [] as UIMessage[],
  status: "ready" as "ready" | "submitted" | "streaming" | "error",
  error: undefined as Error | undefined,
  sendMessage: vi.fn(),
  setMessages: vi.fn(),
  useChat: vi.fn(),
  drawerProps: [] as Array<{ agentId: string; open: boolean; onClose: () => void }>,
}));

vi.mock("@ai-sdk/react", () => ({
  useChat: previewHarness.useChat,
}));

vi.mock("@/components/recent-traces-drawer", () => ({
  RecentTracesDrawer: (props: { agentId: string; open: boolean; onClose: () => void }) => {
    previewHarness.drawerProps.push(props);
    return props.open ? (
      <div data-testid="recent-traces-drawer" data-agent-id={props.agentId}>
        Recent runs drawer
      </div>
    ) : null;
  },
}));

beforeEach(() => {
  previewHarness.messages = [];
  previewHarness.status = "ready";
  previewHarness.error = undefined;
  previewHarness.drawerProps.length = 0;
  previewHarness.sendMessage.mockClear();
  previewHarness.setMessages.mockClear();
  previewHarness.useChat.mockReset();
  previewHarness.useChat.mockImplementation(() => ({
    messages: previewHarness.messages,
    sendMessage: previewHarness.sendMessage,
    setMessages: previewHarness.setMessages,
    status: previewHarness.status,
    error: previewHarness.error,
  }));
});

afterEach(() => {
  cleanup();
});

/** Build a `UIMessage` from an array of parts. Caller decides the shape so
 *  each test case mirrors an AI SDK stream snapshot without relying on a
 *  live backend.  */
function uiMessage(parts: unknown[]): UIMessage {
  return {
    id: "m-1",
    role: "assistant",
    parts: parts as UIMessage["parts"],
  } as UIMessage;
}

function agentDraft(overrides: Partial<AgentSpec> = {}): AgentSpec {
  return {
    id: "saved-agent",
    model_id: "research-default",
    system_prompt: "You are a test agent.",
    plugin_ids: [],
    active_hook_filter: [],
    sections: {},
    delegates: [],
    ...overrides,
  };
}

describe("MessageParts — AI SDK part states (R3 #5)", () => {
  it("renders a non-empty text part", () => {
    render(<MessageParts message={uiMessage([{ type: "text", text: "hello" }])} />);
    expect(screen.getByText("hello")).toBeTruthy();
  });

  it("redacts credential patterns inside assistant text parts", () => {
    const { container } = render(
      <MessageParts
        message={uiMessage([
          {
            type: "text",
            text: "upstream echoed Authorization: Bearer sk-real-secret-value",
          },
        ])}
      />,
    );
    const dom = container.textContent ?? "";
    expect(dom).not.toContain("sk-real-secret-value");
    expect(dom).toContain("Authorization: ***");
  });

  it("skips an empty text part rather than printing a blank bubble", () => {
    render(<MessageParts message={uiMessage([{ type: "text", text: "" }])} />);
    expect(screen.queryByText(/empty turn/)).toBeTruthy();
  });

  it("renders reasoning inside a collapsible details block", () => {
    render(
      <MessageParts
        message={uiMessage([{ type: "reasoning", text: "let me think step by step" }])}
      />,
    );
    expect(screen.getByText("Reasoning")).toBeTruthy();
    expect(screen.getByText("let me think step by step")).toBeTruthy();
  });

  it("redacts credential patterns inside reasoning parts", () => {
    const { container } = render(
      <MessageParts
        message={uiMessage([
          {
            type: "reasoning",
            text: "I should call the API with Bearer real-bearer-token-1234567890",
          },
        ])}
      />,
    );
    const dom = container.textContent ?? "";
    expect(dom).not.toContain("real-bearer-token-1234567890");
    expect(dom).toContain("Bearer ***");
  });

  it("renders a `dynamic-tool` part as a ToolInvocation card with name + input", () => {
    render(
      <MessageParts
        message={uiMessage([
          {
            type: "dynamic-tool",
            toolName: "get_weather",
            toolCallId: "call-abcd1234",
            state: "input-streaming",
            input: { city: "SF" },
          },
        ])}
      />,
    );
    expect(screen.getByText("get_weather")).toBeTruthy();
    expect(screen.getByText(/Calling/)).toBeTruthy();
    expect(screen.getByText(/SF/)).toBeTruthy();
  });

  it("renders a typed `tool-<name>` part identical to dynamic-tool", () => {
    render(
      <MessageParts
        message={uiMessage([
          {
            type: "tool-get_weather",
            state: "input-available",
            input: { city: "NYC" },
          },
        ])}
      />,
    );
    expect(screen.getByText("get_weather")).toBeTruthy();
    expect(screen.getByText(/NYC/)).toBeTruthy();
  });

  it("renders `output-available` with the Done badge and an Output block", () => {
    render(
      <MessageParts
        message={uiMessage([
          {
            type: "tool-get_weather",
            state: "output-available",
            input: { city: "SF" },
            output: { temp_c: 18 },
          },
        ])}
      />,
    );
    expect(screen.getByText(/Done/)).toBeTruthy();
    expect(screen.getByText(/temp_c/)).toBeTruthy();
  });

  it("renders `output-error` with the Error badge and errorText (not output)", () => {
    render(
      <MessageParts
        message={uiMessage([
          {
            type: "tool-get_weather",
            state: "output-error",
            input: { city: "??" },
            output: "should not be visible",
            errorText: "Tool execution failed: timeout",
          },
        ])}
      />,
    );
    // Two Error labels render: the state badge ("Error") and the section
    // heading above the errorText body. Both must be present.
    expect(screen.getAllByText(/Error/).length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText(/Tool execution failed: timeout/)).toBeTruthy();
    expect(screen.queryByText(/should not be visible/)).toBeNull();
  });

  // R10 #5 — Tool input/output payloads can carry API keys / auth
  // headers / cookies / JWTs. Until R10 they were JSON-stringified
  // directly into the preview DOM; the redaction pipeline used by
  // audit/trace/diff did not cover this code path.
  it("redacts secret-bearing keys in tool input and output", () => {
    const { container } = render(
      <MessageParts
        message={uiMessage([
          {
            type: "tool-call_api",
            state: "output-available",
            input: {
              headers: { authorization: "Bearer real-token", cookie: "sid=raw" },
              jwt: "raw-jwt",
            },
            output: { api_key: "raw-key", bearer: "raw-bearer", data: { ok: true } },
          },
        ])}
      />,
    );
    const dom = container.textContent ?? "";
    // None of the raw credential strings may appear in the DOM.
    expect(dom).not.toContain("real-token");
    expect(dom).not.toContain("sid=raw");
    expect(dom).not.toContain("raw-jwt");
    expect(dom).not.toContain("raw-key");
    expect(dom).not.toContain("raw-bearer");
    // The redacted placeholder shows up where the secrets used to be.
    expect(dom).toContain("***");
    // Non-secret data is preserved.
    expect(dom).toContain('"ok": true');
  });

  // R12 #3 — Output paths missed by R10's object-only redaction:
  // primitive string outputs, and `errorText` (rendered to a separate
  // <pre>). Both pass through `redactSecretString` now.
  it("redacts credential patterns inside a primitive string tool output", () => {
    const { container } = render(
      <MessageParts
        message={uiMessage([
          {
            type: "tool-call_api",
            state: "output-available",
            input: {},
            output:
              "called with Authorization: Bearer sk-real-secret-value and api_key=raw-key-12345",
          },
        ])}
      />,
    );
    const dom = container.textContent ?? "";
    expect(dom).not.toContain("sk-real-secret-value");
    expect(dom).not.toContain("raw-key-12345");
    expect(dom).toContain("***");
  });

  it("redacts credential patterns inside tool errorText", () => {
    const { container } = render(
      <MessageParts
        message={uiMessage([
          {
            type: "tool-call_api",
            state: "output-error",
            input: {},
            errorText:
              "request failed with Bearer real-bearer-token-1234567890 — body had api_key=raw-key",
          },
        ])}
      />,
    );
    const dom = container.textContent ?? "";
    expect(dom).not.toContain("real-bearer-token-1234567890");
    expect(dom).not.toContain("raw-key");
    expect(dom).toContain("Bearer ***");
  });

  it("renders `output-denied` with the Denied badge", () => {
    render(
      <MessageParts
        message={uiMessage([
          {
            type: "tool-Bash",
            state: "output-denied",
            input: { cmd: "rm -rf /" },
          },
        ])}
      />,
    );
    expect(screen.getByText(/Denied/)).toBeTruthy();
  });

  it("renders `approval-requested` with the awaiting-approval badge", () => {
    render(
      <MessageParts
        message={uiMessage([
          {
            type: "tool-Bash",
            state: "approval-requested",
            input: { cmd: "ls" },
          },
        ])}
      />,
    );
    expect(screen.getByText(/Awaiting approval/)).toBeTruthy();
  });

  it("skips `step-start` parts silently (no bubble content)", () => {
    render(<MessageParts message={uiMessage([{ type: "step-start" }])} />);
    expect(screen.getByText(/empty turn/)).toBeTruthy();
  });

  // R3 #6: unknown parts must not silently render an empty bubble — they go
  // into the "unrecognized part" debug fallback so the user sees them.
  it("collects unknown parts into a single debug fallback", () => {
    render(
      <MessageParts
        message={uiMessage([
          { type: "source", url: "https://example.com" },
          { type: "metadata", payload: 42 },
        ])}
      />,
    );
    const fallback = screen.getByTestId("message-unknown-parts");
    expect(fallback).toBeTruthy();
    expect(fallback.textContent ?? "").toContain("2 unrecognized parts");
    expect(fallback.textContent ?? "").toContain("source");
    expect(fallback.textContent ?? "").toContain("metadata");
  });

  it("mixes text + tool + unknown without losing any of them", () => {
    render(
      <MessageParts
        message={uiMessage([
          { type: "step-start" },
          { type: "text", text: "Let me check the weather." },
          {
            type: "tool-get_weather",
            state: "output-available",
            input: { city: "SF" },
            output: { temp_c: 18 },
          },
          { type: "metadata", payload: 1 },
        ])}
      />,
    );
    expect(screen.getByText("Let me check the weather.")).toBeTruthy();
    expect(screen.getByText("get_weather")).toBeTruthy();
    expect(screen.getByTestId("message-unknown-parts")).toBeTruthy();
  });
});

describe("AgentPreviewPanel — redaction and trace gating", () => {
  it("redacts transport/server error messages before rendering", () => {
    previewHarness.error = new Error(
      "preview failed with Authorization: Bearer sk-real-secret-value",
    );
    const { container } = render(<AgentPreviewPanel draft={agentDraft()} />);
    const dom = container.textContent ?? "";
    expect(dom).not.toContain("sk-real-secret-value");
    expect(dom).toContain("Authorization: ***");
  });

  it("does not show Recent runs for a new unsaved draft with a fallback preview id", () => {
    const { container } = render(<AgentPreviewPanel draft={agentDraft({ id: "   " })} />);
    expect(container.textContent ?? "").toContain("draft-preview");
    expect(screen.queryByTestId("open-recent-traces")).toBeNull();
    const lastDrawerProps = previewHarness.drawerProps[previewHarness.drawerProps.length - 1];
    expect(lastDrawerProps.agentId).toBe("");
  });

  it("does not show Recent runs for a new unsaved draft after the user enters an id", () => {
    render(<AgentPreviewPanel draft={agentDraft({ id: "my-new-agent" })} />);
    expect(screen.queryByTestId("open-recent-traces")).toBeNull();
    const lastDrawerProps = previewHarness.drawerProps[previewHarness.drawerProps.length - 1];
    expect(lastDrawerProps.agentId).toBe("");
  });

  it("opens Recent runs with the real saved agent id, not the preview fallback id", () => {
    render(
      <AgentPreviewPanel
        draft={agentDraft({ id: " edited-draft-id " })}
        traceAgentId=" saved-agent "
      />,
    );
    fireEvent.click(screen.getByTestId("open-recent-traces"));
    const drawer = screen.getByTestId("recent-traces-drawer");
    expect(drawer.getAttribute("data-agent-id")).toBe("saved-agent");
  });
});

describe("hasRenderableContent (R3 #6)", () => {
  it("returns false for a message containing only step-start", () => {
    expect(hasRenderableContent(uiMessage([{ type: "step-start" }]))).toBe(false);
  });

  it("returns false for an empty-text message", () => {
    expect(hasRenderableContent(uiMessage([{ type: "text", text: "" }]))).toBe(false);
  });

  // R8 #4 — unknown-only message: MessageParts renders the unrecognized-
  // parts collapsible fallback for these, so the bubble is informative
  // (the user can expand and see what types arrived). Filtering them
  // out at this gate would hide the diagnostic entirely. (PR #189
  // description: "Unknown SDK parts collapse into an unrecognized part
  // debug fallback".)
  it("returns true for unknown parts only — they render the debug fallback", () => {
    expect(
      hasRenderableContent(
        uiMessage([
          { type: "source", url: "https://example.com" },
          { type: "metadata", payload: 42 },
        ]),
      ),
    ).toBe(true);
  });

  it("returns false when step-start is the only typed part — nothing to render", () => {
    expect(hasRenderableContent(uiMessage([{ type: "step-start" }, { type: "step-start" }]))).toBe(
      false,
    );
  });

  it("returns true for a tool part even with no text", () => {
    expect(
      hasRenderableContent(uiMessage([{ type: "tool-Bash", state: "input-streaming", input: {} }])),
    ).toBe(true);
  });

  it("returns true for a non-empty reasoning part", () => {
    expect(hasRenderableContent(uiMessage([{ type: "reasoning", text: "thinking…" }]))).toBe(true);
  });

  it("returns true for a real text part", () => {
    expect(hasRenderableContent(uiMessage([{ type: "text", text: "hi" }]))).toBe(true);
  });
});

// R14 — `/v1/ai-sdk/agent-previews/runs` rejects payloads carrying
// `endpoint` or `registry` (the server forces local-only resolution to
// stop a crafted draft from routing the run to an arbitrary backend or
// skipping registry-membership checks). Builtin / customized records the
// editor loads naturally carry `registry`, and any agent backed by a
// remote backend carries `endpoint`. Without this strip, every preview
// of such an agent fails with 400 — a hard regression for the most
// common use case.
describe("normalizePreviewAgent — strip endpoint/registry for preview (R14)", () => {
  function previewSpec(overrides: Partial<AgentSpec> = {}): AgentSpec {
    return {
      id: "draft",
      model_id: "research-default",
      system_prompt: "You are a test agent.",
      ...overrides,
    };
  }

  const REMOTE_ENDPOINT: RemoteEndpoint = {
    backend: "a2a",
    base_url: "https://remote.example.com",
    target: "remote-agent",
    timeout_ms: 60_000,
  };

  it("strips endpoint from the payload", () => {
    const draft = previewSpec({ endpoint: REMOTE_ENDPOINT });
    const out = normalizePreviewAgent(draft);
    expect(out.endpoint).toBeUndefined();
    expect("endpoint" in out).toBe(false);
  });

  it("strips registry from the payload", () => {
    const draft = previewSpec({ registry: "cloud" });
    const out = normalizePreviewAgent(draft);
    expect(out.registry).toBeUndefined();
    expect("registry" in out).toBe(false);
  });

  it("strips both endpoint and registry simultaneously", () => {
    const draft = previewSpec({ endpoint: REMOTE_ENDPOINT, registry: "cloud" });
    const out = normalizePreviewAgent(draft);
    expect("endpoint" in out).toBe(false);
    expect("registry" in out).toBe(false);
  });

  it("preserves every other patchable field", () => {
    const draft = previewSpec({
      endpoint: REMOTE_ENDPOINT,
      registry: "cloud",
      plugin_ids: ["permission"],
      active_hook_filter: ["permission"],
      sections: { permission: { default_behavior: "ask" } },
      delegates: ["other"],
      allowed_tools: ["Bash"],
      excluded_tools: ["Read"],
      max_rounds: 16,
      reasoning_effort: "high",
    });
    const out = normalizePreviewAgent(draft);
    expect(out.plugin_ids).toEqual(["permission"]);
    expect(out.active_hook_filter).toEqual(["permission"]);
    expect(out.sections).toEqual({ permission: { default_behavior: "ask" } });
    expect(out.delegates).toEqual(["other"]);
    expect(out.allowed_tools).toEqual(["Bash"]);
    expect(out.excluded_tools).toEqual(["Read"]);
    expect(out.max_rounds).toBe(16);
    expect(out.reasoning_effort).toBe("high");
  });

  it("defaults a blank id to 'draft-preview'", () => {
    const out = normalizePreviewAgent(previewSpec({ id: "   " }));
    expect(out.id).toBe("draft-preview");
  });

  it("does not mutate the source draft", () => {
    const draft = previewSpec({ endpoint: REMOTE_ENDPOINT, registry: "cloud" });
    const snapshot = JSON.parse(JSON.stringify(draft));
    normalizePreviewAgent(draft);
    expect(draft).toEqual(snapshot);
  });
});
