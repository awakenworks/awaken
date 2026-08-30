import { describe, expect, it } from "vitest";
import { assistantContextForLocation } from "./assistant-guidance";

describe("assistantContextForLocation", () => {
  it("routes every primary Console question to a curated help topic", () => {
    expect(assistantContextForLocation("/w/default/files").topic).toBe("files-artifacts");
    expect(assistantContextForLocation("/w/default/vaults").topic).toBe("runtime-secrets");
    expect(assistantContextForLocation("/w/default/protocols").topic).toBe("api-access");
    expect(assistantContextForLocation("/w/default/webhooks").topic).toBe("webhooks");
  });

  it("keeps exact dynamic route and query context", () => {
    expect(assistantContextForLocation("/w/default/agents/lead", "?stage=advanced&section=orchestration"))
      .toMatchObject({
        label: "Agent",
        topic: "agent",
        path: "/w/default/agents/lead?stage=advanced&section=orchestration",
      });
  });

  it("provides localized labels and starter questions for every surface", () => {
    const context = assistantContextForLocation("/w/default/protocols");
    expect(context.labelZh).toBe("API 与协议");
    expect(context.suggestionsZh).toHaveLength(context.suggestions.length);
    expect(context.suggestionsZh.every((suggestion) => /[\u3400-\u9fff]/.test(suggestion))).toBe(true);
  });
});
