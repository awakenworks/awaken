// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import type { UIMessage } from "@ai-sdk/react";
import { MessageParts, hasRenderableContent } from "./agent-preview-panel";

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

describe("MessageParts — AI SDK part states (R3 #5)", () => {
  it("renders a non-empty text part", () => {
    render(<MessageParts message={uiMessage([{ type: "text", text: "hello" }])} />);
    expect(screen.getByText("hello")).toBeTruthy();
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

describe("hasRenderableContent (R3 #6)", () => {
  it("returns false for a message containing only step-start", () => {
    expect(hasRenderableContent(uiMessage([{ type: "step-start" }]))).toBe(false);
  });

  it("returns false for an empty-text message", () => {
    expect(hasRenderableContent(uiMessage([{ type: "text", text: "" }]))).toBe(false);
  });

  it("returns false for unknown parts only (no empty bubble for sources/metadata)", () => {
    // This is the regression: previously `hasRenderableContent` returned
    // true for anything non-step-start non-empty-text, so a message
    // containing only metadata/source parts would draw an empty bubble.
    expect(
      hasRenderableContent(
        uiMessage([
          { type: "source", url: "https://example.com" },
          { type: "metadata", payload: 42 },
        ]),
      ),
    ).toBe(false);
  });

  it("returns true for a tool part even with no text", () => {
    expect(
      hasRenderableContent(
        uiMessage([{ type: "tool-Bash", state: "input-streaming", input: {} }]),
      ),
    ).toBe(true);
  });

  it("returns true for a non-empty reasoning part", () => {
    expect(
      hasRenderableContent(uiMessage([{ type: "reasoning", text: "thinking…" }])),
    ).toBe(true);
  });

  it("returns true for a real text part", () => {
    expect(hasRenderableContent(uiMessage([{ type: "text", text: "hi" }]))).toBe(true);
  });
});
