import { describe, expect, it } from "vitest";
import { NAV } from "./navigation/paths";
import { TOP_LEVEL_PAGE_INTENTS, pageIntentForPath } from "./page-intents";

describe("page intent coverage", () => {
  it("gives every visible navigation page a user-facing purpose", () => {
    expect(Object.keys(TOP_LEVEL_PAGE_INTENTS).sort()).toEqual(NAV.map((item) => item.key).sort());
    for (const item of NAV) {
      const intent = pageIntentForPath(item.path.replace(":ws", "team"));
      expect(intent?.title).toBeTruthy();
      expect(intent?.description).toBeTruthy();
      expect(intent?.outcome).toBeTruthy();
    }
  });

  it("covers every dedicated nested page", () => {
    for (const path of [
      "/w/team/agents/new",
      "/w/team/agents/support",
      "/w/team/sessions/sesn_1",
      "/w/team/memory/dreams/dream_1",
      "/w/team/assistant",
      "/w/team/credentials",
      "/w/team/dashboard",
      "/w/team/audit-log",
      "/w/team/datasets",
      "/w/team/eval-runs",
    ]) expect(pageIntentForPath(path), path).toBeDefined();
  });
});
