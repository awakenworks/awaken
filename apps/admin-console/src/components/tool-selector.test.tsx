// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { ToolSelector } from "./tool-selector";
import type { ComponentProps } from "react";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

const REGISTERED = ["Bash", "Read", "mcp:weather", "mcp:fs"];

function renderIt(overrides: Partial<ComponentProps<typeof ToolSelector>> = {}) {
  const onChange = overrides.onChange ?? vi.fn();
  render(
    <ToolSelector
      label="Allowed"
      registered={REGISTERED}
      literals={[]}
      patterns={[]}
      onChange={onChange}
      {...overrides}
    />,
  );
  return { onChange };
}

describe("ToolSelector", () => {
  it("checking a tool adds it to literals", () => {
    const { onChange } = renderIt();
    const bashLabel = screen.getByText("Bash").closest("label")!;
    const checkbox = bashLabel.querySelector(
      "input[type='checkbox']",
    ) as HTMLInputElement;
    fireEvent.click(checkbox);
    expect(onChange).toHaveBeenCalledWith({ literals: ["Bash"], patterns: [] });
  });

  it("unchecking a tool removes it from literals", () => {
    const { onChange } = renderIt({ literals: ["Bash"] });
    const bashLabel = screen.getByText("Bash").closest("label")!;
    const checkbox = bashLabel.querySelector(
      "input[type='checkbox']",
    ) as HTMLInputElement;
    expect(checkbox.checked).toBe(true);
    fireEvent.click(checkbox);
    expect(onChange).toHaveBeenCalledWith({ literals: [], patterns: [] });
  });

  it("adding a pattern appends to patterns", () => {
    const { onChange } = renderIt();
    fireEvent.change(screen.getByPlaceholderText("e.g. mcp:*"), {
      target: { value: "mcp:*" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add pattern" }));
    expect(onChange).toHaveBeenCalledWith({ literals: [], patterns: ["mcp:*"] });
  });

  it("removing a pattern drops it", () => {
    const { onChange } = renderIt({ patterns: ["mcp:*"] });
    fireEvent.click(
      screen.getByRole("button", { name: "Remove pattern mcp:*" }),
    );
    expect(onChange).toHaveBeenCalledWith({ literals: [], patterns: [] });
  });

  it("'Allow all tools' button adds *", () => {
    const { onChange } = renderIt();
    fireEvent.click(screen.getByRole("button", { name: "Allow all tools" }));
    expect(onChange).toHaveBeenCalledWith({ literals: [], patterns: ["*"] });
  });

  it("'Seed literals from registry' snapshots the registered list", () => {
    const { onChange } = renderIt();
    fireEvent.click(
      screen.getByRole("button", { name: "Seed literals from registry" }),
    );
    expect(onChange).toHaveBeenCalledWith({
      literals: ["Bash", "Read", "mcp:weather", "mcp:fs"],
      patterns: [],
    });
  });

  it("shows match count next to each pattern", () => {
    renderIt({ patterns: ["mcp:*", "never-*"] });
    expect(screen.getByText(/matches 2: mcp:weather/)).toBeTruthy();
    expect(screen.getByText(/matches none/)).toBeTruthy();
  });

  it("'Exclude all tools' button copy when label is 'Excluded'", () => {
    const { onChange } = renderIt({ label: "Excluded" });
    fireEvent.click(screen.getByRole("button", { name: "Exclude all tools" }));
    expect(onChange).toHaveBeenCalledWith({ literals: [], patterns: ["*"] });
  });
});
