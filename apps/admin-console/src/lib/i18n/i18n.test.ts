// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { en } from "./en";
import { zhCN } from "./zh-CN";

describe("i18n dictionaries", () => {
  it("zh-CN has the same top-level keys as en", () => {
    expect(Object.keys(zhCN).sort()).toEqual(Object.keys(en).sort());
  });

  it("zh-CN nav.items has the same shape as en", () => {
    expect(Object.keys(zhCN.nav.items).sort()).toEqual(
      Object.keys(en.nav.items).sort(),
    );
  });

  it("zh-CN dashboard.system has the same shape as en", () => {
    expect(Object.keys(zhCN.dashboard.system).sort()).toEqual(
      Object.keys(en.dashboard.system).sort(),
    );
  });

  it("zh-CN trace.scorers.types covers heuristic/judge/code/human", () => {
    expect(Object.keys(zhCN.trace.scorers.types).sort()).toEqual(
      ["code", "heuristic", "human", "judge"],
    );
  });

  it("zh-CN values are non-empty strings or nested objects", () => {
    function walk(node: unknown, path: string[]): void {
      if (node === null || node === undefined) {
        throw new Error(`null/undefined at ${path.join(".")}`);
      }
      if (typeof node === "string") {
        expect(node.length).toBeGreaterThan(0);
        return;
      }
      if (typeof node === "object") {
        for (const [k, v] of Object.entries(node as Record<string, unknown>)) {
          walk(v, [...path, k]);
        }
        return;
      }
      throw new Error(`unexpected type ${typeof node} at ${path.join(".")}`);
    }
    walk(zhCN, []);
  });

  it("zh-CN preserves named interpolation placeholders from en", () => {
    // Spot-check a few keys with {{var}} placeholders.
    const samples: Array<[string, string]> = [
      [en.dashboard.health.meta, zhCN.dashboard.health.meta],
      [en.dashboard.plugins.meta, zhCN.dashboard.plugins.meta],
      [en.editor.editTitle, zhCN.editor.editTitle],
    ];
    for (const [enStr, zhStr] of samples) {
      const enVars = (enStr.match(/\{\{(\w+)\}\}/g) ?? []).sort();
      const zhVars = (zhStr.match(/\{\{(\w+)\}\}/g) ?? []).sort();
      expect(zhVars).toEqual(enVars);
    }
  });
});
