import { describe, expect, it } from "vitest";
import type { UIMessage } from "ai";
import { previewApplicationScope, uiMessageText } from "./SandboxPane";

describe("AI SDK Live Preview helpers", () => {
  it("maps the selected workspace to an opaque application scope", () => {
    expect(previewApplicationScope("project-42")).toBe("console:project-42");
    expect(previewApplicationScope("")).toBe("console:default");
  });

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
});
