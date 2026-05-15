import { describe, expect, it } from "vitest";
import {
  createReminderField,
  createReminderRule,
  moveItem,
  normalizeGenerativeUiConfig,
  normalizeReminderConfig,
  normalizePermissionConfig,
  pluginConfigEntryKey,
  pluginConfigDisplaySummary,
  pluginDisplayName,
  schemaDescription,
  schemaTitle,
  serializeGenerativeUiConfig,
  serializePermissionConfig,
  serializeReminderConfig,
} from "./plugin-config";

describe("plugin config helpers", () => {
  it("summarizes permission config with rule count", () => {
    expect(
      pluginConfigDisplaySummary("permission", "permission", {
        default_behavior: "deny",
        rules: [{ tool: "Bash", behavior: "allow", scope: "project" }],
      }),
    ).toBe("Default deny · 1 rule");

    expect(normalizePermissionConfig({}).default_behavior).toBe("ask");
  });

  it("normalizes reminder text and field output modes", () => {
    const textConfig = normalizeReminderConfig({
      rules: [
        {
          tool: "get_weather",
          output: { status: "success", content: "*rain*" },
          message: { target: "system", content: "Bring an umbrella." },
        },
        {
          tool: "Bash",
          output: {
            content: {
              fields: [{ path: "error.code", op: "exact", value: "403" }],
            },
          },
          message: { target: "suffix_system", content: "Permission denied." },
        },
      ],
    });

    expect(textConfig.rules[0].mode).toBe("status_and_text");
    expect(textConfig.rules[0].status).toBe("success");
    expect(textConfig.rules[0].text).toBe("*rain*");
    expect(textConfig.rules[1].mode).toBe("content_fields");
    expect(textConfig.rules[1].fields[0]).toMatchObject({
      path: "error.code",
      op: "exact",
      value: "403",
    });
  });

  it("serializes reminder drafts back into runtime config shape", () => {
    const rule = createReminderRule();
    rule.tool = "get_stock_price";
    rule.mode = "status_and_fields";
    rule.status = "error";
    rule.fields = [{ path: "error.code", op: "exact", value: "429" }];
    rule.target = "conversation";
    rule.content = "Rate limited.";

    expect(serializeReminderConfig({ rules: [rule] })).toEqual({
      rules: [
        {
          name: undefined,
          tool: "get_stock_price",
          output: {
            status: "error",
            content: {
              fields: [{ path: "error.code", op: "exact", value: "429" }],
            },
          },
          message: {
            target: "conversation",
            content: "Rate limited.",
            cooldown_turns: 0,
          },
        },
      ],
    });
  });

  it("names known plugins and builds stable config entry keys", () => {
    expect(pluginConfigEntryKey("permission", "rules")).toBe("permission:rules");
    expect(pluginDisplayName("permission")).toBe("Permissions");
    expect(pluginDisplayName("reminder")).toBe("Reminders");
    expect(pluginDisplayName("generative-ui")).toBe("Generative UI");
    expect(pluginDisplayName("ext-deferred-tools")).toBe("Deferred Tools");
    expect(pluginDisplayName("frontend_tools")).toBe("Frontend Tools");
    expect(pluginDisplayName("custom-plugin")).toBe("custom-plugin");
  });

  it("summarizes generic and generative-ui config states", () => {
    expect(pluginConfigDisplaySummary("reminder", "reminder", { rules: [] })).toBe(
      "No reminder rules",
    );
    expect(pluginConfigDisplaySummary("generative-ui", "generative-ui", {})).toBe(
      "Prompt defaults",
    );
    expect(
      pluginConfigDisplaySummary("generative-ui", "generative-ui", {
        instructions: "Use cards",
        catalog_id: "catalog-a",
        examples: "Example",
      }),
    ).toBe("instruction override · catalog override · examples");
    expect(pluginConfigDisplaySummary("custom", "settings", { enabled: true })).toBe(
      "Configured",
    );
    expect(pluginConfigDisplaySummary("custom", "settings", { empty: "" })).toBe(
      "Schema form",
    );
  });

  it("normalizes and serializes generative-ui config with trimmed optional fields", () => {
    expect(
      normalizeGenerativeUiConfig({
        catalog_id: " catalog-a ",
        examples: "Example",
        instructions: "Use cards",
      }),
    ).toEqual({
      catalog_id: " catalog-a ",
      examples: "Example",
      instructions: "Use cards",
    });
    expect(
      serializeGenerativeUiConfig({
        catalog_id: " catalog-a ",
        examples: "",
        instructions: "Use cards",
      }),
    ).toEqual({
      catalog_id: "catalog-a",
      instructions: "Use cards",
    });
  });

  it("falls back safely for invalid permission and reminder enum values", () => {
    expect(
      normalizePermissionConfig({
        default_behavior: "invalid",
        rules: [{ tool: 12, behavior: "invalid", scope: "invalid" }],
      }),
    ).toEqual({
      default_behavior: "ask",
      rules: [{ tool: "", behavior: "ask", scope: "project" }],
    });

    expect(
      normalizeReminderConfig({
        rules: [
          {
            name: 12,
            tool: 34,
            output: { status: "invalid", content: { fields: [{ op: "invalid" }] } },
            message: { target: "invalid", content: 56, cooldown_turns: Number.NaN },
          },
        ],
      }).rules[0],
    ).toMatchObject({
      name: "",
      tool: "",
      mode: "status_and_fields",
      status: "success",
      fields: [{ path: "", op: "glob", value: "" }],
      target: "system",
      content: "",
      cooldown_turns: 0,
    });
  });

  it("serializes permission and every reminder output mode", () => {
    expect(
      serializePermissionConfig({
        default_behavior: "allow",
        rules: [{ tool: "Bash(*)", behavior: "deny", scope: "thread" }],
      }),
    ).toEqual({
      default_behavior: "allow",
      rules: [{ tool: "Bash(*)", behavior: "deny", scope: "thread" }],
    });

    const base = createReminderRule();
    expect(serializeReminderConfig({ rules: [{ ...base, mode: "status", status: "pending" }] }))
      .toEqual({
        rules: [
          {
            name: undefined,
            tool: "",
            output: { status: "pending" },
            message: { target: "system", content: "", cooldown_turns: 0 },
          },
        ],
      });
    expect(serializeReminderConfig({ rules: [{ ...base, mode: "content_text", text: "*ok*" }] }))
      .toEqual({
        rules: [
          {
            name: undefined,
            tool: "",
            output: { content: "*ok*" },
            message: { target: "system", content: "", cooldown_turns: 0 },
          },
        ],
      });
    expect(
      serializeReminderConfig({
        rules: [
          {
            ...base,
            mode: "content_fields",
            fields: [{ path: "status", op: "not_exact", value: "ok" }],
          },
        ],
      }),
    ).toEqual({
      rules: [
        {
          name: undefined,
          tool: "",
          output: { content: { fields: [{ path: "status", op: "not_exact", value: "ok" }] } },
          message: { target: "system", content: "", cooldown_turns: 0 },
        },
      ],
    });
  });

  it("reads schema metadata and exposes small collection helpers", () => {
    expect(schemaTitle({ title: " Settings " })).toBe(" Settings ");
    expect(schemaTitle({ title: " " })).toBeNull();
    expect(schemaDescription({ description: "Helpful copy" })).toBe("Helpful copy");
    expect(schemaDescription({ description: "" })).toBeNull();
    expect(moveItem(["a", "b", "c"], 0, 2)).toEqual(["b", "c", "a"]);
    expect(createReminderField()).toEqual({ path: "", op: "glob", value: "" });
  });
});
