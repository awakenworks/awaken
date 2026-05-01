/// Reasoning effort presets understood by all major providers, plus a
/// "Custom" option that surfaces a free-form input for numeric levels.
export const REASONING_EFFORT_PRESETS = ["low", "medium", "high"] as const;
export type ReasoningEffortPreset = (typeof REASONING_EFFORT_PRESETS)[number];

export type ReasoningEffortMode =
  | { kind: "default" }
  | { kind: "preset"; value: ReasoningEffortPreset }
  | { kind: "custom"; value: string };

/// Inspect a stored reasoning_effort value (string | number | null) and
/// classify it into a UI mode. Numbers become a custom string so they
/// round-trip through the editor; unknown strings are treated as custom.
export function reasoningEffortMode(
  value: string | number | null | undefined,
): ReasoningEffortMode {
  if (value === undefined || value === null || value === "") {
    return { kind: "default" };
  }
  if (typeof value === "number") {
    return { kind: "custom", value: String(value) };
  }
  if ((REASONING_EFFORT_PRESETS as readonly string[]).includes(value)) {
    return { kind: "preset", value: value as ReasoningEffortPreset };
  }
  return { kind: "custom", value };
}

/// Convert a UI mode back to the wire-shape stored on AgentSpec.
export function reasoningEffortValue(
  mode: ReasoningEffortMode,
): string | number | null | undefined {
  if (mode.kind === "default") return undefined;
  if (mode.kind === "preset") return mode.value;
  const trimmed = mode.value.trim();
  if (trimmed.length === 0) return undefined;
  const asNumber = Number(trimmed);
  if (!Number.isNaN(asNumber) && /^-?\d+(\.\d+)?$/.test(trimmed)) {
    return asNumber;
  }
  return trimmed;
}
