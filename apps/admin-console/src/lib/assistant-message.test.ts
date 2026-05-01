import { describe, expect, it } from "vitest";
import {
  describeToolCallState,
  previewPayload,
  viewMessage,
} from "./assistant-message";

describe("viewMessage", () => {
  it("returns an empty block list for messages with no parts", () => {
    expect(viewMessage({}).blocks).toEqual([]);
    expect(viewMessage({ parts: [] }).blocks).toEqual([]);
  });

  it("preserves non-empty text parts and drops blank ones", () => {
    const view = viewMessage({
      parts: [
        { type: "text", text: "Hello" },
        { type: "text", text: "   " },
        { type: "text", text: "world" },
      ],
    });
    expect(view.blocks).toEqual([
      { kind: "text", id: "0", text: "Hello" },
      { kind: "text", id: "2", text: "world" },
    ]);
  });

  it("captures reasoning parts and step markers", () => {
    const view = viewMessage({
      parts: [
        { type: "reasoning", text: "thinking..." },
        { type: "step-start" },
      ],
    });
    expect(view.blocks).toEqual([
      { kind: "reasoning", id: "0", text: "thinking..." },
      { kind: "step-start", id: "1" },
    ]);
  });

  it("normalises typed tool parts (tool-<name>)", () => {
    const view = viewMessage({
      parts: [
        {
          type: "tool-create_agent",
          state: "output-available",
          input: { id: "alpha" },
          output: { id: "alpha", model_id: "gpt" },
        },
      ],
    });
    expect(view.blocks).toEqual([
      {
        kind: "tool-call",
        id: "0",
        toolName: "create_agent",
        state: "output-available",
        input: { id: "alpha" },
        output: { id: "alpha", model_id: "gpt" },
        errorText: undefined,
      },
    ]);
  });

  it("normalises dynamic-tool parts and uses the embedded toolName", () => {
    const view = viewMessage({
      parts: [
        {
          type: "dynamic-tool",
          toolName: "list_models",
          state: "input-available",
          input: {},
        },
      ],
    });
    expect(view.blocks[0]).toMatchObject({
      kind: "tool-call",
      toolName: "list_models",
      state: "input-available",
    });
  });

  it("captures tool errors", () => {
    const view = viewMessage({
      parts: [
        {
          type: "tool-foo",
          state: "output-error",
          errorText: "boom",
        },
      ],
    });
    expect(view.blocks[0]).toMatchObject({
      kind: "tool-call",
      state: "output-error",
      errorText: "boom",
    });
  });

  it("classifies unknown future states defensively", () => {
    const view = viewMessage({
      parts: [{ type: "tool-foo", state: "future-state" }],
    });
    expect(view.blocks[0]).toMatchObject({
      kind: "tool-call",
      state: "unknown",
    });
  });

  it("captures unrecognised parts so the user sees them rather than nothing", () => {
    const view = viewMessage({ parts: [{ type: "future-feature" }] });
    expect(view.blocks).toEqual([
      { kind: "unknown", id: "0", type: "future-feature" },
    ]);
  });

  it("ignores garbage entries without crashing", () => {
    const view = viewMessage({ parts: [null, undefined, 42, "string"] });
    // None of these have a valid `type` string, so they all turn into
    // unknown placeholders with empty type.
    expect(view.blocks).toEqual([
      { kind: "unknown", id: "0", type: "" },
      { kind: "unknown", id: "1", type: "" },
      { kind: "unknown", id: "2", type: "" },
      { kind: "unknown", id: "3", type: "" },
    ]);
  });
});

describe("describeToolCallState", () => {
  it("maps every known state to a label + tone", () => {
    expect(describeToolCallState("output-available").tone).toBe("success");
    expect(describeToolCallState("output-error").tone).toBe("error");
    expect(describeToolCallState("approval-requested").tone).toBe("warn");
    expect(describeToolCallState("input-available").label).toBe(
      "Calling tool…",
    );
  });
});

describe("previewPayload", () => {
  it("returns null for undefined and empty strings", () => {
    expect(previewPayload(undefined)).toBeNull();
    expect(previewPayload("")).toBeNull();
  });

  it("returns strings verbatim", () => {
    expect(previewPayload("hello")).toBe("hello");
  });

  it("pretty-prints structured payloads", () => {
    expect(previewPayload({ a: 1 })).toBe('{\n  "a": 1\n}');
  });

  it("renders null literally", () => {
    expect(previewPayload(null)).toBe("null");
  });

  it("falls back to String() when JSON serialisation fails", () => {
    const cyclic: Record<string, unknown> = {};
    cyclic.self = cyclic;
    expect(previewPayload(cyclic)).toBe("[object Object]");
  });
});
