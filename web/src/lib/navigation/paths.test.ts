import { describe, expect, it } from "vitest";
import { workspaceLabel } from "../app-state";
import { NAV, WORKSPACE_JOURNEY, navPath, titleForPath, visibleNavigation } from "./paths";

// Hosted navigation cause/effect decision table:
// T1 C1 BYOK enabled -> E1 expose Models & providers under Connect.
// T2 !C1 -> E2 expose the managed model catalog as Models under Author.
// Constraint: credential setup is owned by the model workflow, so navigation
// never introduces a second credential surface.
// Workspace label partition:
// T3 default -> E3 friendly Default; T4 hosted personal coordinate -> E4 Personal
// Workspace; T5 short authored name -> E5 preserve; T6 long opaque coordinate ->
// E6 bounded, recognizable prefix/suffix while the full value remains in title.
describe("deployment-aware navigation", () => {
  it("retains local provider authoring when BYOK is enabled", () => {
    expect(visibleNavigation(true).find((item) => item.key === "models")).toMatchObject({
      label: "Models & providers",
      group: "connect",
    });
  });

  it("projects only the Cloud model catalog when BYOK is disabled", () => {
    const navigation = visibleNavigation(false);
    expect(navigation.some((item) => item.key === "credentials")).toBe(false);
    expect(navigation.find((item) => item.key === "models")).toMatchObject({
      label: "Models",
      group: "author",
    });
  });
});

describe("workspaceLabel", () => {
  it("bounds opaque coordinates without changing authored short names", () => {
    expect(workspaceLabel("default")).toBe("Default");
    expect(workspaceLabel("awaken:personal:acct_0123456789")).toBe("Personal Workspace");
    expect(workspaceLabel("product-design")).toBe("product-design");
    expect(workspaceLabel("workspace_abcdefghijklmnopqrstuvwxyz_0123456789")).toBe(
      "workspace_ab…23456789",
    );
  });
});
describe("console information architecture", () => {
  it("keeps one source-of-truth route for every primary surface", () => {
    expect(NAV.map((item) => item.group)).toEqual([
      "workspace",
      "author",
      "author",
      "author",
      "author",
      "run",
      "run",
      "run",
      "run",
      "connect",
      "connect",
      "connect",
      "connect",
      "govern",
      "govern",
      "govern",
    ]);
    expect(NAV.find((item) => item.key === "files")).toMatchObject({
      group: "author",
      sectionLabel: "Resources",
    });
    expect(NAV.find((item) => item.key === "artifacts")).toMatchObject({
      group: "run",
      path: "/w/:ws/artifacts",
    });
    expect(NAV.find((item) => item.key === "mcp")).toMatchObject({
      group: "connect",
      path: "/w/:ws/mcp",
    });
    expect(NAV.some((item) => item.key === "credentials")).toBe(false);
  });

  it("resolves the added workspace paths and titles", () => {
    const artifacts = NAV.find((item) => item.key === "artifacts")!;
    expect(navPath(artifacts, "workspace-a")).toBe("/w/workspace-a/artifacts");
    expect(titleForPath("/w/workspace-a/mcp")).toEqual({
      scope: "workspace-a",
      title: "MCP overview",
    });
  });

  /**
   * Overview journey decision table.
   * Causes: C1 an operator enters the Workspace overview; C2 NAV owns the
   * current route for every destination; C3 labels may be English or Chinese.
   * Effects: E1 the product intent is Connect→Build→Run→Observe; E2 every step
   * reuses the exact NAV object and therefore its canonical route; E3 locale
   * changes copy only, never ownership or order. Rule R1: C1+C2+C3 ->
   * E1+E2+E3, with no parallel route registry or lifecycle state.
   */
  it("expresses the Agent proof journey over canonical navigation references", () => {
    expect(WORKSPACE_JOURNEY.map((step) => step.label)).toEqual(["Connect", "Build", "Run", "Observe"]);
    expect(WORKSPACE_JOURNEY.map((step) => step.destination)).toEqual([
      NAV.find((item) => item.key === "models"),
      NAV.find((item) => item.key === "agents"),
      NAV.find((item) => item.key === "sessions"),
      NAV.find((item) => item.key === "artifacts"),
    ]);
    expect(navPath(WORKSPACE_JOURNEY[2].destination, "workspace-a")).toBe("/w/workspace-a/sessions");
  });
});
