import { describe, expect, it } from "vitest";
import {
  navGroups,
  navIndex,
  resolveBreadcrumbs,
  type BreadcrumbCrumb,
} from "./nav";

/** Drop labelKey from crumbs so tests focus on the resolved literals. */
function plain(crumbs: BreadcrumbCrumb[]): BreadcrumbCrumb[] {
  return crumbs.map(({ labelKey: _l, ...rest }) => rest);
}

describe("navGroups (IA v2.4 — topology layers)", () => {
  it("indexes every nav item by path", () => {
    for (const group of navGroups) {
      for (const item of group.items) {
        expect(navIndex[item.path]).toBeDefined();
        expect(navIndex[item.path].id).toBe(item.id);
        expect(navIndex[item.path].group).toBe(group.label);
      }
    }
  });

  it("groups follow the agents → resources → infrastructure → observe order", () => {
    expect(navGroups.map((g) => g.label)).toEqual([
      "Agents",
      "Resources",
      "Infrastructure",
      "Observe",
    ]);
  });

  it("Skills sits in Resources, not Observe", () => {
    expect(navIndex["/skills"].group).toBe("Resources");
  });

  it("Providers sits in Infrastructure, not Resources", () => {
    expect(navIndex["/providers"].group).toBe("Infrastructure");
  });

  it("Models sits in Infrastructure with Providers", () => {
    expect(navIndex["/models"].group).toBe("Infrastructure");
  });
});

describe("resolveBreadcrumbs", () => {
  it("dashboard → Observe / Dashboard", () => {
    expect(plain(resolveBreadcrumbs("/"))).toEqual([
      { label: "Observe" },
      { label: "Dashboard" },
    ]);
  });

  it("agents list → Agents (group label collapses with page label)", () => {
    expect(plain(resolveBreadcrumbs("/agents"))).toEqual([{ label: "Agents" }]);
  });

  it("agent new → Agents (link) / New", () => {
    expect(plain(resolveBreadcrumbs("/agents/new"))).toEqual([
      { label: "Agents", path: "/agents" },
      { label: "New" },
    ]);
  });

  it("agent detail → Agents (link) / id", () => {
    expect(plain(resolveBreadcrumbs("/agents/research-assistant"))).toEqual([
      { label: "Agents", path: "/agents" },
      { label: "research-assistant" },
    ]);
  });

  it("agent dashboard → Agents (link) / id (link) / Dashboard", () => {
    expect(plain(resolveBreadcrumbs("/agents/research-assistant/dashboard"))).toEqual([
      { label: "Agents", path: "/agents" },
      { label: "research-assistant", path: "/agents/research-assistant" },
      { label: "Dashboard" },
    ]);
  });

  it("decodes encoded id segment", () => {
    expect(plain(resolveBreadcrumbs("/agents/with%20space"))).toEqual([
      { label: "Agents", path: "/agents" },
      { label: "with space" },
    ]);
  });

  it("audit log → Observe / Audit Log", () => {
    expect(plain(resolveBreadcrumbs("/audit-log"))).toEqual([
      { label: "Observe" },
      { label: "Audit Log" },
    ]);
  });

  it("skills → Resources / Skills", () => {
    expect(plain(resolveBreadcrumbs("/skills"))).toEqual([
      { label: "Resources" },
      { label: "Skills" },
    ]);
  });

  it("providers → Infrastructure / Providers", () => {
    expect(plain(resolveBreadcrumbs("/providers"))).toEqual([
      { label: "Infrastructure" },
      { label: "Providers" },
    ]);
  });

  it("models → Infrastructure / Models", () => {
    expect(plain(resolveBreadcrumbs("/models"))).toEqual([
      { label: "Infrastructure" },
      { label: "Models" },
    ]);
  });

  it("assistant route keeps breadcrumbs without a sidebar nav item", () => {
    expect(plain(resolveBreadcrumbs("/assistant"))).toEqual([
      { label: "Assistant" },
      { label: "AI Assistant" },
    ]);
  });

  it("mcp server detail → Resources / MCP Servers (link) / id", () => {
    expect(plain(resolveBreadcrumbs("/mcp-servers/github"))).toEqual([
      { label: "Resources" },
      { label: "MCP Servers", path: "/mcp-servers" },
      { label: "github" },
    ]);
  });

  it("skill detail → Resources / Skills (link) / id (decoded)", () => {
    expect(plain(resolveBreadcrumbs("/skills/code%20review"))).toEqual([
      { label: "Resources" },
      { label: "Skills", path: "/skills" },
      { label: "code review" },
    ]);
  });

  it("unknown path falls back to Admin", () => {
    expect(plain(resolveBreadcrumbs("/no/such/route"))).toEqual([{ label: "Admin" }]);
  });
});
