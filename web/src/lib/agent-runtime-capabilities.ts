export type RuntimeSupport = "supported" | "conditional" | "unavailable";

export type AcpWorkingDirectoryIssue =
  | "absolute"
  | "backslash"
  | "traversal"
  | "empty_segment"
  | "colon"
  | "too_long";

export function acpWorkingDirectoryIssue(value: string): AcpWorkingDirectoryIssue | null {
  if (!value) return null;
  if (value.length > 512) return "too_long";
  if (value.startsWith("/") || /^[A-Za-z]:/.test(value)) return "absolute";
  if (value.includes("\\")) return "backslash";
  if (value.includes(":")) return "colon";
  const parts = value.split("/");
  if (parts.some((part) => part === "." || part === "..")) return "traversal";
  if (parts.some((part) => part.length === 0)) return "empty_segment";
  return null;
}
export type RuntimeCapabilityKey =
  | "environment_session"
  | "context"
  | "tools_mcp"
  | "state_concurrency"
  | "background_tools";

/** Product capability matrix for the two execution roots. Harness-specific
 * settings remain live capability data; this matrix contains only invariants
 * enforced by Awaken's execution architecture. */
export function runtimeCapabilitySupport(
  acp: boolean,
  features?: {
    state_machine: RuntimeSupport;
    background_tools: RuntimeSupport;
    awaken_tool_bridge: RuntimeSupport;
  },
): Record<RuntimeCapabilityKey, RuntimeSupport> {
  return {
    environment_session: "supported",
    context: "supported",
    tools_mcp: acp ? (features?.awaken_tool_bridge ?? "conditional") : "supported",
    state_concurrency: acp ? (features?.state_machine ?? "unavailable") : "supported",
    background_tools: acp ? (features?.background_tools ?? "unavailable") : "supported",
  };
}
