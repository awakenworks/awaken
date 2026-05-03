// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, act } from "@testing-library/react";
import { createMemoryRouter, RouterProvider, createRoutesFromElements, Route, Outlet } from "react-router";
import "../lib/i18n";
import { CommandPaletteProvider, useCommandPalette } from "./command-palette";

function HostShell() {
  const palette = useCommandPalette();
  return (
    <div>
      <button onClick={palette.open}>open</button>
      <Outlet />
    </div>
  );
}

function renderHost(initialPath = "/") {
  const router = createMemoryRouter(
    createRoutesFromElements(
      <Route
        path="/"
        element={
          <CommandPaletteProvider>
            <HostShell />
          </CommandPaletteProvider>
        }
      >
        <Route index element={<div>home</div>} />
        <Route path="agents" element={<div>agents-page</div>} />
        <Route path="agents/new" element={<div>new-agent</div>} />
      </Route>,
    ),
    { initialEntries: [initialPath] },
  );
  return render(<RouterProvider router={router} />);
}

beforeEach(() => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => ({
      ok: true,
      status: 200,
      text: async () =>
        JSON.stringify({
          items: [],
          total: 0,
          agents: [],
          tools: [],
          plugins: [],
          skills: [],
          models: [],
          providers: [],
          namespaces: [],
        }),
    })),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("CommandPalette", () => {
  it("opens via the host button and renders the input", async () => {
    renderHost();
    fireEvent.click(screen.getByText("open"));
    const input = await screen.findByPlaceholderText(/Search agents/i);
    expect(input).toBeDefined();
  });

  it("opens via Cmd+K and closes on Escape", async () => {
    renderHost();
    act(() => {
      fireEvent.keyDown(window, { key: "k", metaKey: true });
    });
    expect(await screen.findByPlaceholderText(/Search agents/i)).toBeDefined();
    act(() => {
      fireEvent.keyDown(window, { key: "Escape" });
    });
    expect(screen.queryByPlaceholderText(/Search agents/i)).toBeNull();
  });

  it("filters items by query", async () => {
    renderHost();
    fireEvent.click(screen.getByText("open"));
    const input = await screen.findByPlaceholderText(/Search agents/i);
    fireEvent.change(input, { target: { value: "audit" } });
    expect(await screen.findByText("Audit Log")).toBeDefined();
  });
});
