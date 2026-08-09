import { describe, expect, it } from "vitest";
import type { UIMessage } from "ai";
import { draftPreviewRequest, draftPreviewSignature, previewApplicationScope, uiMessageText } from "./SandboxPane";

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
});
