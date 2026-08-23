import { describe, expect, it } from "vitest";
import { defaultWebSearchConfig, webSearchProviderOptions } from "./WebSearchBehaviorEditor";

describe("WebSearch capability projection", () => {
  /** Cause/effect table: free and paid schema branches become provider choices;
   * only a branch requiring `credential` asks for a Vault pin; the first
   * server-ordered branch becomes the non-secret default; missing branches
   * produce no invented provider. */
  it("derives provider and credential decisions from schema only", () => {
    const schema = {
      oneOf: [
        { title: "Free", properties: { provider_id: { const: "free" }, options: { type: "object" } }, required: ["provider_id"] },
        { title: "Paid", properties: { provider_id: { const: "paid" }, credential: { type: "object" }, options: { type: "object" } }, required: ["provider_id", "credential"] },
      ],
    };
    expect(webSearchProviderOptions(schema)).toEqual([
      { id: "free", label: "Free", requiresCredential: false, optionsSchema: { type: "object" }, realization: "host_executed" },
      { id: "paid", label: "Paid", requiresCredential: true, optionsSchema: { type: "object" }, realization: "host_executed" },
    ]);
    expect(defaultWebSearchConfig(schema)).toEqual({ provider_id: "free", options: {} });
    expect(webSearchProviderOptions({})).toEqual([]);
  });
});
