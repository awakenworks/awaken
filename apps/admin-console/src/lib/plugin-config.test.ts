import { describe, expect, it } from "vitest";
import {
  createReminderRule,
  normalizeReminderConfig,
  normalizePermissionConfig,
  pluginConfigDisplaySummary,
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
});
