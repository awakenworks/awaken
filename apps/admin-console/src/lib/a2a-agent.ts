import type { AgentSpec } from "@/lib/config-api";

export const A2A_SERVER_ID_OPTION = "a2a_server_id";

export function a2aServerIdForAgent(spec: AgentSpec): string | null {
  if (spec.endpoint?.backend !== "a2a") return null;
  const option = spec.endpoint?.options?.[A2A_SERVER_ID_OPTION];
  if (typeof option === "string" && option.trim()) return option;
  return null;
}

export function isDiscoveredA2aAgent(spec: AgentSpec): boolean {
  return spec.endpoint?.backend === "a2a" && a2aServerIdForAgent(spec) !== null;
}
