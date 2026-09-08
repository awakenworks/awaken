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

// L1 supported stored locale -> render it; L2 invalid or unavailable storage ->
// render English. This provider must not prevent the shared theme fallback from
// mounting, and it must never recover a Workspace coordinate from browser storage.
function LocaleProbe() {
  return createElement("output", null, useApp().locale);
}
it.each(["zh", "invalid", "blocked"])("renders with locale storage %s", (value) => {
  vi.stubGlobal("localStorage", {
    getItem: () => { if (value === "blocked") throw new Error("blocked"); return value; },
  });
  try {
    expect(renderToStaticMarkup(createElement(AppProvider, { children: createElement(LocaleProbe) })))
      .toBe(`<output>${value === "zh" ? "zh" : "en"}</output>`);
  } finally { vi.unstubAllGlobals(); }
});
