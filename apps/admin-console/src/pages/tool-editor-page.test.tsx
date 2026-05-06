// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router";
import { ToolEditorPage } from "./tool-editor-page";
import { withQueryClient } from "../test/query";

const tool = {
  id: "echo",
  name: "Echo",
  description: "Stock description",
  category: "debug",
  parameters_schema: {},
};

const meta = {
  source: { kind: "builtin", binary_version: "v1" },
  user_overrides: { description: "customized" },
  hidden: false,
  created_at: 0,
  updated_at: 0,
};

function hrefOf(input: string | URL | Request): string {
  if (typeof input === "string") return input;
  if (input instanceof URL) return input.href;
  return input.url;
}

function jsonResponse(data: unknown) {
  return {
    ok: true,
    status: 200,
    text: async () => JSON.stringify(data),
  };
}

function errorResponse(status: number, error: string) {
  return {
    ok: false,
    status,
    text: async () => JSON.stringify({ error }),
  };
}

function stubToolFetch(options: { metaResponse?: unknown; patchResponse?: unknown } = {}) {
  const fetchSpy = vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
    const href = hrefOf(url);
    const method = init?.method?.toUpperCase() ?? "GET";
    if (href.includes("/v1/config/tools/echo/overrides") && method === "PATCH") {
      if (options.patchResponse) return options.patchResponse;
      return jsonResponse({ ...tool, description: "patched" });
    }
    if (href.includes("/v1/config/tools/echo/overrides") && method === "DELETE") {
      return jsonResponse(tool);
    }
    if (href.endsWith("/v1/config/tools/echo/meta")) {
      if (options.metaResponse) return options.metaResponse;
      return jsonResponse(meta);
    }
    if (href.endsWith("/v1/config/tools/echo")) {
      return jsonResponse(tool);
    }
    return { ok: false, status: 404, text: async () => "" };
  });
  vi.stubGlobal("fetch", fetchSpy);
  return fetchSpy;
}

function findFetchCall(fetchSpy: ReturnType<typeof vi.fn>, pattern: string, method: string) {
  return fetchSpy.mock.calls.find(([url, init]) => {
    const requestMethod = (init as RequestInit | undefined)?.method?.toUpperCase() ?? "GET";
    return hrefOf(url as string | URL | Request).includes(pattern) && requestMethod === method;
  });
}

function renderEditor() {
  return render(
    withQueryClient(
      <MemoryRouter initialEntries={["/tools/echo"]}>
        <Routes>
          <Route path="/tools/:id" element={<ToolEditorPage />} />
        </Routes>
      </MemoryRouter>,
    ),
  );
}

describe("ToolEditorPage", () => {
  beforeEach(() => {
    stubToolFetch();
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.clearAllMocks();
  });

  it("shows builtin and editable description side-by-side", async () => {
    renderEditor();
    await waitFor(() => expect(screen.getByDisplayValue("Stock description")).toBeDefined());
    const matches = screen.getAllByText("Stock description");
    expect(matches.length).toBeGreaterThanOrEqual(1);
    expect(matches.some((el) => el.tagName === "P")).toBe(true);
  });

  it("save button patches only the changed description", async () => {
    const fetchSpy = stubToolFetch();
    renderEditor();
    const textarea = await waitFor(() => screen.getByDisplayValue("Stock description"));
    fireEvent.change(textarea, { target: { value: "Custom description" } });
    fireEvent.click(screen.getByRole("button", { name: /save/i }));

    await waitFor(() => {
      const patchCall = findFetchCall(fetchSpy, "/overrides", "PATCH");
      expect(patchCall).toBeDefined();
      expect(JSON.parse(String((patchCall?.[1] as RequestInit).body))).toEqual({
        description: "Custom description",
      });
    });
  });

  it("revert button clears tool overrides", async () => {
    const fetchSpy = stubToolFetch();
    renderEditor();
    await waitFor(() => screen.getByDisplayValue("Stock description"));
    fireEvent.click(screen.getByRole("button", { name: /revert/i }));

    await waitFor(() => {
      expect(findFetchCall(fetchSpy, "/overrides", "DELETE")).toBeDefined();
    });
  });

  it("warns about long descriptions over 400 chars", async () => {
    renderEditor();
    const textarea = await waitFor(() => screen.getByDisplayValue("Stock description"));
    fireEvent.change(textarea, { target: { value: "x".repeat(401) } });
    expect(screen.getByText(/dilute|长描述|attention/i)).toBeDefined();
  });

  it("shows metadata load errors instead of staying in loading state", async () => {
    stubToolFetch({ metaResponse: errorResponse(403, "metadata forbidden") });
    renderEditor();

    expect(await screen.findByText(/Tool metadata unavailable: metadata forbidden/i)).toBeDefined();
    expect(screen.queryByText("Loading…")).toBeNull();
  });

  it("shows mutation errors when saving overrides fails", async () => {
    stubToolFetch({ patchResponse: errorResponse(500, "patch failed") });
    renderEditor();
    const textarea = await waitFor(() => screen.getByDisplayValue("Stock description"));
    fireEvent.change(textarea, { target: { value: "Custom description" } });
    fireEvent.click(screen.getByRole("button", { name: /save/i }));

    expect(await screen.findByText(/Save failed: patch failed/i)).toBeDefined();
  });
});
