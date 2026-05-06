// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router";
import { ToolEditorPage } from "./tool-editor-page";

const stockTool = {
  id: "echo",
  name: "Echo",
  description: "Stock description",
  category: "debug",
  parameters_schema: {},
};

vi.mock("@/lib/config-api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/config-api")>("@/lib/config-api");
  const tool = {
    id: "echo",
    name: "Echo",
    description: "Stock description",
    category: "debug",
    parameters_schema: {},
  };
  return {
    ...actual,
    configApi: {
      getTool: vi.fn().mockResolvedValue(tool),
      getMeta: vi.fn().mockResolvedValue({
        source: { kind: "builtin", binary_version: "v1" },
        user_overrides: { description: "customized" },
        hidden: false,
        created_at: 0,
        updated_at: 0,
      }),
      patchToolOverrides: vi.fn().mockResolvedValue({ ...tool, description: "patched" }),
      clearToolOverrides: vi.fn().mockResolvedValue(tool),
    },
  };
});

function renderEditor() {
  return render(
    <MemoryRouter initialEntries={["/tools/echo"]}>
      <Routes>
        <Route path="/tools/:id" element={<ToolEditorPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe("ToolEditorPage", () => {
  beforeEach(() => vi.clearAllMocks());

  it("shows builtin and editable description side-by-side", async () => {
    renderEditor();
    await waitFor(() => expect(screen.getByDisplayValue("Stock description")).toBeDefined());
    // Both the readonly builtin <p> and the editable textarea show the same text
    const matches = screen.getAllByText("Stock description");
    expect(matches.length).toBeGreaterThanOrEqual(1);
    // The builtin readonly view is a <p>
    expect(matches.some((el) => el.tagName === "P")).toBe(true);
  });

  it("save button calls patchToolOverrides only with changed description", async () => {
    const { configApi } = await import("@/lib/config-api");
    renderEditor();
    const textarea = await waitFor(() => screen.getByDisplayValue("Stock description"));
    fireEvent.change(textarea, { target: { value: "Custom description" } });
    fireEvent.click(screen.getByRole("button", { name: /save/i }));
    await waitFor(() => {
      expect(configApi.patchToolOverrides).toHaveBeenCalledWith("echo", { description: "Custom description" });
    });
  });

  it("revert button calls clearToolOverrides", async () => {
    const { configApi } = await import("@/lib/config-api");
    renderEditor();
    await waitFor(() => screen.getByDisplayValue("Stock description"));
    fireEvent.click(screen.getByRole("button", { name: /revert/i }));
    await waitFor(() => {
      expect(configApi.clearToolOverrides).toHaveBeenCalledWith("echo");
    });
  });

  it("warns about long descriptions over 400 chars", async () => {
    renderEditor();
    const textarea = await waitFor(() => screen.getByDisplayValue("Stock description"));
    fireEvent.change(textarea, { target: { value: "x".repeat(401) } });
    expect(screen.getByText(/dilute|长描述|attention/i)).toBeDefined();
  });
});
