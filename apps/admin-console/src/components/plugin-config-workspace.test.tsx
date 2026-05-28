// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import {
  PluginConfigWorkspace,
  type PluginConfigEntry,
} from "./plugin-config-workspace";
import { pluginConfigEntryKey } from "@/lib/plugin-config";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function entry(
  pluginId: string,
  schemaKey: string,
  selected = true,
  hasStoredConfig = false,
  editor = schemaKey,
): PluginConfigEntry {
  const schema = {
    key: schemaKey,
    display_name: `${pluginId} display`,
    description: `${schemaKey} metadata description`,
    editor,
    schema: {
      type: "object",
      title: `${pluginId} schema`,
      description: `${schemaKey} configuration`,
      properties: {
        enabled: { type: "boolean", title: "Enabled" },
      },
    },
  };
  return {
    plugin: {
      id: pluginId,
      config_schemas: [schema],
    },
    schema,
    selected,
    hasStoredConfig,
  };
}

function renderWorkspace(
  overrides: Partial<Parameters<typeof PluginConfigWorkspace>[0]> = {},
) {
  const onSelectEntry = vi.fn();
  const onUpdateSection = vi.fn();
  const props = {
    entries: [] as PluginConfigEntry[],
    sections: {},
    activeEntryKey: null,
    onSelectEntry,
    onUpdateSection,
    ...overrides,
  };
  return {
    ...render(<PluginConfigWorkspace {...props} />),
    onSelectEntry,
    onUpdateSection,
  };
}

describe("PluginConfigWorkspace", () => {
  it("shows an empty state when no configurable plugin is selected", () => {
    renderWorkspace();

    expect(
      screen.getByText(/Select a configurable plugin to edit/),
    ).toBeTruthy();
  });

  it("selects entries and warns when stored config belongs to a disabled plugin", () => {
    const reminder = entry("reminder", "reminder", false, true);
    const activeEntryKey = pluginConfigEntryKey("reminder", "reminder");
    const { onSelectEntry } = renderWorkspace({
      entries: [reminder],
      activeEntryKey,
      sections: { reminder: { rules: [] } },
    });

    fireEvent.click(screen.getByRole("button", { name: /reminder display/ }));

    expect(onSelectEntry).toHaveBeenCalledWith(activeEntryKey);
    expect(screen.getByText("stored only")).toBeTruthy();
    expect(screen.getByText("plugin disabled")).toBeTruthy();
    expect(screen.getByText(/only takes effect after re-enabling/)).toBeTruthy();
  });

  it("surfaces consistency warnings from the shared plugin mechanism", () => {
    const reminder = {
      ...entry("reminder", "reminder", true, true),
      hookFilteredOut: true,
      warnings: ["Plugin `reminder` is enabled but excluded by active_hook_filter."],
    };

    renderWorkspace({
      entries: [reminder],
      activeEntryKey: pluginConfigEntryKey("reminder", "reminder"),
      sections: { reminder: { rules: [] } },
    });

    expect(screen.getByText("hook filtered")).toBeTruthy();
    expect(
      screen.getByText(/enabled but excluded by active_hook_filter/),
    ).toBeTruthy();
  });

  it("edits permission config before serializing the section update", () => {
    const activeEntryKey = pluginConfigEntryKey("permission", "permission");
    const { onUpdateSection } = renderWorkspace({
      entries: [entry("permission", "permission")],
      activeEntryKey,
      sections: {
        permission: {
          default_behavior: "ask",
          rules: [{ tool: "", behavior: "ask", scope: "project" }],
        },
      },
    });

    fireEvent.change(screen.getByLabelText("Tool pattern"), {
      target: { value: "Bash(npm *)" },
    });
    expect(onUpdateSection).toHaveBeenLastCalledWith(
      "permission",
      expect.objectContaining({
        rules: [expect.objectContaining({ tool: "Bash(npm *)" })],
      }),
    );

    fireEvent.click(screen.getAllByText("Deny").at(-1)!.closest("button")!);
    expect(onUpdateSection).toHaveBeenLastCalledWith(
      "permission",
      expect.objectContaining({
        rules: [expect.objectContaining({ behavior: "deny" })],
      }),
    );
  });

  it("edits reminder field rules before serializing the section update", () => {
    const activeEntryKey = pluginConfigEntryKey("reminder", "reminder");
    const { onUpdateSection } = renderWorkspace({
      entries: [entry("reminder", "reminder")],
      activeEntryKey,
      sections: {
        reminder: {
          rules: [
            {
              name: "weather fields",
              tool: "get_weather",
              output: {
                content: {
                  fields: [{ path: "", op: "exact", value: "" }],
                },
              },
              message: {
                target: "system",
                content: "Bring an umbrella.",
                cooldown_turns: 0,
              },
            },
          ],
        },
      },
    });

    fireEvent.change(screen.getByLabelText("Path"), {
      target: { value: "error.code" },
    });
    expect(onUpdateSection).toHaveBeenLastCalledWith(
      "reminder",
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            output: expect.objectContaining({
              content: expect.objectContaining({
                fields: [expect.objectContaining({ path: "error.code" })],
              }),
            }),
          }),
        ],
      }),
    );

    fireEvent.change(screen.getByLabelText("Value"), {
      target: { value: "403" },
    });
    expect(onUpdateSection).toHaveBeenLastCalledWith(
      "reminder",
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            output: expect.objectContaining({
              content: expect.objectContaining({
                fields: [
                  expect.objectContaining({
                    value: "403",
                  }),
                ],
              }),
            }),
          }),
        ],
      }),
    );
  });

  it("edits generative UI config with the specialized editor", () => {
    const activeEntryKey = pluginConfigEntryKey(
      "generative-ui",
      "generative-ui",
    );
    const { onUpdateSection } = renderWorkspace({
      entries: [entry("generative-ui", "generative-ui")],
      activeEntryKey,
      sections: { "generative-ui": {} },
    });

    fireEvent.change(screen.getByLabelText("Catalog ID"), {
      target: { value: "catalog://default" },
    });

    expect(onUpdateSection).toHaveBeenLastCalledWith("generative-ui", {
      catalog_id: "catalog://default",
    });
  });
});
