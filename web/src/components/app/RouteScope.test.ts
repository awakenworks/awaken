import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, expect, it, vi } from "vitest";

const state = vi.hoisted(() => ({
  pathname: "/",
  workspaceId: "stale",
  setWorkspaceId: vi.fn(),
  effects: [] as Array<() => unknown>,
}));
vi.mock("react", async (original) => ({
  ...await original<typeof import("react")>(),
  useEffect: (effect: () => unknown) => state.effects.push(effect),
}));
vi.mock("react-router", async (original) => ({
  ...await original<typeof import("react-router")>(),
  useLocation: () => ({ pathname: state.pathname }),
}));
vi.mock("../../lib/app-state", async (original) => ({
  ...await original<typeof import("../../lib/app-state")>(),
  useApp: () => state,
}));
import { RouteScope } from "./AppShell";

afterEach(() => vi.clearAllMocks());

it("projects successive route changes through the same decoded workspace seam", () => {
  // Causes: navigation changes, stale presentation, URI encoding, unchanged
  // decoded scope. R1 changed valid -> decoded identity; R2 malformed/absent
  // -> default; R3 unchanged -> no update. Flush the component's real effect
  // callback explicitly: this is a component decision test, not browser/React
  // scheduler verification. The server still authorizes every selected scope.
  for (const [pathname, expected] of [
    ["/w/awaken%3Atenant/overview", "awaken:tenant"],
    ["/w/awaken%3Atenant/sessions", "awaken:tenant"],
    ["/w/tenant%252Fpart/overview", "tenant%2Fpart"],
    ["/w/%E0%A4%A/overview", "default"],
    ["/w/team/overview", "team"],
    ["/overview", "default"],
  ]) {
    const previous = state.workspaceId;
    state.pathname = pathname;
    state.setWorkspaceId.mockClear();
    renderToStaticMarkup(createElement(RouteScope));
    state.effects.splice(0).forEach((effect) => effect());
    if (previous === expected) expect(state.setWorkspaceId).not.toHaveBeenCalled();
    else expect(state.setWorkspaceId).toHaveBeenCalledExactlyOnceWith(expected);
    state.workspaceId = expected;
  }
});
