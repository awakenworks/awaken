// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { ToolsPanel } from "./tools-panel";
import type { AgentSpec, Capabilities } from "@/lib/config-api";

afterEach(() => {
  cleanup();
});

const emptyCapabilities: Capabilities = {
  agents: [],
  tools: [],
  plugins: [],
  skills: [],
  models: [],
  providers: [],
  namespaces: [],
};

const spec: AgentSpec = {
  id: "agent-a",
  model_id: "model-a",
  system_prompt: "",
  max_rounds: 8,
  plugin_ids: [],
  sections: {},
  delegates: [],
  allowed_tools: ["stale:*"],
  excluded_tools: [],
};

describe("ToolsPanel", () => {
  it("keeps catalog fields editable when no tools are published", () => {
    const updateField = vi.fn();

    render(
      <ToolsPanel spec={spec} capabilities={emptyCapabilities} updateField={updateField} />,
    );

    expect(screen.getByText("Allowed Tools")).toBeTruthy();
    expect(screen.getByText("Excluded Tools")).toBeTruthy();
    expect(screen.getByText("stale:*")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Remove" }));

    expect(updateField).toHaveBeenCalledWith("allowed_tools", []);
  });
});
