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
  it("erases the field when the mode is default", () => {
    expect(reasoningEffortValue({ kind: "default" })).toBeUndefined();
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

  it("treats a blank custom value as the default", () => {
    expect(reasoningEffortValue({ kind: "custom", value: "  " })).toBeUndefined();
  });
});
