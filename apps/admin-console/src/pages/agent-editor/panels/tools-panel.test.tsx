// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { ToolsPanel } from "./tools-panel";
import type { AgentSpec, Capabilities } from "@/lib/config-api";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function spec(overrides: Partial<AgentSpec> = {}): AgentSpec {
  return {
    id: "a",
    model_id: "m",
    system_prompt: "p",
    ...overrides,
  };
}

function emptyCapabilities(): Capabilities {
  return {
    agents: [],
    tools: [],
    plugins: [],
    skills: [],
    models: [],
    providers: [],
    namespaces: [],
  };
}

describe("ToolsPanel", () => {
  it("shows the loading state while capabilities are unresolved", () => {
    render(<ToolsPanel spec={spec()} capabilities={null} updateField={vi.fn()} />);
    expect(screen.getByText(/Loading published tool capabilities/i)).toBeTruthy();
    // Pattern editors must NOT be present yet — we don't know what's registered.
    expect(screen.queryByTestId("tool-selector-allowed")).toBeNull();
  });

  it("renders pattern editors even when the registry is empty (forward-config)", () => {
    // Regression: the old gate `!capabilities || !tools.length` blocked
    // operators from authoring `excluded_tool_patterns: ["dangerous-*"]`
    // before any tool was registered. The editors must render against an
    // empty registry; ToolSelector itself surfaces "No tools registered."
    render(<ToolsPanel spec={spec()} capabilities={emptyCapabilities()} updateField={vi.fn()} />);
    expect(screen.getByTestId("tool-selector-allowed")).toBeTruthy();
    expect(screen.getByTestId("tool-selector-excluded")).toBeTruthy();
    // Pattern inputs are present so users can add forward-config patterns.
    expect(screen.getAllByPlaceholderText("e.g. mcp:*").length).toBeGreaterThan(0);
  });
});
