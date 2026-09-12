import { describe, expect, it } from "vitest";
import { assistantSeedMessage } from "./assistant";
import { assistantSeedPath, starterDraftPath } from "./quickstart";

describe("quickstart landing hand-off", () => {
  it("carries a description to the one existing authoring loop", () => {
    // Describing an Agent is the Assistant's job; the landing page only routes
    // there. Coordinates are encoded so a workspace or description containing
    // reserved characters cannot forge a different route.
    expect(assistantSeedPath("workspace-a", "  a support agent  "))
      .toBe("/w/workspace-a/assistant?seed=a%20support%20agent");
    expect(assistantSeedPath("", "x")).toBe("/w/default/assistant?seed=x");
    expect(assistantSeedPath("team space", "a&b=c"))
      .toBe("/w/team%20space/assistant?seed=a%26b%3Dc");
  });

  it("round-trips that seed into exactly one opening message", () => {
    // The panel de-duplicates by id, so a reload re-derives the same id rather
    // than sending the description a second time.
    const first = assistantSeedMessage("?seed=a%20support%20agent");
    expect(first).toEqual({ id: "seed:a support agent", text: "a support agent" });
    expect(assistantSeedMessage("?seed=a%20support%20agent")).toEqual(first);
    expect(assistantSeedMessage("")).toBeUndefined();
    expect(assistantSeedMessage("?seed=%20%20")).toBeUndefined();
  });

  it("hands a Starter over by route so a reload reproduces the same draft", () => {
    // `template` already selects the Dream system Agent, so a Starter must not
    // reuse that key.
    expect(starterDraftPath("workspace-a", "evidence-brief"))
      .toBe("/w/workspace-a/agents/new?starter=evidence-brief");
    expect(starterDraftPath("workspace-a")).toBe("/w/workspace-a/agents/new");
    expect(starterDraftPath("")).toBe("/w/default/agents/new");
  });
});
