// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, act, waitFor } from "@testing-library/react";
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
        <Route path="agents/:id" element={<div>agent-editor:agent-a</div>} />
        <Route path="assistant" element={<div>assistant-page</div>} />
      </Route>,
    ),
    { initialEntries: [initialPath] },
  );
  return render(<RouterProvider router={router} />);
}

function stubPaletteFetch({
  agents = [],
  tools = [],
}: {
  agents?: Array<{ id: string; model_id: string }>;
  tools?: Array<{ id: string; name: string; description: string }>;
} = {}) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: string | URL | Request) => {
      const href = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
      const body = href.includes("/v1/config/agents")
        ? { namespace: "agents", items: agents, offset: 0, limit: 100 }
        : {
            items: [],
            total: 0,
            agents: [],
            tools,
            plugins: [],
            skills: [],
            models: [],
            providers: [],
            namespaces: [],
          };
      return {
        ok: true,
        status: 200,
        text: async () => JSON.stringify(body),
      };
    }),
  );
}

beforeEach(() => {
  stubPaletteFetch();
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

  it("renders fetched agents and opens the selected agent with keyboard", async () => {
    stubPaletteFetch({
      agents: [{ id: "agent-a", model_id: "model-a" }],
      tools: [{ id: "tool-a", name: "Tool A", description: "Tool from registry" }],
    });
    renderHost();

    fireEvent.click(screen.getByText("open"));
    const input = await screen.findByPlaceholderText(/Search agents/i);
    fireEvent.change(input, { target: { value: "agent-a" } });
    await screen.findByText("agent-a");
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "ArrowUp" });
    fireEvent.keyDown(input, { key: "Enter" });

    await screen.findByText("agent-editor:agent-a");
    expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull();
  });

  it("opens tools through the assistant route and supports empty results", async () => {
    stubPaletteFetch({
      tools: [{ id: "tool.files.read", name: "Read file", description: "Read from filesystem" }],
    });
    renderHost();

    fireEvent.click(screen.getByText("open"));
    const input = await screen.findByPlaceholderText(/Search agents/i);
    fireEvent.change(input, { target: { value: "tool.files.read" } });
    await screen.findByText("Read from filesystem");
    fireEvent.click(screen.getByRole("button", { name: /tool\.files\.read/i }));

    await screen.findByText("assistant-page");
    expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull();

    fireEvent.click(screen.getByText("open"));
    const reopenedInput = await screen.findByPlaceholderText(/Search agents/i);
    fireEvent.change(reopenedInput, { target: { value: "no-such-command" } });
    expect(await screen.findByText("No matches.")).toBeDefined();
  });

  it("closes when the backdrop is clicked", async () => {
    renderHost();
    fireEvent.click(screen.getByText("open"));
    await screen.findByRole("dialog", { name: "Command palette" });

    fireEvent.click(screen.getByRole("dialog", { name: "Command palette" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull(),
    );
  });
});
