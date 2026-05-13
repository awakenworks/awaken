import { describe, expect, it } from "vitest";
import {
  reasoningEffortMode,
  reasoningEffortValue,
} from "./reasoning-effort";

describe("reasoningEffortMode", () => {
  it("treats undefined / null / empty string as the runtime default", () => {
    expect(reasoningEffortMode(undefined)).toEqual({ kind: "default" });
    expect(reasoningEffortMode(null)).toEqual({ kind: "default" });
    expect(reasoningEffortMode("")).toEqual({ kind: "default" });
  });

  it("recognises the three named presets", () => {
    expect(reasoningEffortMode("low")).toEqual({
      kind: "preset",
      value: "low",
    });
    expect(reasoningEffortMode("medium")).toEqual({
      kind: "preset",
      value: "medium",
    });
    expect(reasoningEffortMode("high")).toEqual({
      kind: "preset",
      value: "high",
    });
  });

  it("returns numeric levels as custom strings so they round-trip in the input", () => {
    expect(reasoningEffortMode(42)).toEqual({ kind: "custom", value: "42" });
  });

  it("falls back to custom for unrecognised strings", () => {
    expect(reasoningEffortMode("ultra")).toEqual({
      kind: "custom",
      value: "ultra",
    });
  });
});

describe("reasoningEffortValue", () => {
  it("returns null for default so customized agents PATCH an explicit null override", () => {
    // Returning null (not undefined) is load-bearing for customized
    // agents: the patch-diff walks fields and routes undefined into a
    // DELETE override (which falls back to the base record's value).
    // The "Provider default" UI choice must override the base with an
    // explicit null instead.
    expect(reasoningEffortValue({ kind: "default" })).toBeNull();
  });

  it("returns the preset string verbatim", () => {
    expect(reasoningEffortValue({ kind: "preset", value: "high" })).toBe("high");
  });

  it("parses purely numeric custom values back into numbers", () => {
    expect(reasoningEffortValue({ kind: "custom", value: " 17 " })).toBe(17);
    expect(reasoningEffortValue({ kind: "custom", value: "1.5" })).toBe(1.5);
  });

  it("preserves non-numeric custom values as strings", () => {
    expect(reasoningEffortValue({ kind: "custom", value: "ultra" })).toBe(
      "ultra",
    );
  });

  it("treats a blank custom value the same as default (null = provider default override)", () => {
    expect(reasoningEffortValue({ kind: "custom", value: "  " })).toBeNull();
  });
});
