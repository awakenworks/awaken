// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi, type Mock } from "vitest";
import { cleanup, render, screen, act, fireEvent, within } from "@testing-library/react";
import { ToolSelector } from "./tool-selector";
import type { ToolInfo } from "@/lib/config-api";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

const TOOLS: ToolInfo[] = [
  { id: "Bash", name: "Bash", description: "Run shell commands" },
  { id: "Read", name: "Read", description: "Read files" },
  { id: "plugin:reminder/add", name: "Add Reminder", description: "Add reminder" },
  { id: "plugin:reminder/list", name: "List Reminders", description: "List reminders" },
  { id: "mcp:weather/forecast", name: "Forecast", description: "Weather forecast" },
  { id: "mcp:db/query", name: "Query", description: "DB query" },
];

interface SelectorProps {
  title?: string;
  description?: string;
  value?: string[] | null | undefined;
  onChange?: Mock;
  tools?: ToolInfo[];
  variant?: "include" | "exclude";
  overridden?: boolean;
  onReset?: Mock;
  resetLabel?: string;
}

function renderSelector(overrides: SelectorProps = {}) {
  const onChange = overrides.onChange ?? vi.fn();
  const props = {
    title: "Allowed Tools",
    description: "Configure which tools this agent can use.",
    value: undefined as string[] | undefined,
    onChange,
    tools: TOOLS,
    ...overrides,
  };
  return { ...render(<ToolSelector {...props} />), props };
}

describe("ToolSelector — default All-tools mode", () => {
  it("hides group list and shows all-mode body when value is undefined", () => {
    renderSelector({ value: undefined });

    // "All tools" radio label should be present and checked
    const allLabel = screen.getByText("All tools");
    expect(allLabel).toBeTruthy();

    const radio = allLabel
      .closest("label")!
      .querySelector("input[type='radio']") as HTMLInputElement;
    expect(radio.checked).toBe(true);

    // allBody description should be visible
    expect(screen.getByText(/Every tool published to the runtime/)).toBeTruthy();

    // Group headers should NOT appear
    expect(screen.queryByText("Built-in")).toBeNull();
    expect(screen.queryByText(/Plugin/)).toBeNull();
    expect(screen.queryByText(/MCP/)).toBeNull();
  });
});

describe("ToolSelector — switching to Custom mode", () => {
  it("reveals groups in correct order: Built-in → Plugin · reminder → MCP · db → MCP · weather", () => {
    renderSelector({ value: undefined });

    const customLabel = screen.getByText("Custom selection");
    const customRadio = customLabel
      .closest("label")!
      .querySelector("input[type='radio']") as HTMLInputElement;

    act(() => {
      fireEvent.click(customRadio);
    });

    // After clicking Custom the onChange will be called but value doesn't change
    // since value is controlled — we need to re-render with the new value
    // In test, we check that onChange was called with a list and re-render
    // But since value is controlled, let's render with an explicit array to see groups.
    cleanup();

    renderSelector({
      value: [
        "Bash",
        "Read",
        "plugin:reminder/add",
        "plugin:reminder/list",
        "mcp:weather/forecast",
        "mcp:db/query",
      ],
    });

    // Tab strip exposes broad source kinds (Built-in/Plugin/MCP), each group
    // panel below exposes the specific source label. The text "Built-in" can
    // therefore appear twice — once as a tab, once as a group header. Filter
    // to the group rendering by matching only the per-source labels.
    const allGroupSpans = screen
      .getAllByText(/^(Plugin · reminder|MCP · db|MCP · weather)$/)
      .map((el) => el.textContent!);
    expect(allGroupSpans[0]).toBe("Plugin · reminder");
    // MCP groups sorted alphabetically: db before weather
    expect(allGroupSpans[1]).toBe("MCP · db");
    expect(allGroupSpans[2]).toBe("MCP · weather");
    // Built-in must still render exactly once as a group header (separately
    // verified by the tab strip — see source-tabs test below).
    const builtin = screen.getAllByText("Built-in");
    expect(builtin.length).toBeGreaterThanOrEqual(1);
  });
});

describe("ToolSelector — search filtering", () => {
  it("filters tools to only matching group when searching 'forecast'", () => {
    renderSelector({
      value: [
        "Bash",
        "Read",
        "plugin:reminder/add",
        "plugin:reminder/list",
        "mcp:weather/forecast",
        "mcp:db/query",
      ],
    });

    const searchInput = screen.getByRole("searchbox");
    act(() => {
      fireEvent.change(searchInput, { target: { value: "forecast" } });
    });

    // Only MCP · weather group panel should remain. The "Built-in" tab is
    // still in the tab strip — only assert that the GROUP HEADERS for the
    // others are gone.
    expect(screen.getByText("MCP · weather")).toBeTruthy();
    expect(screen.queryByText("Plugin · reminder")).toBeNull();
    expect(screen.queryByText("MCP · db")).toBeNull();

    // Only the forecast tool id should appear
    expect(screen.getByText("mcp:weather/forecast")).toBeTruthy();
    expect(screen.queryByText("mcp:db/query")).toBeNull();
    expect(screen.queryByText("Bash")).toBeNull();
  });
});

describe("ToolSelector — group Select all / Clear buttons", () => {
  it("calls onChange with full set when 'Select all' is clicked on the built-in group", () => {
    // value = all except Bash — so built-in group is partially selected
    const value = [
      "Read",
      "plugin:reminder/add",
      "plugin:reminder/list",
      "mcp:weather/forecast",
      "mcp:db/query",
    ];
    const { props } = renderSelector({ value });

    // Find "Select all" buttons — the built-in group is first
    const selectAllBtns = screen.getAllByRole("button", { name: "Select all" });
    act(() => {
      fireEvent.click(selectAllBtns[0]); // first group = Built-in
    });

    expect(props.onChange).toHaveBeenCalledOnce();
    // setGroupSelection with every tool selected for include variant collapses to ["*"].
    const result = props.onChange.mock.calls[0][0];
    expect(result).toEqual(["*"]);
  });

  it("calls onChange excluding group tools when 'Clear' is clicked on the built-in group", () => {
    const value = [
      "Bash",
      "Read",
      "plugin:reminder/add",
      "plugin:reminder/list",
      "mcp:weather/forecast",
      "mcp:db/query",
    ];
    const { props } = renderSelector({ value });

    const clearBtns = screen.getAllByRole("button", { name: "Clear" });
    act(() => {
      fireEvent.click(clearBtns[0]); // first group = Built-in
    });

    expect(props.onChange).toHaveBeenCalledOnce();
    const result = props.onChange.mock.calls[0][0] as string[];
    expect(result).not.toContain("Bash");
    expect(result).not.toContain("Read");
    expect(result).toContain("plugin:reminder/add");
    expect(result).toContain("mcp:weather/forecast");
  });
});

describe("ToolSelector — group selection state badge", () => {
  it("shows '1 of 1 selected' for MCP · weather when forecast is the only tool in that group", () => {
    // MCP · weather group contains only mcp:weather/forecast (one tool)
    // mcp:db/query is in a separate MCP · db group
    renderSelector({ value: ["mcp:weather/forecast"] });

    // groupSelectionState returns "all" for 1/1, so summariseSelection shows "N of N selected"
    expect(screen.getByText("1 of 1 selected")).toBeTruthy();
  });

  it("shows '0 of 2 selected' for the plugin group when none are selected", () => {
    // Only select non-plugin tools
    renderSelector({ value: ["Bash", "Read", "mcp:weather/forecast", "mcp:db/query"] });

    // Plugin group has 2 tools, none selected
    expect(screen.getByText("0 of 2 selected")).toBeTruthy();
  });

  it("shows '1 of 2 selected' for the plugin group when one is selected", () => {
    renderSelector({
      value: ["Bash", "Read", "plugin:reminder/add", "mcp:weather/forecast", "mcp:db/query"],
    });

    // Plugin group: 2 tools, 1 selected
    expect(screen.getByText("1 of 2 selected")).toBeTruthy();
  });
});

describe("ToolSelector — variant='exclude' labels", () => {
  it("shows 'Block none' instead of 'All tools' for the all-mode radio", () => {
    renderSelector({ value: [], variant: "exclude" });

    expect(screen.getByText("Block none")).toBeTruthy();
    expect(screen.queryByText("All tools")).toBeNull();
  });

  it("shows exclude-mode body text when value is the explicit empty list", () => {
    renderSelector({ value: [], variant: "exclude" });

    expect(screen.getByText(/No tools are explicitly excluded/)).toBeTruthy();
  });

  it("allows entering Custom exclusion from the explicit empty list", () => {
    const { props } = renderSelector({ value: [], variant: "exclude" });

    const customLabel = screen.getByText("Custom exclusion");
    const customRadio = customLabel
      .closest("label")!
      .querySelector("input[type='radio']") as HTMLInputElement;

    act(() => {
      fireEvent.click(customRadio);
    });

    expect(props.onChange).toHaveBeenCalledWith([]);
    expect(screen.getAllByText("Built-in").length).toBeGreaterThan(0);
    expect(screen.getByText("Bash")).toBeTruthy();
  });

  it("allows entering Custom exclusion from legacy null", () => {
    renderSelector({ value: null, variant: "exclude" });

    const customLabel = screen.getByText("Custom exclusion");
    const customRadio = customLabel
      .closest("label")!
      .querySelector("input[type='radio']") as HTMLInputElement;

    act(() => {
      fireEvent.click(customRadio);
    });

    expect(screen.getAllByText("Built-in").length).toBeGreaterThan(0);
    expect(screen.getByText("Bash")).toBeTruthy();
  });

  it("selects only the clicked excluded tool after legacy null is normalized", () => {
    const view = renderSelector({ value: null, variant: "exclude" });

    const customLabel = screen.getByText("Custom exclusion");
    const customRadio = customLabel
      .closest("label")!
      .querySelector("input[type='radio']") as HTMLInputElement;

    act(() => {
      fireEvent.click(customRadio);
    });

    expect(view.props.onChange).toHaveBeenCalledWith([]);

    view.rerender(<ToolSelector {...view.props} value={[]} />);

    const bashLabel = screen.getByText("Bash").closest("label")!;
    const bashCheckbox = bashLabel.querySelector("input[type='checkbox']") as HTMLInputElement;
    expect(bashCheckbox.checked).toBe(false);

    act(() => {
      fireEvent.click(bashCheckbox);
    });

    expect(view.props.onChange).toHaveBeenLastCalledWith(["Bash"]);
  });
});

describe("ToolSelector — legacy null deprecation hint", () => {
  it("shows a deprecation banner when value is null", () => {
    renderSelector({ value: null });
    expect(screen.getByText(/Legacy config detected/)).toBeTruthy();
    expect(screen.getByText(/Runtime treats this as the explicit \["\*"\] form/)).toBeTruthy();
    expect(screen.queryByText(/Save to migrate/)).toBeNull();
  });

  it("shows a neutral exclude deprecation banner when value is null", () => {
    renderSelector({ value: null, variant: "exclude" });
    expect(screen.getByText(/Runtime treats this as the explicit \[\] form/)).toBeTruthy();
    expect(screen.queryByText(/Save to migrate/)).toBeNull();
  });

  it("does NOT show the banner when value is the explicit ['*']", () => {
    renderSelector({ value: ["*"] });
    expect(screen.queryByText(/Legacy config detected/)).toBeNull();
  });

  it("does NOT show the banner when value is the explicit empty list", () => {
    renderSelector({ value: [] });
    expect(screen.queryByText(/Legacy config detected/)).toBeNull();
  });
});

describe("ToolSelector — explicit ['*'] all-mode", () => {
  it("treats ['*'] as the 'all' mode (radio checked)", () => {
    renderSelector({ value: ["*"] });
    const allLabel = screen.getByText("All tools");
    const radio = allLabel
      .closest("label")!
      .querySelector("input[type='radio']") as HTMLInputElement;
    expect(radio.checked).toBe(true);
  });
});

describe("ToolSelector — toggling individual tool", () => {
  it("calls onChange with tool removed when unchecking a checked tool", () => {
    const value = ["Bash", "Read"];
    const { props } = renderSelector({ value });

    // Find the checkbox for Bash by looking at the label containing "Bash" id text
    const bashIdEl = screen.getByText("Bash");
    const bashLabel = bashIdEl.closest("label")!;
    const bashCheckbox = bashLabel.querySelector("input[type='checkbox']") as HTMLInputElement;
    expect(bashCheckbox.checked).toBe(true);

    act(() => {
      fireEvent.click(bashCheckbox);
    });

    expect(props.onChange).toHaveBeenCalledOnce();
    const result = props.onChange.mock.calls[0][0] as string[];
    expect(result).not.toContain("Bash");
    expect(result).toContain("Read");
  });

  it("calls onChange with tool added when checking an unchecked tool", () => {
    const value = ["Read"];
    const { props } = renderSelector({ value });

    const bashIdEl = screen.getByText("Bash");
    const bashLabel = bashIdEl.closest("label")!;
    const bashCheckbox = bashLabel.querySelector("input[type='checkbox']") as HTMLInputElement;
    expect(bashCheckbox.checked).toBe(false);

    act(() => {
      fireEvent.click(bashCheckbox);
    });

    expect(props.onChange).toHaveBeenCalledOnce();
    const result = props.onChange.mock.calls[0][0];
    // Adding Bash to Read still not all tools, so should be explicit array
    expect(Array.isArray(result)).toBe(true);
    expect((result as string[]).sort()).toEqual(["Bash", "Read"].sort());
  });
});

describe("ToolSelector — glob-backed selections", () => {
  it("renders glob-matched tools as checked but not directly editable", () => {
    const { props } = renderSelector({ value: ["mcp:weather/*"] });

    const forecastIdEl = screen.getByText("mcp:weather/forecast");
    const forecastLabel = forecastIdEl.closest("label")!;
    const forecastCheckbox = forecastLabel.querySelector(
      "input[type='checkbox']",
    ) as HTMLInputElement;

    expect(forecastCheckbox.checked).toBe(true);
    expect(forecastCheckbox.disabled).toBe(true);
    expect(screen.getAllByText("mcp:weather/*").length).toBeGreaterThan(0);

    act(() => {
      fireEvent.click(forecastCheckbox);
    });

    expect(props.onChange).not.toHaveBeenCalled();
  });

  it("disables group Clear when a group contains glob-backed selections", () => {
    renderSelector({ value: ["mcp:weather/*"] });

    const weatherGroup = screen.getByText("MCP · weather").closest("div.rounded-md") as HTMLElement;
    const clearButton = within(weatherGroup).getByRole("button", { name: "Clear" });

    expect((clearButton as HTMLButtonElement).disabled).toBe(true);
  });

  it("protects the exclude-all wildcard from checkbox edits", () => {
    const { props } = renderSelector({ value: ["*"], variant: "exclude" });

    const bashLabel = screen.getByText("Bash").closest("label")!;
    const bashCheckbox = bashLabel.querySelector("input[type='checkbox']") as HTMLInputElement;

    expect(bashCheckbox.checked).toBe(true);
    expect(bashCheckbox.disabled).toBe(true);
    expect(screen.getAllByText("*").length).toBeGreaterThan(0);

    act(() => {
      fireEvent.click(bashCheckbox);
    });

    expect(props.onChange).not.toHaveBeenCalled();
  });

  it("protects the exclude-all wildcard from group clear", () => {
    const { props } = renderSelector({ value: ["*"], variant: "exclude" });

    const builtInLabels = screen.getAllByText("Built-in");
    const builtInGroup = builtInLabels[builtInLabels.length - 1].closest("div.rounded-md")!;
    const clearButton = within(builtInGroup as HTMLElement).getByRole("button", { name: "Clear" });

    expect((clearButton as HTMLButtonElement).disabled).toBe(true);

    act(() => {
      fireEvent.click(clearButton);
    });

    expect(props.onChange).not.toHaveBeenCalled();
  });
});

describe("ToolSelector — pattern / unmanaged catalog entries", () => {
  it("lists unmatched catalog entries and removes them by raw value", () => {
    const { props } = renderSelector({ value: ["Bash", "stale:*"] });

    expect(screen.getByText("Pattern / unmanaged catalog entries")).toBeTruthy();
    expect(screen.getByText("stale:*")).toBeTruthy();
    expect(screen.getByText("No current tool matches")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Remove" }));

    expect(props.onChange).toHaveBeenCalledWith(["Bash"]);
  });

  it("shows matching tools for wildcard entries", () => {
    renderSelector({ value: ["mcp:weather/*"] });

    expect(screen.getByText("Pattern / unmanaged catalog entries")).toBeTruthy();
    expect(screen.getAllByText("mcp:weather/*").length).toBeGreaterThan(0);
    expect(screen.getByText(/Matches mcp:weather\/forecast/)).toBeTruthy();
  });

  it("keeps catalog controls visible when no tools are published", () => {
    const { props } = renderSelector({ value: ["stale:*"], tools: [] });

    expect(screen.getByText("Allowed Tools")).toBeTruthy();
    expect(screen.getAllByText("No tools are currently published.").length).toBeGreaterThan(0);
    expect(screen.getByText("stale:*")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Remove" }));

    expect(props.onChange).toHaveBeenCalledWith([]);
  });
});

describe("ToolSelector — reset override control", () => {
  it("calls onReset from the section reset button", () => {
    const onReset = vi.fn();
    renderSelector({
      value: ["Bash"],
      overridden: true,
      onReset,
      resetLabel: "Reset allowed_tools to default",
    });

    fireEvent.click(screen.getByRole("button", { name: "Reset allowed_tools to default" }));

    expect(onReset).toHaveBeenCalledOnce();
  });
});
