export type ToolPatternKind = "tool-id" | "tool-call";

export const TOOL_ID_PATTERN_PLACEHOLDER = "mcp__github__*";
export const TOOL_CALL_PATTERN_PLACEHOLDER = "Bash(npm *)";
export const EXACT_TOOL_ID_PLACEHOLDER = "get_weather";

export const TOOL_ID_PATTERN_EXAMPLES = ["get_weather", "mcp__github__*", "search*", "*"];
export const TOOL_CALL_PATTERN_EXAMPLES = [
  "Bash(npm *)",
  'Edit(file_path ~ "src/**")',
  "mcp__github__*",
];

export function toolPatternLabel(kind: ToolPatternKind): string {
  return kind === "tool-id" ? "Tool ID pattern" : "Tool call pattern";
}

export function toolPatternDescription(kind: ToolPatternKind): string {
  if (kind === "tool-id") {
    return "Matches tool ids only. Use exact ids or anchored wildcards; MCP tools use mcp__server__tool ids.";
  }
  return "Matches a tool call. Use exact ids, wildcards, regex tool names, or argument matchers when the plugin supports call-level triggers.";
}

export function toolPatternExamples(kind: ToolPatternKind): readonly string[] {
  return kind === "tool-id" ? TOOL_ID_PATTERN_EXAMPLES : TOOL_CALL_PATTERN_EXAMPLES;
}

export function toolPatternPlaceholder(kind: ToolPatternKind): string {
  return kind === "tool-id" ? TOOL_ID_PATTERN_PLACEHOLDER : TOOL_CALL_PATTERN_PLACEHOLDER;
}

export function normalizeToolPatternInput(value: string): string {
  return value.trim();
}

export function validateToolPatternInput(
  value: string,
  options: { kind: ToolPatternKind; allowEmpty?: boolean } = { kind: "tool-id" },
): string | null {
  const trimmed = normalizeToolPatternInput(value);
  if (!trimmed) {
    return options.allowEmpty === false ? "Enter a tool pattern." : null;
  }
  if (hasDanglingEscape(trimmed)) {
    return "Pattern ends with a dangling escape. Use \\\\ for a literal backslash.";
  }
  if (options.kind === "tool-id" && trimmed.includes("(")) {
    return "Tool ID patterns match ids only. Use a call-pattern field for argument matchers.";
  }
  return null;
}

function hasDanglingEscape(value: string): boolean {
  let slashCount = 0;
  for (let index = value.length - 1; index >= 0; index -= 1) {
    if (value[index] !== "\\") break;
    slashCount += 1;
  }
  return slashCount % 2 === 1;
}
