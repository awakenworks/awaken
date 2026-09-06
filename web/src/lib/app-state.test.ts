import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AppProvider, useApp } from "./app-state";

function WorkspaceProbe() {
  return createElement("output", null, useApp().workspaceId);
}

describe("initial presentation workspace", () => {
  afterEach(() => vi.unstubAllGlobals());

  // Causes: route present/absent, valid/invalid URI escape, stale storage.
  // Rules: W1 valid named route -> decode exactly once; W2 default, absent,
  // or malformed route -> presentation default. Stale storage never selects
  // scope. SSR exercises the actual provider initializer, not a copied parser.
  it.each([
    ["/w/awaken%3Atenant/overview", "awaken:tenant"],
    ["/w/tenant%252Fpart/overview", "tenant%2Fpart"],
    ["/w/team/overview", "team"],
    ["/w/default/overview", "default"],
    ["/w/%E0%A4%A/overview", "default"],
    ["/w//overview", "default"],
    ["/overview", "default"],
    [undefined, "default"],
  ])("initializes %s as %s", (pathname, expected) => {
    vi.stubGlobal("location", pathname === undefined ? undefined : { pathname });
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => key === "awaken.console.workspace" ? "stale" : null,
    });
    expect(renderToStaticMarkup(createElement(AppProvider, {
      children: createElement(WorkspaceProbe),
    }))).toBe(`<output>${expected}</output>`);
  });
});
