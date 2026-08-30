export type RuntimeSupport = "supported" | "conditional" | "unavailable";
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
): Record<RuntimeCapabilityKey, RuntimeSupport> {
  return {
    environment_session: "supported",
    context: "supported",
    tools_mcp: acp ? "conditional" : "supported",
    state_concurrency: acp ? "conditional" : "supported",
    background_tools: acp ? "unavailable" : "supported",
  };
}
