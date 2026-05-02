import { describe, expect, it } from "vitest";
import { navGroups, navIndex, resolveBreadcrumbs } from "./nav";

describe("navGroups", () => {
  it("indexes every nav item by path", () => {
    for (const group of navGroups) {
      for (const item of group.items) {
        expect(navIndex[item.path]).toBeDefined();
        expect(navIndex[item.path].id).toBe(item.id);
        expect(navIndex[item.path].group).toBe(group.label);
      }
    }
  });

  it("groups configure / observe / assistant exhaustively", () => {
    expect(navGroups.map((g) => g.label)).toEqual([
      "Configure",
      "Observe",
      "Assistant",
    ]);
  });
});

describe("resolveBreadcrumbs", () => {
  it("dashboard → Observe / Dashboard", () => {
    expect(resolveBreadcrumbs("/")).toEqual([
      { label: "Observe" },
      { label: "Dashboard" },
    ]);
  });

  it("agents list → Configure / Agents", () => {
    expect(resolveBreadcrumbs("/agents")).toEqual([
      { label: "Configure" },
      { label: "Agents" },
    ]);
  });

  it("agent new → Configure / Agents (link) / New", () => {
    expect(resolveBreadcrumbs("/agents/new")).toEqual([
      { label: "Configure" },
      { label: "Agents", path: "/agents" },
      { label: "New" },
    ]);
  });

  it("agent detail → Configure / Agents (link) / id", () => {
    expect(resolveBreadcrumbs("/agents/research-assistant")).toEqual([
      { label: "Configure" },
      { label: "Agents", path: "/agents" },
      { label: "research-assistant" },
    ]);
  });

  it("agent dashboard → Configure / Agents (link) / id (link) / Dashboard", () => {
    expect(resolveBreadcrumbs("/agents/research-assistant/dashboard")).toEqual([
      { label: "Configure" },
      { label: "Agents", path: "/agents" },
      { label: "research-assistant", path: "/agents/research-assistant" },
      { label: "Dashboard" },
    ]);
  });

  it("decodes encoded id segment", () => {
    expect(resolveBreadcrumbs("/agents/with%20space")).toEqual([
      { label: "Configure" },
      { label: "Agents", path: "/agents" },
      { label: "with space" },
    ]);
  });

  it("audit log → Observe / Audit Log", () => {
    expect(resolveBreadcrumbs("/audit-log")).toEqual([
      { label: "Observe" },
      { label: "Audit Log" },
    ]);
  });

  it("unknown path falls back to Admin", () => {
    expect(resolveBreadcrumbs("/no/such/route")).toEqual([{ label: "Admin" }]);
  });
});
