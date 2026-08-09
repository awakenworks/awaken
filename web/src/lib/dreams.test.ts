import { describe, expect, it } from "vitest";
import type { ManagedModel, Session } from "./api/types";
import { eligibleDreamSessions, isDreamTerminal, readyDreamModels } from "./dreams";

describe("Dream UI decisions", () => {
  it("offers only supported models that are active in the workspace catalog", () => {
    const executable = [
      { id: "claude-sonnet-5", display_name: "Claude Sonnet 5" },
      { id: "provider/other", display_name: "other" },
    ] as ManagedModel[];
    expect(readyDreamModels(["claude-sonnet-5", "claude-opus-4-8"], executable))
      .toEqual(["claude-sonnet-5"]);
  });

  it("excludes running, archived, and Dream auxiliary sessions", () => {
    const session = (id: string, status = "idle", metadata = {}, archived_at: string | null = null) => ({
      id, status, metadata, archived_at,
    }) as Session;
    expect(eligibleDreamSessions([
      session("ready"),
      session("running", "running"),
      session("aux", "idle", { "awaken.session.origin": "dream" }),
      session("archived", "idle", {}, "now"),
    ]).map((item) => item.id)).toEqual(["ready"]);
  });

  it("allows archive only for terminal states", () => {
    expect(isDreamTerminal("pending")).toBe(false);
    expect(isDreamTerminal("running")).toBe(false);
    expect(isDreamTerminal("completed")).toBe(true);
    expect(isDreamTerminal("failed")).toBe(true);
    expect(isDreamTerminal("canceled")).toBe(true);
  });
});
