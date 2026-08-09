import { describe, expect, it } from "vitest";
import { assistantContextForLocation } from "./assistant-guidance";

describe("assistantContextForLocation", () => {
  it("routes every primary Console question to a curated help topic", () => {
    expect(assistantContextForLocation("/w/default/files").topic).toBe("files-artifacts");
    expect(assistantContextForLocation("/w/default/vaults").topic).toBe("runtime-secrets");
    expect(assistantContextForLocation("/w/default/protocols").topic).toBe("api-access");
  });

  it("keeps exact dynamic route and query context", () => {
    expect(assistantContextForLocation("/w/default/agents/lead", "?stage=advanced&section=orchestration"))
      .toMatchObject({
        label: "Agent",
        topic: "agent",
        path: "/w/default/agents/lead?stage=advanced&section=orchestration",
      });
  });
});
